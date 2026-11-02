//! Deterministic in-memory adapter: no hardware, threads, clocks or desktop hotkeys.
//! Scripts apply synchronously; PCM advances only through [`DryStream::pump`], and asynchronous
//! media commands complete only through [`DryBackend::pump_control`]. All callbacks execute outside
//! control-state locks. This models contracts, not native scheduling, latency or feedback physics.

use crate::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

pub const MAX_PUMP_FRAMES: usize = MAX_CAPTURE_BLOCK_FRAMES;
pub const RENDER_SAMPLE_CAPACITY: usize = 4096;
pub const MEDIA_REQUEST_CAPACITY: usize = 16;
pub const MEDIA_SESSION_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PttCancellation {
    Locked,
    SessionLost,
    WorkerTimeout,
}

#[derive(Clone, Debug)]
pub enum ScriptEvent {
    UpsertDevice(Device),
    RemoveDevice(DeviceId),
    InvalidateDevice(DeviceId),
    SetDefaults(Defaults),
    /// External writes invalidate mute ownership, including same-value writes.
    SetMute {
        device: DeviceId,
        muted: bool,
    },
    /// Session tokens must increase for newly introduced sessions and are never reused.
    SetMedia {
        session: MediaSessionToken,
        info: Option<MediaInfo>,
    },
    /// Explicit OS current-session selection; never inferred from ordering or app id.
    SetCurrentMediaSession {
        session: Option<MediaSessionToken>,
    },
    /// Deterministically expires an admitted request without a wall clock or native side effect.
    TimeoutMediaRequest {
        request_id: u64,
    },
    /// Physical down edge; repeats cannot reopen a cancelled lease before physical up.
    PttPress,
    /// Physical up edge; emits proof only if a scripted physical key was actually down.
    PttRelease,
    PttRenew,
    /// Absolute logical monotonic time; also expires held PTT without hidden wall clocks.
    AdvanceTime {
        now_ns: u64,
    },
    CancelPtt(PttCancellation),
    /// None recovers availability. Only ServerUnavailable and PermissionDenied are accepted.
    Availability(Option<BackendError>),
}

struct Endpoint {
    device: Device,
    generation: u64,
    muted: bool,
    mute_revision: u64,
    leased: bool,
}

struct State {
    devices: BTreeMap<DeviceId, Endpoint>,
    defaults: Defaults,
    events: VecDeque<Event>,
    overflowed: bool,
    generation: u64,
    media: BTreeMap<MediaSessionToken, MediaInfo>,
    current_media: Option<MediaSessionToken>,
    max_session_token: Option<u64>,
    requests: VecDeque<MediaCommandRequest>,
    completions: VecDeque<Event>,
    last_media_command: Option<MediaCommandRequest>,
    hotkey: Option<PttHotkey>,
    physical_down: bool,
    // One positive edge survives queue overflow; synthetic closures cannot replace it.
    pending_physical_up: bool,
    now_ns: u64,
    ptt_expires_at_ns: Option<u64>,
    unavailable: Option<BackendError>,
}

impl State {
    fn available(&self) -> Result<(), BackendError> {
        self.unavailable.map_or(Ok(()), Err)
    }

    fn emit(&mut self, event: Event) {
        if self.events.len() == EVENT_CAPACITY {
            if self.events.pop_front()
                == Some(Event::PttReleased {
                    reason: PttReleaseReason::PhysicalKeyUp,
                })
            {
                self.pending_physical_up = true;
            }
            self.overflowed = true;
            self.ptt_expires_at_ns = None;
        }
        if self.overflowed
            && event
                == (Event::PttReleased {
                    reason: PttReleaseReason::PhysicalKeyUp,
                })
        {
            self.pending_physical_up = true;
        } else {
            self.events.push_back(event);
        }
    }

    fn release_ptt(&mut self, reason: PttReleaseReason) {
        let leased = self.ptt_expires_at_ns.take().is_some();
        let physical_up = reason == PttReleaseReason::PhysicalKeyUp && self.physical_down;
        if physical_up {
            self.physical_down = false;
        }
        if physical_up || (leased && reason != PttReleaseReason::PhysicalKeyUp) {
            self.emit(Event::PttReleased { reason });
        }
    }

    fn endpoint(&self, id: &DeviceId, generation: u64) -> Result<&Endpoint, BackendError> {
        self.available()?;
        self.devices
            .get(id)
            .filter(|endpoint| endpoint.generation == generation)
            .ok_or(BackendError::DeviceGone)
    }

