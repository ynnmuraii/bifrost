use crate::{
    mute::{LinuxMuteGuard, MuteWatches, RenderMuteWatches},
    ptt::PortalPtt,
    raw::{self, Config, Connection},
    stream::{LinuxStream, Signals, StreamTarget},
};
use audio_backend::*;
use pulseaudio::{protocol as p, Client};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::CStr,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, Weak,
    },
    thread::{self, JoinHandle},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limitation {
    ProcessExclusionUnavailable,
    /// Pulse cannot identify the writer: source-mute ownership is best-effort ownership.
    EndpointMuteBestEffortOwnership,
    /// Native release requires a Wayland GlobalShortcuts portal; X11 audio PTT is unsupported.
    GlobalPttWaylandPortalOnly,
}
const LIMITS: [Limitation; 3] = [
    Limitation::ProcessExclusionUnavailable,
    Limitation::EndpointMuteBestEffortOwnership,
    Limitation::GlobalPttWaylandPortalOnly,
];
#[derive(Clone)]
pub(crate) struct Entry {
    pub device: Device,
    pub generation: u64,
    pub(crate) index: u32,
    pub monitor: Option<String>,
    pub serial: Option<u64>,
    pub server_cookie: u32,
}
pub(crate) type Key = (bool, String);
#[derive(Default)]
struct Catalog {
    entries: BTreeMap<Key, Entry>,
    defaults: Defaults,
    next_generation: u64,
    events: VecDeque<Event>,
    overflow: bool,
    streams: Vec<(Key, u64, Weak<Signals>)>,
}
pub(crate) struct State {
    catalog: Mutex<Catalog>,
    pub alive: AtomicBool,
    pub clock: Instant,
    pub(crate) mutes: MuteWatches,
    pub(crate) render_mutes: RenderMuteWatches,
}
pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl State {
    pub fn new() -> Self {
        Self {
            catalog: Mutex::new(Catalog::default()),
            alive: AtomicBool::new(true),
            clock: Instant::now(),
            mutes: MuteWatches::default(),
            render_mutes: RenderMuteWatches::default(),
        }
    }
    fn emit(catalog: &mut Catalog, event: Event) {
        if catalog.events.len() == EVENT_CAPACITY {
            catalog.events.pop_front();
            catalog.overflow = true;
        }
        catalog.events.push_back(event);
    }
    pub fn register(
        &self,
        key: Key,
        generation: u64,
        signals: &Arc<Signals>,
    ) -> Result<(), BackendError> {
        let mut catalog = lock(&self.catalog);
        if !self.alive.load(Ordering::Acquire)
            || catalog
                .entries
                .get(&key)
                .is_none_or(|entry| entry.generation != generation)
        {
            return Err(BackendError::DeviceGone);
        }
        catalog
            .streams
            .retain(|(_, _, stream)| stream.strong_count() > 0);
        catalog
            .streams
            .push((key, generation, Arc::downgrade(signals)));
        Ok(())
    }
    pub fn invalidate_all(&self) {
        self.alive.store(false, Ordering::Release);
        self.mutes.invalidate_all();
        self.render_mutes.invalidate_all();
        let mut catalog = lock(&self.catalog);
        for (_, _, stream) in &catalog.streams {
            if let Some(signals) = stream.upgrade() {
                signals.invalidate();
            }
        }
        Self::emit(&mut catalog, Event::ResyncRequired);
    }
    fn removed(&self, event: p::SubscriptionEvent) {
        if event.event_type != p::SubscriptionEventType::Removed {
            return;
        }
        let capture = match event.event_facility {
            p::SubscriptionEventFacility::Sink => false,
            p::SubscriptionEventFacility::Source => true,
            _ => return,
        };
        let mut catalog = lock(&self.catalog);
        let key = catalog
            .entries
            .iter()
            .find(|(key, entry)| key.0 == capture && Some(entry.index) == event.index)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            if let Some(entry) = catalog.entries.remove(&key) {
                for (bound, _, signals) in &catalog.streams {
                    if *bound == key {
                        if let Some(signals) = signals.upgrade() {
                            signals.invalidate();
                        }
                    }
                }
                Self::emit(
                    &mut catalog,
                    Event::Device(DeviceEvent::Removed(entry.device.id.clone())),
                );
                Self::emit(
                    &mut catalog,
                    Event::Device(DeviceEvent::Invalidated(entry.device.id)),
                );
            }
        }
    }
    pub(crate) fn valid(&self, key: &Key, generation: u64) -> bool {
        self.alive.load(Ordering::Acquire)
            && lock(&self.catalog)
                .entries
                .get(key)
                .is_some_and(|entry| entry.generation == generation)
    }
    fn mute_changed(&self, index: u32, muted: bool) {
        let mut catalog = lock(&self.catalog);
        let snapshot = catalog
            .entries
            .iter()
            .find(|(key, entry)| key.0 && entry.index == index)
            .map(|(_, entry)| MuteSnapshot {
                endpoint: entry.device.id.clone(),
                generation: entry.generation,
                muted,
            });
        if let Some(snapshot) = snapshot {
            Self::emit(&mut catalog, Event::MuteChanged(snapshot));
        }
    }
    fn replace(&self, mut entries: BTreeMap<Key, Entry>, defaults: Defaults, notify: bool) {
        let mut catalog = lock(&self.catalog);
        for (key, entry) in &mut entries {
            if let Some(old) = catalog.entries.get(key).filter(|old| {
                old.index == entry.index
                    && old.device.format == entry.device.format
                    && old.serial == entry.serial
                    && old.server_cookie == entry.server_cookie
            }) {
                entry.generation = old.generation;
            } else {
                catalog.next_generation += 1;
                entry.generation = catalog.next_generation;
            }
        }
        if notify {
            let mut events = Vec::new();
            for (key, old) in &catalog.entries {
                match entries.get(key) {
                    None => {
                        events.push(Event::Device(DeviceEvent::Removed(old.device.id.clone())));
                        events.push(Event::Device(DeviceEvent::Invalidated(
                            old.device.id.clone(),
                        )));
                    }
                    Some(new) if new.generation != old.generation => {
                        events.push(Event::Device(DeviceEvent::FormatChanged(
                            old.device.id.clone(),
                        )));
                        events.push(Event::Device(DeviceEvent::Invalidated(
                            old.device.id.clone(),
                        )));
                    }
                    _ => {}
                }
            }
            for (key, new) in &entries {
                if !catalog.entries.contains_key(key) {
                    events.push(Event::Device(DeviceEvent::Added(new.device.id.clone())));
                }
            }
            for role in [DefaultRole::Multimedia, DefaultRole::Communications] {
                let old = catalog.defaults.for_role(role);
                let new = defaults.for_role(role);
                if old.render != new.render {
                    events.push(Event::Device(DeviceEvent::DefaultChanged {
                        direction: Direction::Render,
                        role,
                    }));
                }
                if old.capture != new.capture {
                    events.push(Event::Device(DeviceEvent::DefaultChanged {
                        direction: Direction::Capture,
                        role,
                    }));
                }
            }
            for event in events {
                Self::emit(&mut catalog, event);
            }
        }
        for (key, generation, signals) in &catalog.streams {
            if entries
                .get(key)
                .is_none_or(|entry| entry.generation != *generation)
            {
                if let Some(signals) = signals.upgrade() {
                    signals.invalidate();
                }
            }
        }
        catalog.entries = entries;
        catalog.defaults = defaults;
    }
}
/// Keep the backend alive while its streams are active. Construction is explicit opt-in;
/// it connects only to a local Unix Pulse server and never changes native defaults/routing.
pub struct LinuxBackend {
    pub(crate) config: Arc<Config>,
    connection: Connection,
    pub(crate) state: Arc<State>,
    watch_socket: UnixStream,
    watcher: Option<JoinHandle<()>>,
    media: crate::mpris::Mpris,
    ptt: PortalPtt,
    ptt_availability: Availability,
}
impl LinuxBackend {
    pub fn connect() -> Result<Self, BackendError> {
        let config = Arc::new(Config::from_env()?);
        let connection = config.client()?;
        let mut watcher = config.raw()?;
        watcher.subscribe()?;
        let watch_socket = watcher
            .socket
            .try_clone()
            .map_err(|_| BackendError::ServerUnavailable)?;
        let (entries, defaults) = enumerate(&connection.client)?;
        let state = Arc::new(State::new());
        state.replace(entries, defaults, false);
        let watch_state = state.clone();
        let watch_client = connection.client.clone();
        let handle = thread::Builder::new()
            .name("bfa-pulse-watch".into())
            .spawn(move || {
                while let Ok(event) = watcher.event() {
                    if !watch_state.alive.load(Ordering::Acquire) {
                        return;
                    }
                    // Queries run on this control thread, never under the catalog/reactor lock.
                    let mute = watch_state.mutes.observe(event, &watch_client);
                    watch_state.render_mutes.observe(event, &watch_client);
                    watch_state.removed(event);
                    match enumerate(&watch_client) {
                        Ok((entries, defaults)) => watch_state.replace(entries, defaults, true),
                        Err(_) => break,
                    }
                    if let Some((index, muted)) = mute {
                        watch_state.mute_changed(index, muted);
                    }
                }
                if watch_state.alive.load(Ordering::Acquire) {
                    watch_state.invalidate_all();
                }
            })
            .map_err(|_| BackendError::ServerUnavailable)?;
        Ok(Self {
            config,
            connection,
            state,
            watch_socket,
            watcher: Some(handle),
            media: crate::mpris::Mpris::start(),
            ptt: PortalPtt::new(),
            ptt_availability: Availability::Unavailable(BackendError::Unsupported),
        })
    }
    pub fn limitations(&self) -> &'static [Limitation] {
        &LIMITS
    }
    pub fn monitor_availability(&self) -> Availability {
        if self.state.alive.load(Ordering::Acquire) {
            Availability::Available
        } else {
            Availability::Unavailable(BackendError::ServerUnavailable)
        }
    }
    fn resolve(
        &self,
        selection: &DeviceSelection,
        capture: bool,
    ) -> Result<(Key, Entry), BackendError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(BackendError::ServerUnavailable);
        }
        let catalog = lock(&self.state.catalog);
        let id = match selection {
            DeviceSelection::Explicit(id) => id,
            DeviceSelection::FollowDefault(role) => {
                let defaults = catalog.defaults.for_role(*role);
                if capture {
                    defaults.capture.as_ref()
                } else {
                    defaults.render.as_ref()
                }
                .ok_or(BackendError::DeviceGone)?
            }
        };
        let key = (capture, id.as_str().to_owned());
        let entry = catalog
            .entries
            .get(&key)
            .ok_or(BackendError::DeviceGone)?
            .clone();
        Ok((key, entry))
    }
}
impl Drop for LinuxBackend {
    fn drop(&mut self) {
        self.media.shutdown();
        self.state.invalidate_all();
        let _ = self.watch_socket.shutdown(Shutdown::Both);
        self.connection.close();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

fn id(name: &CStr) -> Result<DeviceId, BackendError> {
    DeviceId::new(name.to_str().map_err(|_| BackendError::InvalidArgument)?)
}
fn label(description: Option<&CStr>, name: &CStr) -> String {
    let text = description.unwrap_or(name).to_string_lossy();
    let mut end = text.len().min(MAX_DEVICE_LABEL_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
fn enumerate(client: &Client) -> Result<(BTreeMap<Key, Entry>, Defaults), BackendError> {
    let (sinks, sources, server) = raw::wait(async {
        futures::try_join!(
            client.list_sinks(),
            client.list_sources(),
            client.server_info()
        )
    })?;
    let mut entries = BTreeMap::new();
    for sink in sinks {
        let device = Device {
            id: id(&sink.name)?,
            label: label(sink.description.as_deref(), &sink.name),
            direction: Direction::Render,
            format: AudioFormat {
                sample_rate: sink.sample_spec.sample_rate,
                channels: u16::from(sink.sample_spec.channels),
            },
        };
        entries.insert(
            (false, device.id.as_str().to_owned()),
            Entry {
                device,
                index: sink.index,
                generation: 0,
                serial: raw::object_serial(&sink.props),
                server_cookie: server.cookie,
                monitor: sink
                    .monitor_source_name
                    .as_deref()
                    .map(|name| name.to_string_lossy().into_owned()),
            },
        );
    }
    for source in sources {
        let device = Device {
            id: id(&source.name)?,
            label: label(source.description.as_deref(), &source.name),
            direction: Direction::Capture,
            format: AudioFormat {
                sample_rate: source.sample_spec.sample_rate,
                channels: u16::from(source.sample_spec.channels),
            },
        };
        entries.insert(
            (true, device.id.as_str().to_owned()),
            Entry {
                device,
                index: source.index,
                generation: 0,
                serial: raw::object_serial(&source.props),
                server_cookie: server.cookie,
                monitor: None,
            },
        );
    }
    let defaults = DefaultEndpoints {
        render: server.default_sink_name.as_deref().map(id).transpose()?,
        capture: server.default_source_name.as_deref().map(id).transpose()?,
    };
    Ok((
        entries,
        Defaults {
            multimedia: defaults.clone(),
            communications: defaults,
        },
    ))
}
impl Backend for LinuxBackend {
    type Stream = LinuxStream;
    type Mute = LinuxMuteGuard;
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            system_capture: true,
            endpoint_mute: self.state.alive.load(Ordering::Acquire),
            media: self.media.availability() == Availability::Available,
            global_ptt: self.ptt_availability == Availability::Available,
            ..Capabilities::default()
        }
    }
    fn probe_capabilities(&mut self) -> CapabilityProbe {
        self.ptt_availability = match PortalPtt::probe() {
            Ok(_) => Availability::Available,
            Err(error) => Availability::Unavailable(error),
        };
        CapabilityProbe {
            os_build: None,
            process_exclude: Availability::Unavailable(BackendError::Unsupported),
            endpoint_mute: self.monitor_availability(),
            media: self.media.availability(),
            ptt: self.ptt_availability,
            ptt_release: (self.ptt_availability == Availability::Available)
                .then_some(ReleaseObservation::Native),
        }
    }
    fn devices(&self) -> Result<Vec<Device>, BackendError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(BackendError::ServerUnavailable);
        }
        Ok(lock(&self.state.catalog)
            .entries
            .values()
            .map(|entry| entry.device.clone())
            .collect())
    }
    fn defaults(&self) -> Result<Defaults, BackendError> {
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(BackendError::ServerUnavailable);
        }
        Ok(lock(&self.state.catalog).defaults.clone())
    }
    fn start_capture(
        &mut self,
        request: CaptureSpec,
        callback: CaptureCallback,
    ) -> Result<LinuxStream, BackendError> {
        let mut monitor = None;
        let (key, entry, source, format) = match request {
            CaptureSpec::Monitor { sink, format } => {
                let (sink_key, sink) = self.resolve(&sink, false)?;
                let source = sink.monitor.clone().ok_or(BackendError::Unsupported)?;
                let (key, entry) =
                    self.resolve(&DeviceSelection::Explicit(DeviceId::new(&source)?), true)?;
                monitor = Some((sink_key, sink));
                (key, entry, source, format)
            }
            CaptureSpec::Microphone { endpoint, format } => {
                let (key, entry) = self.resolve(&endpoint, true)?;
                let source = entry.device.id.as_str().to_owned();
                (key, entry, source, format)
            }
            _ => return Err(BackendError::Unsupported),
        };
        LinuxStream::capture(
            self.config.clone(),
            StreamTarget {
                state: self.state.clone(),
                key,
                entry,
            },
            monitor,
            &source,
            format,
            callback,
        )
    }
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<LinuxStream, BackendError> {
        let (key, entry) = self.resolve(&request.device, false)?;
        LinuxStream::render(
            self.config.clone(),
            self.state.clone(),
            key,
            entry,
            request.requested,
            callback,
        )
    }
    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError> {
        let (_, entry) = self.resolve(selection, true)?;
        let info = raw::wait(
            self.connection
                .client
                .source_info_by_name(raw::name(entry.device.id.as_str())?),
        )?;
        if info.index != entry.index {
            return Err(BackendError::DeviceGone);
        }
        Ok(MuteSnapshot {
            endpoint: entry.device.id,
            generation: entry.generation,
            muted: info.muted,
        })
    }
    fn acquire_mute(
        &mut self,
        selection: &DeviceSelection,
    ) -> Result<LinuxMuteGuard, BackendError> {
        let (key, entry) = self.resolve(selection, true)?;
        LinuxMuteGuard::acquire(self.config.clone(), self.state.clone(), key, entry)
    }
    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        self.media.sessions()
    }
    fn current_media_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.media.current()
    }
    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        self.media.command(request)
    }
    fn cancel_media_commands(&mut self) {
        self.media.cancel();
    }
    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        if hotkey.is_some() && self.ptt_availability != Availability::Available {
            self.ptt_availability = match PortalPtt::probe() {
                Ok(_) => Availability::Available,
                Err(error) => Availability::Unavailable(error),
            };
            if let Availability::Unavailable(error) = self.ptt_availability {
                return Err(error);
            }
        }
        self.ptt.configure(hotkey, reserved)
    }
    fn poll_event(&mut self) -> Option<Event> {
        if let Some(event) = self.ptt.poll_event() {
            if matches!(event, Event::ResyncRequired) {
                self.ptt_availability = Availability::Unavailable(BackendError::Unsupported);
            }
            return Some(event);
        }
        if let Some(event) = self.media.poll_completion() {
            return Some(event);
        }
        let mut catalog = lock(&self.state.catalog);
        if catalog.overflow {
            catalog.overflow = false;
            Some(Event::ResyncRequired)
        } else {
            catalog
                .events
                .pop_front()
                .or_else(|| self.media.poll_changed())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn entry(index: u32) -> Entry {
        Entry {
            device: Device {
                id: DeviceId::new("TEST_SINK").unwrap(),
                label: "Test sink".into(),
                direction: Direction::Render,
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                },
            },
            generation: 0,
            index,
            monitor: Some("TEST_SINK.monitor".into()),
            serial: Some(u64::from(index) + 1),
            server_cookie: 5,
        }
    }
    #[test]
    fn index_reuse_changes_generation_and_queue_overflow_requires_resync() {
        let state = State::new();
        let key = (false, "TEST_SINK".into());
        state.replace(
            BTreeMap::from([(key.clone(), entry(1))]),
            Defaults::default(),
            false,
        );
        let generation = lock(&state.catalog).entries[&key].generation;
        state.removed(p::SubscriptionEvent {
            event_facility: p::SubscriptionEventFacility::Sink,
            event_type: p::SubscriptionEventType::Removed,
            index: Some(1),
        });
        state.replace(
            BTreeMap::from([(key.clone(), entry(1))]),
            Defaults::default(),
            true,
        );
        assert_ne!(lock(&state.catalog).entries[&key].generation, generation);
        let generation = lock(&state.catalog).entries[&key].generation;
        let mut changed = entry(1);
        changed.serial = Some(9);
        state.replace(
            BTreeMap::from([(key.clone(), changed)]),
            Defaults::default(),
            true,
        );
        assert_ne!(lock(&state.catalog).entries[&key].generation, generation);
        let generation = lock(&state.catalog).entries[&key].generation;
        let mut replaced_server = entry(1);
        replaced_server.serial = Some(9);
        replaced_server.server_cookie = 6;
        state.replace(
            BTreeMap::from([(key.clone(), replaced_server)]),
            Defaults::default(),
            true,
        );
        assert_ne!(lock(&state.catalog).entries[&key].generation, generation);
        let mut catalog = lock(&state.catalog);
        for _ in 0..EVENT_CAPACITY + 1 {
            State::emit(&mut catalog, Event::ResyncRequired);
        }
        assert!(catalog.overflow);
        assert_eq!(catalog.events.len(), EVENT_CAPACITY);
    }
}