    fn resolve(
        &self,
        selection: &DeviceSelection,
        direction: Direction,
    ) -> Result<&Endpoint, BackendError> {
        self.available()?;
        let id = match selection {
            DeviceSelection::Explicit(id) => id,
            DeviceSelection::FollowDefault(role) => {
                let defaults = self.defaults.for_role(*role);
                match direction {
                    Direction::Capture => defaults.capture.as_ref(),
                    Direction::Render => defaults.render.as_ref(),
                }
                .ok_or(BackendError::DeviceGone)?
            }
        };
        let endpoint = self.devices.get(id).ok_or(BackendError::DeviceGone)?;
        if endpoint.device.direction != direction {
            return Err(BackendError::InvalidArgument);
        }
        Ok(endpoint)
    }

    fn execute_media(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        self.available()?;
        let info = self
            .media
            .get_mut(&request.session)
            .ok_or(BackendError::DeviceGone)?;
        if info.revision != request.expected_revision {
            return Err(BackendError::StaleRevision);
        }
        if !info.commands.contains(request.command) {
            return Err(BackendError::Unsupported);
        }
        let revision = info.revision.checked_add(1).ok_or(BackendError::Busy)?;
        match request.command {
            MediaCommand::Play => info.state = PlaybackState::Playing,
            MediaCommand::Pause => info.state = PlaybackState::Paused,
            MediaCommand::Toggle => {
                info.state = if info.state == PlaybackState::Playing {
                    PlaybackState::Paused
                } else {
                    PlaybackState::Playing
                }
            }
            // Track navigation is recorded; scripts provide the resulting track metadata.
            MediaCommand::Next | MediaCommand::Previous => {
                if info.position_ms.is_some() {
                    info.position_ms = Some(0);
                }
            }
            MediaCommand::SeekTo { position_ms } => {
                let (min, max) = info.seek_range().ok_or(BackendError::Unsupported)?;
                if !(min..=max).contains(&position_ms) {
                    return Err(BackendError::InvalidArgument);
                }
                info.position_ms = Some(position_ms);
            }
        }
        info.revision = revision;
        let info = info.clone();
        self.last_media_command = Some(request);
        self.emit(Event::MediaChanged {
            session: request.session,
            info: Some(info),
        });
        Ok(())
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub struct DryBackend {
    state: Arc<Mutex<State>>,
    capabilities: Capabilities,
}

impl DryBackend {
    /// Empty endpoints/sessions, cached capabilities only. Construction performs no probe.
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                devices: BTreeMap::new(),
                defaults: Defaults::default(),
                events: VecDeque::with_capacity(EVENT_CAPACITY),
                overflowed: false,
                generation: 0,
                media: BTreeMap::new(),
                current_media: None,
                max_session_token: None,
                requests: VecDeque::with_capacity(MEDIA_REQUEST_CAPACITY),
                completions: VecDeque::with_capacity(MEDIA_REQUEST_CAPACITY),
                last_media_command: None,
                hotkey: None,
                physical_down: false,
                pending_physical_up: false,
                now_ns: 0,
                ptt_expires_at_ns: None,
                unavailable: None,
            })),
            capabilities,
        }
    }

    /// Last executed command, not merely admitted request. Bounded scripted command sink.
    pub fn last_media_command(&self) -> Option<MediaCommandRequest> {
        lock(&self.state).last_media_command
    }

    /// Executes all currently admitted requests in FIFO order; rechecks session and revision at
    /// execution, and reserves each completion until polled. No callbacks or native IO occur.
    pub fn pump_control(&mut self) {
        let mut state = lock(&self.state);
        while let Some(request) = state.requests.pop_front() {
            let result = state.execute_media(request);
            state.completions.push_back(Event::MediaCommandCompleted {
                request_id: request.request_id,
                result,
            });
        }
    }

    pub fn apply(&mut self, event: ScriptEvent) -> Result<(), BackendError> {
        let mut state = lock(&self.state);
        match event {
            ScriptEvent::UpsertDevice(device) => {
                device.validate()?;
                let generation = state.generation.checked_add(1).ok_or(BackendError::Busy)?;
                let previous = state.devices.get(&device.id);
                let muted = previous.is_some_and(|endpoint| endpoint.muted);
                let notification = if previous.is_some() {
                    DeviceEvent::FormatChanged(device.id.clone())
                } else {
                    DeviceEvent::Added(device.id.clone())
                };
                state.generation = generation;
                state.devices.insert(
                    device.id.clone(),
                    Endpoint {
                        device,
                        generation,
                        muted,
                        mute_revision: 0,
                        leased: false,
                    },
                );
                state.emit(Event::Device(notification));
            }
            ScriptEvent::RemoveDevice(id) => {
                state.devices.remove(&id).ok_or(BackendError::DeviceGone)?;
                state.emit(Event::Device(DeviceEvent::Removed(id.clone())));
                let mut defaults = state.defaults.clone();
                for endpoints in [&mut defaults.multimedia, &mut defaults.communications] {
                    if endpoints.capture.as_ref() == Some(&id) {
                        endpoints.capture = None;
                    }
                    if endpoints.render.as_ref() == Some(&id) {
                        endpoints.render = None;
                    }
                }
                set_defaults(&mut state, defaults)?;
                state.release_ptt(PttReleaseReason::UnavailableOrReset);
            }
            ScriptEvent::InvalidateDevice(id) => {
                if !state.devices.contains_key(&id) {
                    return Err(BackendError::DeviceGone);
                }
                let generation = state.generation.checked_add(1).ok_or(BackendError::Busy)?;
                state.generation = generation;
                let endpoint = state.devices.get_mut(&id).ok_or(BackendError::DeviceGone)?;
                endpoint.generation = generation;
                endpoint.leased = false;
                state.emit(Event::Device(DeviceEvent::Invalidated(id)));
                state.release_ptt(PttReleaseReason::UnavailableOrReset);
            }
            ScriptEvent::SetDefaults(defaults) => set_defaults(&mut state, defaults)?,
            ScriptEvent::SetMute { device, muted } => {
                let endpoint = state
                    .devices
                    .get_mut(&device)
                    .ok_or(BackendError::DeviceGone)?;
                if endpoint.device.direction == Direction::Capture
                    && !self.capabilities.endpoint_mute
                {
                    return Err(BackendError::Unsupported);
                }
                endpoint.mute_revision = endpoint
                    .mute_revision
                    .checked_add(1)
                    .ok_or(BackendError::Busy)?;
                endpoint.muted = muted;
                let snapshot = mute_snapshot(endpoint);
                if endpoint.device.direction == Direction::Capture {
                    state.emit(Event::MuteChanged(snapshot));
                }
            }
            ScriptEvent::SetMedia { session, info } => {
                if !self.capabilities.media {
                    return Err(BackendError::Unsupported);
                }
                if let Some(info) = &info {
                    if info.session != session {
                        return Err(BackendError::InvalidArgument);
                    }
                    if let Some(previous) = state.media.get(&session) {
                        if info.revision <= previous.revision {
                            return Err(BackendError::StaleRevision);
                        }
                    } else {
                        if state
                            .max_session_token
                            .is_some_and(|last| session.0 <= last)
                        {
                            return Err(BackendError::StaleRevision);
                        }
                        if state.media.len() == MEDIA_SESSION_CAPACITY {
                            return Err(BackendError::Busy);
                        }
                        state.max_session_token = Some(session.0);
                    }
                    state.media.insert(session, info.clone());
                } else {
                    state
                        .media
                        .remove(&session)
                        .ok_or(BackendError::DeviceGone)?;
                    cancel_requests(&mut state, Some(session), BackendError::DeviceGone);
                    if state.current_media == Some(session) {
                        state.current_media = None;
                    }
                }
                state.emit(Event::MediaChanged { session, info });
            }
            ScriptEvent::SetCurrentMediaSession { session } => {
                if !self.capabilities.media {
                    return Err(BackendError::Unsupported);
                }
                if session.is_some_and(|token| !state.media.contains_key(&token)) {
                    return Err(BackendError::DeviceGone);
                }
                if state.current_media != session {
                    let changed = session.or(state.current_media);
                    state.current_media = session;
                    if let Some(changed) = changed {
                        let info = state.media.get(&changed).cloned();
                        state.emit(Event::MediaChanged {
                            session: changed,
                            info,
                        });
                    }
                }
            }
            ScriptEvent::TimeoutMediaRequest { request_id } => {
                let position = state
                    .requests
                    .iter()
                    .position(|request| request.request_id == request_id)
                    .ok_or(BackendError::InvalidArgument)?;
                let request = state
                    .requests
                    .remove(position)
                    .ok_or(BackendError::InvalidArgument)?;
                state.completions.push_back(Event::MediaCommandCompleted {
                    request_id: request.request_id,
                    result: Err(BackendError::Timeout),
                });
            }
            ScriptEvent::PttPress => {
                if !self.capabilities.global_ptt || state.hotkey.is_none() {
                    return Err(BackendError::Unsupported);
                }
                state.available()?;
                if !state.physical_down {
                    state.physical_down = true;
                    if state.overflowed || state.pending_physical_up {
                        return Ok(());
                    }
                    let expires_at_ns = state
                        .now_ns
                        .checked_add(PTT_LEASE_NS)
                        .ok_or(BackendError::InvalidArgument)?;
                    let observed_at_ns = state.now_ns;
                    // Lease state is independent of the physical hold.
                    state.ptt_expires_at_ns = Some(expires_at_ns);
                    state.emit(Event::PttPressed {
                        observed_at_ns,
                        expires_at_ns,
                    });
                }
            }
            ScriptEvent::PttRenew => {
                state.available()?;
                if state.ptt_expires_at_ns.is_none() {
                    return Err(BackendError::Stopped);
                }
                let expires_at_ns = state
                    .now_ns
                    .checked_add(PTT_LEASE_NS)
                    .ok_or(BackendError::InvalidArgument)?;
                state.ptt_expires_at_ns = Some(expires_at_ns);
                state.emit(Event::PttRenewed { expires_at_ns });
            }
            ScriptEvent::AdvanceTime { now_ns } => {
                if now_ns < state.now_ns {
                    return Err(BackendError::InvalidArgument);
                }
                state.now_ns = now_ns;
                if state
                    .ptt_expires_at_ns
                    .is_some_and(|expires| expires <= now_ns)
                {
                    state.release_ptt(PttReleaseReason::LeaseExpired);
                }
            }
            ScriptEvent::PttRelease => {
                if !self.capabilities.global_ptt {
                    return Err(BackendError::Unsupported);
                }
                state.release_ptt(PttReleaseReason::PhysicalKeyUp);
            }
            ScriptEvent::CancelPtt(_) => {
                state.release_ptt(PttReleaseReason::UnavailableOrReset);
                // Session loss requires explicit re-registration before accepting new presses.
                state.hotkey = None;
            }
            ScriptEvent::Availability(error) => {
                if !matches!(
                    error,
                    None | Some(BackendError::ServerUnavailable | BackendError::PermissionDenied)
                ) {
                    return Err(BackendError::InvalidArgument);
                }
                state.unavailable = error;
                if let Some(error) = error {
                    cancel_requests(&mut state, None, error);
                }
                state.release_ptt(PttReleaseReason::UnavailableOrReset);
                state.emit(Event::ResyncRequired);
            }
        }
        Ok(())
    }
}

fn cancel_requests(state: &mut State, session: Option<MediaSessionToken>, error: BackendError) {
    let count = state.requests.len();
    for _ in 0..count {
        if let Some(request) = state.requests.pop_front() {
            if session.is_none_or(|session| request.session == session) {
                state.completions.push_back(Event::MediaCommandCompleted {
                    request_id: request.request_id,
                    result: Err(error),
                });
            } else {
                state.requests.push_back(request);
            }
        }
    }
}

fn set_defaults(state: &mut State, defaults: Defaults) -> Result<(), BackendError> {
    for role in [DefaultRole::Multimedia, DefaultRole::Communications] {
        let endpoints = defaults.for_role(role);
        for (id, direction) in [
            (&endpoints.capture, Direction::Capture),
            (&endpoints.render, Direction::Render),
        ] {
            if let Some(id) = id {
                let endpoint = state.devices.get(id).ok_or(BackendError::DeviceGone)?;
                if endpoint.device.direction != direction {
                    return Err(BackendError::InvalidArgument);
                }
            }
        }
    }
    for role in [DefaultRole::Multimedia, DefaultRole::Communications] {
        let previous = state.defaults.for_role(role);
        let next = defaults.for_role(role);
        let capture_changed = previous.capture != next.capture;
        let render_changed = previous.render != next.render;
        if capture_changed {
            state.emit(Event::Device(DeviceEvent::DefaultChanged {
                direction: Direction::Capture,
                role,
            }));
        }
        if render_changed {
            state.emit(Event::Device(DeviceEvent::DefaultChanged {
                direction: Direction::Render,
                role,
            }));
        }
    }
    state.defaults = defaults;
    Ok(())
}

fn mute_snapshot(endpoint: &Endpoint) -> MuteSnapshot {
    MuteSnapshot {
        endpoint: endpoint.device.id.clone(),
        generation: endpoint.generation,
        muted: endpoint.muted,
    }
}

impl Backend for DryBackend {
    type Stream = DryStream;
    type Mute = DryMuteGuard;

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn probe_capabilities(&mut self) -> CapabilityProbe {
        let state = lock(&self.state);
        let availability = |supported: bool| {
            if !supported {
                Availability::Unavailable(BackendError::Unsupported)
            } else if let Some(error) = state.unavailable {
                Availability::Unavailable(error)
            } else {
                Availability::Available
            }
        };
        CapabilityProbe {
            os_build: None,
            process_exclude: availability(
                self.capabilities.exclude_own_process_tree && self.capabilities.system_capture,
            ),
            endpoint_mute: availability(self.capabilities.endpoint_mute),
            media: availability(self.capabilities.media),
            ptt: availability(self.capabilities.global_ptt),
            ptt_release: self
                .capabilities
                .global_ptt
                .then_some(ReleaseObservation::Native),
        }
    }

    fn devices(&self) -> Result<Vec<Device>, BackendError> {
        let state = lock(&self.state);
        state.available()?;
        Ok(state
            .devices
            .values()
            .map(|endpoint| endpoint.device.clone())
            .collect())
    }

    fn defaults(&self) -> Result<Defaults, BackendError> {
        let state = lock(&self.state);
        state.available()?;
        Ok(state.defaults.clone())
    }

    fn start_capture(
        &mut self,
        request: CaptureSpec,
        callback: CaptureCallback,
    ) -> Result<DryStream, BackendError> {
        let state = lock(&self.state);
        state.available()?;
        let monitor = matches!(&request, CaptureSpec::Monitor { .. });
        let (endpoint, format, feedback, microphone) = match request {
            CaptureSpec::SystemExcludingOwnProcessTree { format } => {
                if !self.capabilities.system_capture || !self.capabilities.exclude_own_process_tree
                {
                    return Err(BackendError::Unsupported);
                }
                if format
                    != (AudioFormat {
                        sample_rate: TARGET_SAMPLE_RATE,
                        channels: 2,
                    })
                {
                    return Err(BackendError::InvalidArgument);
                }
                (
                    None,
                    format,
                    FeedbackGuarantee::OwnProcessTreeExcluded,
                    false,
                )
            }
            CaptureSpec::EndpointLoopback { endpoint, format }
            | CaptureSpec::Monitor {
                sink: endpoint,
                format,
            } => {
                format.validate()?;
                if !self.capabilities.system_capture {
                    return Err(BackendError::Unsupported);
                }
                let endpoint = state.resolve(&endpoint, Direction::Render)?;
                (
                    Some((endpoint.device.id.clone(), endpoint.generation)),
                    endpoint.device.format,
                    FeedbackGuarantee::None,
                    false,
                )
            }
            CaptureSpec::Microphone { endpoint, format } => {
                format.validate()?;
                let endpoint = state.resolve(&endpoint, Direction::Capture)?;
                (
                    Some((endpoint.device.id.clone(), endpoint.generation)),
                    endpoint.device.format,
                    FeedbackGuarantee::None,
                    true,
                )
            }
        };
        drop(state);
        let mut stream = DryStream::new(
            self.state.clone(),
            endpoint,
            format,
            feedback,
            microphone,
            Callback::Capture(callback),
        );
        stream.monitor = monitor;
        Ok(stream)
    }

    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<DryStream, BackendError> {
        request.requested.validate()?;
        let state = lock(&self.state);
        let endpoint = state.resolve(&request.device, Direction::Render)?;
        let binding = Some((endpoint.device.id.clone(), endpoint.generation));
        let format = endpoint.device.format;
        drop(state);
        Ok(DryStream::new(
            self.state.clone(),
            binding,
            format,
            FeedbackGuarantee::None,
            false,
            Callback::Render(callback),
        ))
    }

    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError> {
        if !self.capabilities.endpoint_mute {
            return Err(BackendError::Unsupported);
        }
        let state = lock(&self.state);
        Ok(mute_snapshot(state.resolve(selection, Direction::Capture)?))
    }

    fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<DryMuteGuard, BackendError> {
        if !self.capabilities.endpoint_mute {
            return Err(BackendError::Unsupported);
        }
        let mut state = lock(&self.state);
        let snapshot = mute_snapshot(state.resolve(selection, Direction::Capture)?);
        let endpoint = state
            .devices
            .get_mut(&snapshot.endpoint)
            .ok_or(BackendError::DeviceGone)?;
        if endpoint.leased {
            return Err(BackendError::Busy);
        }
        endpoint.leased = true;
        Ok(DryMuteGuard {
            state: self.state.clone(),
            device: snapshot.endpoint,
            generation: snapshot.generation,
            original: snapshot.muted,
            revision: endpoint.mute_revision,
            owned: false,
            active: true,
        })
    }

    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        if !self.capabilities.media {
            return Err(BackendError::Unsupported);
        }
        let state = lock(&self.state);
        state.available()?;
        Ok(state.media.values().cloned().collect())
    }

    fn current_media_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        if !self.capabilities.media {
            return Err(BackendError::Unsupported);
        }
        let state = lock(&self.state);
        state.available()?;
        Ok(state.current_media)
    }

    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        if !self.capabilities.media {
            return Err(BackendError::Unsupported);
        }
        let mut state = lock(&self.state);
        state.available()?;
        if state.requests.len() + state.completions.len() == MEDIA_REQUEST_CAPACITY
            || state.requests.iter().any(|pending| pending.request_id == request.request_id)
            || state.completions.iter().any(|event| matches!(event, Event::MediaCommandCompleted { request_id, .. } if *request_id == request.request_id))
        {
            return Err(BackendError::Busy);
        }
        state.requests.push_back(request);
        Ok(())
    }

    fn cancel_media_commands(&mut self) {
        let mut state = lock(&self.state);
        cancel_requests(&mut state, None, BackendError::Cancelled);
    }

    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        if !self.capabilities.global_ptt {
            return Err(BackendError::Unsupported);
        }
        if let Some(key) = hotkey {
            key.validate()?;
            if reserved.contains(&key) {
                return Err(BackendError::Conflict);
            }
        }
        let mut state = lock(&self.state);
        state.available()?;
        state.release_ptt(PttReleaseReason::UnavailableOrReset);
        state.hotkey = hotkey;
        Ok(())
    }

    fn poll_event(&mut self) -> Option<Event> {
        let mut state = lock(&self.state);
        if state.overflowed {
            state.overflowed = false;
            if state.events.iter().any(|event| {
                *event
                    == (Event::PttReleased {
                        reason: PttReleaseReason::PhysicalKeyUp,
                    })
            }) {
                state.pending_physical_up = true;
            }
            state.events.retain(|event| {
                !matches!(
                    event,
                    Event::PttPressed { .. } | Event::PttRenewed { .. } | Event::PttReleased { .. }
                )
            });
            return Some(Event::ResyncRequired);
        }
        if state.pending_physical_up {
            state.pending_physical_up = false;
            return Some(Event::PttReleased {
                reason: PttReleaseReason::PhysicalKeyUp,
            });
        }
        state
            .completions
            .pop_front()
            .or_else(|| state.events.pop_front())
    }
}

enum Callback {
    Capture(CaptureCallback),
    Render(RenderCallback),
}

pub struct DryStream {
    state: Arc<Mutex<State>>,
    endpoint: Option<(DeviceId, u64)>,
    format: AudioFormat,
    feedback: FeedbackGuarantee,
    microphone: bool,
    monitor: bool,
    source_silence: Option<DryMuteGuard>,
    source_admitted: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    callback: Option<Callback>,
    buffer: Vec<f32>,
    frames: u64,
    status: StreamStatus,
    rendered: VecDeque<f32>,
}

impl DryStream {
    fn new(
        state: Arc<Mutex<State>>,
        endpoint: Option<(DeviceId, u64)>,
        format: AudioFormat,
        feedback: FeedbackGuarantee,
        microphone: bool,
        callback: Callback,
    ) -> Self {
        let samples = MAX_PUMP_FRAMES * usize::from(format.channels);
        Self {
            state,
            endpoint,
            format,
            feedback,
            microphone,
            monitor: false,
            source_silence: None,
            source_admitted: None,
            callback: Some(callback),
            buffer: vec![0.0; samples],
            frames: 0,
            status: StreamStatus::Running,
            rendered: VecDeque::with_capacity(RENDER_SAMPLE_CAPACITY),
        }
    }

    /// Advances 1..=MAX_PUMP_FRAMES actual frames, independent of chunk boundaries.
    /// Sample (frame, channel) is `((frame * channels + channel) % 256) / 128 - 1`.
    /// Muted microphone capture produces silence and advances its clock. Disappearance, profile
    /// replacement and server errors stop the stream and release its callback before returning.
    /// Both 480-frame/48 kHz and 441-frame/44.1 kHz native blocks fit the bounded scratch buffer.
    pub fn pump(&mut self, frames: usize) -> Result<(), BackendError> {
        if let Some(guard) = self.source_silence.as_mut() {
            if self
                .source_admitted
                .as_ref()
                .is_some_and(|admitted| admitted())
            {
                let mut state = lock(&guard.state);
                let endpoint = state
                    .devices
                    .get_mut(&guard.device)
                    .filter(|endpoint| endpoint.generation == guard.generation)
                    .ok_or(BackendError::DeviceGone)?;
                if !endpoint.muted {
                    endpoint.muted = true;
                    endpoint.mute_revision += 1;
                    guard.owned = true;
                }
                guard.revision = endpoint.mute_revision;
            } else {
                // Refresh the render-only lease; microphone ownership remains unchanged.
                {
                    let state = lock(&guard.state);
                    guard.revision = state
                        .endpoint(&guard.device, guard.generation)?
                        .mute_revision;
                }
                guard.restore()?;
                self.source_silence = None;
            }
        }
        if self.callback.is_none() {
            return Err(BackendError::Stopped);
        }
        if frames == 0 || frames > MAX_PUMP_FRAMES {
            return Err(BackendError::InvalidArgument);
        }
        let status = {
            let state = lock(&self.state);
            match &self.endpoint {
                Some((id, generation)) => state
                    .endpoint(id, *generation)
                    .map(|endpoint| endpoint.muted),
                None => state.available().map(|()| false),
            }
        };
        let muted = match status {
            Ok(muted) => muted,
            Err(error) => {
                self.status = stream_status(error);
                self.stop()?;
                return Err(error);
            }
        };
        let channels = usize::from(self.format.channels);
        let samples = &mut self.buffer[..frames * channels];
        match self.callback.as_mut().ok_or(BackendError::Stopped)? {
            Callback::Capture(callback) => {
                let first = (self.frames % 256) * channels as u64;
                for (index, sample) in samples.iter_mut().enumerate() {
                    *sample = if muted && self.microphone {
                        0.0
                    } else {
                        ((first + index as u64) % 256) as f32 / 128.0 - 1.0
                    };
                }
                let timestamp =
                    u128::from(self.frames) * 1_000_000_000 / u128::from(self.format.sample_rate);
                callback(CaptureBlock {
                    samples,
                    frames,
                    format: self.format,
                    timestamp_ns: timestamp.min(u128::from(u64::MAX)) as u64,
                    timestamp_valid: true,
                    device_position: Some(self.frames),
                    discontinuity: false,
                });
            }
            Callback::Render(callback) => {
                samples.fill(0.0);
                callback(samples);
                for sample in samples.iter().copied() {
                    if self.rendered.len() == RENDER_SAMPLE_CAPACITY {
                        self.rendered.pop_front();
                    }
                    self.rendered.push_back(sample);
                }
            }
        }
        self.frames = self.frames.saturating_add(frames as u64);
        Ok(())
    }

    pub fn processed_frames(&self) -> u64 {
        self.frames
    }

    /// Most recent interleaved render samples, capped at RENDER_SAMPLE_CAPACITY.
    pub fn rendered_samples(&self) -> impl Iterator<Item = f32> + '_ {
        self.rendered.iter().copied()
    }
}

impl Stream for DryStream {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn device(&self) -> Option<&DeviceId> {
        self.endpoint.as_ref().map(|(id, _)| id)
    }

    fn feedback_guarantee(&self) -> FeedbackGuarantee {
        self.feedback
    }

    fn snapshot(&self) -> StreamSnapshot {
        let status = if self.status == StreamStatus::Running {
            let state = lock(&self.state);
            match &self.endpoint {
                Some((id, generation)) => state.endpoint(id, *generation).map(|_| ()),
                None => state.available(),
            }
            .map_or_else(stream_status, |()| StreamStatus::Running)
        } else {
            self.status
        };
        StreamSnapshot {
            status,
            device_position: Some(self.frames),
            clock_frequency: Some(u64::from(self.format.sample_rate)),
            host_time_100ns: u64::try_from(
                u128::from(self.frames) * 10_000_000 / u128::from(self.format.sample_rate),
            )
            .ok(),
            underrun_frames: 0,
            dropped_capture_frames: 0,
        }
    }

    fn suppress_local_playback(
        &mut self,
        admitted: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), BackendError> {
        if self.status != StreamStatus::Running {
            return Err(BackendError::Stopped);
        }
        if !self.monitor {
            return Err(BackendError::Unsupported);
        }
        let (device, generation) = self.endpoint.as_ref().ok_or(BackendError::Unsupported)?;
        let (original, observed_revision) = {
            let state = lock(&self.state);
            let endpoint = state.endpoint(device, *generation)?;
            (endpoint.muted, endpoint.mute_revision)
        };
        // Admission is arbitrary caller code: never run it under the Dry state lock.
        if !admitted() {
            return Err(BackendError::PermissionDenied);
        }
        // Never block after admission; revalidate the pinned state before any write.
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(BackendError::Busy),
        };
        let endpoint = state.endpoint(device, *generation)?;
        if endpoint.mute_revision != observed_revision || endpoint.muted != original {
            return Err(BackendError::OwnershipLost);
        }
        if let Some(guard) = &self.source_silence {
            return if guard.active
                && endpoint.leased
                && endpoint.muted
                && endpoint.mute_revision == guard.revision
            {
                Ok(())
            } else {
                Err(BackendError::OwnershipLost)
            };
        }
        if endpoint.leased {
            return Err(BackendError::Busy);
        }
        let revision = if original {
            observed_revision
        } else {
            observed_revision.checked_add(1).ok_or(BackendError::Busy)?
        };
        let endpoint = state
            .devices
            .get_mut(device)
            .ok_or(BackendError::DeviceGone)?;
        endpoint.leased = true;
        endpoint.muted = true;
        endpoint.mute_revision = revision;
        self.source_admitted = Some(admitted);
        self.source_silence = Some(DryMuteGuard {
            state: self.state.clone(),
            device: device.clone(),
            generation: *generation,
            original,
            revision,
            owned: true,
            active: true,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        let restored = self.source_silence.take().map_or(Ok(()), |mut guard| {
            if let Ok(endpoint) = lock(&guard.state).endpoint(&guard.device, guard.generation) {
                guard.revision = endpoint.mute_revision;
            }
            match guard.restore() {
                Err(BackendError::OwnershipLost | BackendError::DeviceGone) => Ok(()),
                result => result,
            }
        });
        self.callback = None;
        if self.status == StreamStatus::Running {
            self.status = StreamStatus::Stopped;
        }
        restored
    }
}

impl Drop for DryStream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn stream_status(error: BackendError) -> StreamStatus {
    if error == BackendError::DeviceGone {
        StreamStatus::DeviceInvalidated
    } else {
        StreamStatus::Failed(error)
    }
}

pub struct DryMuteGuard {
    state: Arc<Mutex<State>>,
    device: DeviceId,
    generation: u64,
    original: bool,
    revision: u64,
    owned: bool,
    active: bool,
}

impl MuteGuard for DryMuteGuard {
    fn original_muted(&self) -> bool {
        self.original
    }

    fn snapshot(&self) -> Result<MuteSnapshot, BackendError> {
        let state = lock(&self.state);
        Ok(mute_snapshot(
            state.endpoint(&self.device, self.generation)?,
        ))
    }

    fn set_capture_mute(
        &mut self,
        expected_generation: u64,
        muted: bool,
    ) -> Result<(), BackendError> {
        if !self.active {
            return Err(BackendError::Stopped);
        }
        if expected_generation != self.generation {
            return Err(BackendError::StaleRevision);
        }
        let mut state = lock(&self.state);
        let current = state.endpoint(&self.device, self.generation)?;
        if current.mute_revision != self.revision {
            self.owned = false;
            return Err(BackendError::OwnershipLost);
        }
        if current.muted == muted {
            return Ok(());
        }
        if !muted && (self.original || !self.owned) {
            return Err(BackendError::PermissionDenied);
        }
        let revision = self.revision.checked_add(1).ok_or(BackendError::Busy)?;
        let endpoint = state
            .devices
            .get_mut(&self.device)
            .ok_or(BackendError::DeviceGone)?;
        endpoint.muted = muted;
        endpoint.mute_revision = revision;
        let snapshot = mute_snapshot(endpoint);
        self.revision = revision;
        self.owned = true;
        state.emit(Event::MuteChanged(snapshot));
        Ok(())
    }

    fn restore(&mut self) -> Result<(), BackendError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut state = lock(&self.state);
        // Release local lease even when unavailable; never restore hardware state blindly.
        let unavailable = state.unavailable;
        let endpoint = state
            .devices
            .get_mut(&self.device)
            .filter(|endpoint| endpoint.generation == self.generation)
            .ok_or(BackendError::DeviceGone)?;
        endpoint.leased = false;
        if let Some(error) = unavailable {
            return Err(error);
        }
        if self.owned && endpoint.mute_revision != self.revision {
            return Err(BackendError::OwnershipLost);
        }
        if self.owned && endpoint.muted != self.original {
            endpoint.mute_revision = endpoint
                .mute_revision
                .checked_add(1)
                .ok_or(BackendError::Busy)?;
            endpoint.muted = self.original;
            let snapshot = mute_snapshot(endpoint);
            if endpoint.device.direction == Direction::Capture {
                state.emit(Event::MuteChanged(snapshot));
            }
        }
        Ok(())
    }
}

impl Drop for DryMuteGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests;
