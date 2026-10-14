//! Input injection: `org.freedesktop.portal.RemoteDesktop` plus the EIS sender
//! (KVM-016).
//!
//! Flow: `CreateSession` -> `SelectDevices(POINTER|KEYBOARD, persist_mode=2)` ->
//! `Start` -> `ConnectToEIS` -> EIS handshake as `Sender` -> bind seat capabilities ->
//! answer `ei_device.done` with `ei_device.ready` (required from `ei_device` version 3)
//! -> wait for the device to be *resumed* -> emulate. After `ConnectToEIS` the portal's
//! `Notify*` methods are never used again: input goes over the EIS file descriptor.
//!
//! Two things are deliberately unlike other libei clients here:
//!
//! * Absolute pointer input needs a region. The EIS server discards absolute events
//!   outside its regions silently, so this backend validates absolute positions instead
//!   of pretending delivery. Relative motion is supported as an explicit event; the
//!   agent may use it with its bounded cursor estimate when absolute regions are absent.
//! * The device must be resumed before anything may be emulated (a freshly advertised
//!   device is paused). [`WaylandInjector::pump`] keeps that state current, and
//!   [`WaylandInjector::inject`] refuses to emit on a paused device.

use std::collections::BTreeSet;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions,
    StartOptions as PortalStartOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::stream::StreamExt;
use futures_util::Stream;
use input_model::{hid_key_to_evdev, InputEvent, KeyState, Position};
use reis::ei;
use reis::event::{self, DeviceCapability, EiEvent};

use crate::capture::portal_versions;
use crate::compositor::{compositor_signature, portal_interfaces, Compositor, PortalVersions};
use crate::grant::PortalGrant;
use crate::{map_portal_error, BackendInfo, WaylandError, REMOTE_DESKTOP_INTERFACE};

/// `ConnectToEIS` requires version 2 of the RemoteDesktop interface.
const REMOTE_DESKTOP_V2: u32 = 2;
/// Handshake name the compositor logs for this client.
const HANDSHAKE_NAME: &str = "lan-workspace";
/// How long to wait for the EIS server to resume the device we asked for.
const DEVICE_RESUME_TIMEOUT: Duration = Duration::from_secs(10);
/// Collects companion devices arriving shortly after the first device resumes.
const DEVICE_DISCOVERY_QUIET: Duration = Duration::from_millis(50);
/// From this `ei_device` version on, a sender must answer `ei_device.done` with
/// `ei_device.ready`, otherwise the EIS server may never send `ei_device.resumed` and
/// nothing may be emulated.
const DEVICE_READY_VERSION: u32 = 3;
/// One wheel detent, the unit of [`InputEvent::Scroll`].
const DETENT_UNITS: i32 = 120;

type EiStream = Pin<Box<dyn Stream<Item = Result<EiEvent, reis::Error>>>>;

/// Where the EIS file descriptor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectSource {
    /// A `RemoteDesktop` portal session (permission dialog, restore token).
    Portal,
    /// A compositor-provided EI socket (`LIBEI_SOCKET`), no portal involved: this is the
    /// `--direct` spike path.
    Direct,
}

/// One EIS region, in the compositor's private "region" coordinate space.
///
/// Regions are the areas the EIS server accepts input for. The compositor may publish a
/// region that does not match the real desktop size (for example a 1920x1080 region for
/// a 4K output), so the region is what the spike has to print in order to judge
/// fractional scaling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    /// X offset in logical pixels.
    pub x: u32,
    /// Y offset in logical pixels.
    pub y: u32,
    /// Width in logical pixels.
    pub width: u32,
    /// Height in logical pixels.
    pub height: u32,
    /// Physical scale the compositor applies for this region.
    pub scale: f32,
}

impl Region {
    /// Copies the region an EIS server advertised.
    #[must_use]
    pub fn from_ei(region: &event::Region) -> Self {
        Self {
            x: region.x,
            y: region.y,
            width: region.width,
            height: region.height,
            scale: region.scale,
        }
    }

    /// `true` when `position` lies inside this region.
    ///
    /// The upper/right edges are exclusive; EIS rejects points outside its regions.
    #[must_use]
    pub fn contains(&self, position: Position) -> bool {
        let x = position.x;
        let y = position.y;
        x >= f64::from(self.x)
            && x < f64::from(self.x) + f64::from(self.width)
            && y >= f64::from(self.y)
            && y < f64::from(self.y) + f64::from(self.height)
    }
}

/// What the injector can actually do on this compositor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaylandCapabilities {
    /// Absolute pointer injection is possible: `ei_pointer_absolute` plus a region.
    pub has_absolute_injection: bool,
    /// Relative pointer injection is possible.
    pub has_relative_injection: bool,
    /// `ei_keyboard` is available.
    pub has_keyboard: bool,
    /// `ei_button` is available.
    pub has_buttons: bool,
    /// `ei_scroll` is available.
    pub has_scroll: bool,
}

/// Result of one [`WaylandInjector::inject`] batch.
#[derive(Debug)]
pub struct InjectOutcome {
    /// How many events were handed to the EIS server before the first failure.
    pub applied_count: usize,
    /// The failure that stopped the batch, if any.
    pub os_error: Option<WaylandError>,
    /// The last absolute position that was actually delivered.
    pub actual: Option<Position>,
}

impl InjectOutcome {
    fn ok() -> Self {
        Self {
            applied_count: 0,
            os_error: None,
            actual: None,
        }
    }

    /// `true` when every event in the batch was applied.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.os_error.is_none()
    }
}

/// Per-device protocol state needed to maintain independent EIS serials and frames.
struct SenderDevice {
    device: event::Device,
    last_serial: u32,
    sequence: u32,
    emulating: bool,
    resumed: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct DeviceOffer {
    absolute_with_region: bool,
    relative_pointer: bool,
    keyboard: bool,
    button: bool,
    scroll: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DeviceSelection {
    absolute: Option<usize>,
    relative: Option<usize>,
    pointer: Option<usize>,
    keyboard: Option<usize>,
    button: Option<usize>,
    scroll: Option<usize>,
}

fn select_devices(offers: &[DeviceOffer]) -> DeviceSelection {
    let absolute = offers.iter().position(|offer| offer.absolute_with_region);
    let relative = offers.iter().position(|offer| offer.relative_pointer);
    let pointer = absolute.or(relative);
    let keyboard = offers.iter().position(|offer| offer.keyboard);
    let button = pointer
        .filter(|index| offers[*index].button)
        .or_else(|| offers.iter().position(|offer| offer.button));
    let scroll = pointer
        .filter(|index| offers[*index].scroll)
        .or_else(|| offers.iter().position(|offer| offer.scroll));
    DeviceSelection {
        absolute,
        relative,
        pointer,
        keyboard,
        button,
        scroll,
    }
}
fn route_event(selection: DeviceSelection, event: InputEvent) -> Option<usize> {
    match event {
        InputEvent::PointerAbsolute(_) => selection.absolute,
        InputEvent::PointerRelative { .. } => selection.relative,
        InputEvent::Key { .. } => selection.keyboard,
        InputEvent::Button { .. } => selection.button,
        InputEvent::Scroll { .. } => selection.scroll,
    }
}

/// EIS sender that merges independently advertised interfaces from one seat.
pub struct WaylandInjector {
    session: Option<Session<RemoteDesktop>>,
    connection: event::Connection,
    events: EiStream,
    devices: Vec<SenderDevice>,
    selection: DeviceSelection,
    active_device: Option<usize>,
    device_name: Option<String>,
    device_type: ei::device::DeviceType,
    keyboard: Option<ei::Keyboard>,
    pointer: Option<ei::Pointer>,
    pointer_absolute: Option<ei::PointerAbsolute>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    regions: Vec<Region>,
    dimensions: Option<(u32, u32)>,
    source: InjectSource,
    compositor: Option<Compositor>,
    versions: PortalVersions,
    restore_token: Option<String>,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    scroll_remainder: (i32, i32),
    cursor: Option<Position>,
    started: Instant,
    ei_error: Option<reis::Error>,
    last_inject_report: Option<Instant>,
    applied_edges: input_model::EdgeCounters,
}

impl WaylandInjector {
    /// Opens a `RemoteDesktop` portal session and connects to EIS through it.
    ///
    /// `grant` is a stored token (KVM-017); it is only sent when it belongs to this
    /// compositor and these portal versions.
    pub async fn connect(grant: Option<&PortalGrant>) -> Result<Self, WaylandError> {
        let compositor = Compositor::detect()?;
        let versions = portal_versions().await;
        let signature = compositor_signature(compositor, &versions);

        let remote_desktop = RemoteDesktop::new()
            .await
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        let portal_version = remote_desktop.version();
        if portal_version < REMOTE_DESKTOP_V2 {
            return Err(WaylandError::PortalTooOld {
                portal_interface: REMOTE_DESKTOP_INTERFACE,
                required: REMOTE_DESKTOP_V2,
                found: portal_version,
            });
        }

        let restore_token = grant
            .filter(|grant| {
                grant.portal_interface == REMOTE_DESKTOP_INTERFACE
                    && grant.compositor_signature == signature
            })
            .map(|grant| grant.token().to_owned());

        let session = remote_desktop
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        let select = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
        remote_desktop
            .select_devices(&session, select)
            .await
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        let selected = remote_desktop
            .start(&session, None, PortalStartOptions::default())
            .await
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?
            .response()
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        // The portal issues a fresh token for this endpoint; a token we sent in is only
        // an input to the dialog.
        let fresh_token = selected.restore_token().map(ToOwned::to_owned);

        let fd = remote_desktop
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
            .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        context.flush().map_err(std::io::Error::from)?;

        Self::attach(
            context,
            InjectSource::Portal,
            Some(compositor),
            versions,
            fresh_token,
            Some(session),
        )
        .await
    }

    /// Connects to a compositor-provided EI socket (`LIBEI_SOCKET`) without a portal
    /// session, so no permission dialog appears.
    ///
    /// This is spike question 4; it is opt-in (`--direct`) and never the default,
    /// because the socket is only handed to clients the compositor launched itself.
    pub async fn connect_direct() -> Result<Self, WaylandError> {
        let Some(context) = ei::Context::connect_to_env()? else {
            return Err(WaylandError::EiSocketUnavailable);
        };
        context.flush().map_err(std::io::Error::from)?;
        let compositor = Compositor::detect().ok();
        let versions = portal_versions().await;
        Self::attach(
            context,
            InjectSource::Direct,
            compositor,
            versions,
            None,
            None,
        )
        .await
    }

    async fn attach(
        context: ei::Context,
        source: InjectSource,
        compositor: Option<Compositor>,
        versions: PortalVersions,
        restore_token: Option<String>,
        portal_session: Option<Session<RemoteDesktop>>,
    ) -> Result<Self, WaylandError> {
        let (connection, mut events) = context
            .handshake_tokio(HANDSHAKE_NAME, ei::handshake::ContextType::Sender)
            .await?;
        let mut devices = Vec::new();

        let handshake = tokio::time::timeout(DEVICE_RESUME_TIMEOUT, async {
            let mut any_resumed = false;
            loop {
                let event = if any_resumed {
                    match tokio::time::timeout(DEVICE_DISCOVERY_QUIET, events.next()).await {
                        Ok(event) => event,
                        Err(_) => break,
                    }
                } else {
                    events.next().await
                };
                let Some(event) = event else {
                    return Err(WaylandError::EiDisconnected);
                };
                match event? {
                    EiEvent::SeatAdded(seat_added) => {
                        tracing::debug!(
                            seat = ?seat_added.seat.name(),
                            requested_capabilities = ?seat_capabilities(),
                            "EIS seat advertised; binding requested capabilities"
                        );
                        seat_added.seat.bind_capabilities(seat_capabilities());
                        connection.flush().map_err(std::io::Error::from)?;
                    }
                    EiEvent::DeviceAdded(added) => {
                        log_device("done", &added.device);
                        declare_device_ready(&added.device);
                        if !devices
                            .iter()
                            .any(|state: &SenderDevice| state.device == added.device)
                        {
                            devices.push(SenderDevice {
                                device: added.device,
                                last_serial: connection.serial(),
                                sequence: 0,
                                emulating: false,
                                resumed: false,
                            });
                        }
                    }
                    EiEvent::DeviceResumed(resumed) => {
                        log_device("resumed", &resumed.device);
                        let Some(device) = devices
                            .iter_mut()
                            .find(|state| state.device == resumed.device)
                        else {
                            continue;
                        };
                        device.last_serial = resumed.serial;
                        device.resumed = true;
                        any_resumed = true;
                    }
                    EiEvent::DevicePaused(paused) => {
                        if let Some(device) = devices
                            .iter_mut()
                            .find(|state| state.device == paused.device)
                        {
                            device.resumed = false;
                        }
                        any_resumed = devices.iter().any(|device| device.resumed);
                    }
                    EiEvent::DeviceRemoved(removed) => {
                        devices.retain(|state| state.device != removed.device);
                        any_resumed = devices.iter().any(|device| device.resumed);
                    }
                    EiEvent::Disconnected(_) => return Err(WaylandError::EiDisconnected),
                    _ => {}
                }
            }
            if any_resumed {
                Ok(())
            } else {
                Err(WaylandError::NoEiDevice)
            }
        })
        .await;
        match handshake {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_elapsed) => {
                return Err(WaylandError::DeviceNotResumed {
                    seconds: DEVICE_RESUME_TIMEOUT.as_secs(),
                });
            }
        }
        let mut injector = Self {
            session: portal_session,
            connection,
            events: Box::pin(events),
            devices,
            selection: DeviceSelection::default(),
            active_device: None,
            device_name: None,
            device_type: ei::device::DeviceType::Virtual,
            keyboard: None,
            pointer: None,
            pointer_absolute: None,
            button: None,
            scroll: None,
            regions: Vec::new(),
            dimensions: None,
            source,
            compositor,
            versions,
            restore_token,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            scroll_remainder: (0, 0),
            cursor: None,
            started: Instant::now(),
            ei_error: None,
            last_inject_report: None,
            applied_edges: input_model::EdgeCounters::default(),
        };
        injector.refresh_selection();
        injector.log_selection();
        let _ = injector.connection.flush();
        Ok(injector)
    }

    /// Processes every EIS event that is already available.
    ///
    /// Call this before [`WaylandInjector::inject`] so a `paused`/`resumed` transition,
    /// or a device replacement, is not missed. Returns how many events were handled.
    pub async fn pump(&mut self) -> usize {
        let mut processed = 0;
        loop {
            // A zero-length wait still polls the stream once, so this only takes events
            // that are already on the socket.
            match tokio::time::timeout(Duration::ZERO, self.events.next()).await {
                Ok(Some(Ok(event))) => {
                    processed += 1;
                    self.handle_event(event);
                }
                Ok(Some(Err(error))) => {
                    self.ei_error = Some(error);
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }
        processed
    }

    /// Edges encoded into EIS frames for the current injection lease.
    pub fn applied_edges(&self) -> input_model::EdgeCounters {
        self.applied_edges
    }

    pub fn reset_applied_edges(&mut self) {
        self.applied_edges = input_model::EdgeCounters::default();
    }

    /// Emulates a batch of events, each committed in its own `ei_device.frame`.
    ///
    /// One frame per event keeps the events in order and satisfies the protocol's
    /// "at most one absolute motion / one scroll per frame" rules by construction.
    ///
    /// `applied_count` counts the events written to the EIS socket; `os_error` is set
    /// when an event could not be encoded or when the write/flush failed, in which case
    /// the counted events did not reach the compositor.
    pub fn inject(&mut self, events: &[InputEvent]) -> InjectOutcome {
        let mut outcome = InjectOutcome::ok();
        for event in events {
            let index = self.device_for_event(*event);
            self.active_device = index;
            if let Some(index) = index {
                if !self.ensure_emulating(index) {
                    outcome.os_error = Some(WaylandError::DevicePaused);
                    break;
                }
            }
            match self.apply(*event) {
                Ok(actual) => {
                    outcome.applied_count += 1;
                    self.applied_edges.record(event);
                    if actual.is_some() {
                        outcome.actual = actual;
                    }
                }
                Err(error) => {
                    outcome.os_error = Some(error);
                    break;
                }
            }
        }
        if let Err(error) = self.connection.flush() {
            outcome.os_error = Some(WaylandError::Io(error.into()));
        }
        let now = Instant::now();
        if self
            .last_inject_report
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            self.last_inject_report = Some(now);
            let mut keys = 0;
            let mut pointers = 0;
            let mut buttons = 0;
            let mut scrolls = 0;
            for event in events {
                match event {
                    InputEvent::Key { .. } => keys += 1,
                    InputEvent::PointerAbsolute(_) | InputEvent::PointerRelative { .. } => {
                        pointers += 1;
                    }
                    InputEvent::Button { .. } => buttons += 1,
                    InputEvent::Scroll { .. } => scrolls += 1,
                }
            }
            let first_key_mapping = events.iter().find_map(|event| match event {
                InputEvent::Key { key, .. } => Some((key.page, key.usage, hid_key_to_evdev(*key))),
                _ => None,
            });
            tracing::info!(
                events = events.len(),
                first_key_mapping = ?first_key_mapping,
                keys,
                pointers,
                buttons,
                scrolls,
                applied = outcome.applied_count,
                os_error = ?outcome.os_error,
                actual_position = ?outcome.actual,
                "EIS injection summary"
            );
        }
        outcome
    }

    /// Releases every key and button this injector still holds down.
    ///
    /// A paused device is neutral by protocol, so the ledger is simply dropped then.
    pub fn release_all(&mut self) -> InjectOutcome {
        let mut outcome = InjectOutcome::ok();
        for (index, codes, keyboard) in [
            (self.selection.keyboard, self.pressed_keys.clone(), true),
            (self.selection.button, self.pressed_buttons.clone(), false),
        ] {
            if let Some(index) = index {
                if self.devices[index].emulating {
                    self.active_device = Some(index);
                    for code in codes {
                        if keyboard {
                            if let Some(interface) = self.keyboard.as_ref() {
                                interface.key(u32::from(code), ei::keyboard::KeyState::Released);
                            }
                            self.pressed_keys.remove(&code);
                        } else {
                            if let Some(interface) = self.button.as_ref() {
                                interface
                                    .button(u32::from(code), ei::button::ButtonState::Released);
                            }
                            self.pressed_buttons.remove(&code);
                        }
                        self.commit_frame();
                        outcome.applied_count += 1;
                    }
                }
            }
        }
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        if let Err(error) = self.connection.flush() {
            outcome.os_error = Some(WaylandError::Io(error.into()));
        }
        self.scroll_remainder = (0, 0);
        outcome
    }

    /// Closes the EIS connection and the portal session.
    ///
    /// The EIS implementation releases anything still logically pressed when the
    /// connection goes away; [`WaylandInjector::release_all`] runs first anyway. The
    /// socket is closed when this value is dropped.
    pub async fn disconnect(&mut self) -> Result<(), WaylandError> {
        let release = self.release_all();
        for device in &mut self.devices {
            if device.emulating {
                device.device.device().stop_emulating(device.last_serial);
                device.emulating = false;
            }
            device.resumed = false;
        }
        let _ = self.connection.flush();
        // Best effort write barrier; the callback is never awaited because the
        // connection is closed immediately afterwards.
        let _callback = self.connection.connection().sync(1);
        let _ = self.connection.flush();
        self.connection.connection().disconnect();
        let _ = self.connection.flush();
        if let Some(session) = self.session.take() {
            session
                .close()
                .await
                .map_err(|error| map_portal_error(REMOTE_DESKTOP_INTERFACE, error))?;
        }
        match release.os_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The first region the EIS server advertised, if any.
    #[must_use]
    pub fn absolute_region(&self) -> Option<Region> {
        self.regions.first().copied()
    }

    /// Every region the EIS server advertised.
    #[must_use]
    pub fn regions(&self) -> &[Region] {
        &self.regions
    }

    /// Physical device dimensions in millimetres, for physical devices only.
    #[must_use]
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    /// What this injector can do.
    #[must_use]
    pub fn capabilities(&self) -> WaylandCapabilities {
        WaylandCapabilities {
            has_absolute_injection: self.selection.absolute.is_some(),
            has_relative_injection: self.selection.relative.is_some(),
            has_keyboard: self.selection.keyboard.is_some(),
            has_buttons: self.selection.button.is_some(),
            has_scroll: self.selection.scroll.is_some(),
        }
    }

    /// Whether the EIS descriptor came from a portal session or from `LIBEI_SOCKET`.
    #[must_use]
    pub fn source(&self) -> InjectSource {
        self.source
    }

    /// The compositor, when it could be detected.
    #[must_use]
    pub fn compositor(&self) -> Option<Compositor> {
        self.compositor
    }

    /// The name the EIS server gave the device.
    #[must_use]
    pub fn device_name(&self) -> Option<&str> {
        self.device_name.as_deref()
    }

    /// Whether the device is a virtual or a physical one.
    #[must_use]
    pub fn device_type(&self) -> ei::device::DeviceType {
        self.device_type
    }

    /// The last serial on the selected pointer device, or another selected device.
    #[must_use]
    pub fn last_serial(&self) -> u32 {
        self.selection
            .pointer
            .or(self.selection.keyboard)
            .and_then(|index| self.devices.get(index))
            .map_or(0, |device| device.last_serial)
    }

    /// Whether a selected EIS device is resumed.
    #[must_use]
    pub fn is_resumed(&self) -> bool {
        self.devices.iter().any(|device| device.resumed)
    }

    /// The last absolute position that was successfully injected.
    #[must_use]
    pub fn cursor(&self) -> Option<Position> {
        self.cursor
    }

    /// Maps a logical position onto EIS region coordinates.
    ///
    /// EIS region coordinates are the compositor's private layout, so for the common
    /// single-region case this is the identity; the value is rejected when
    /// `position` is not inside any region, because the EIS server would silently
    /// discard an event that lands outside them.
    #[must_use]
    pub fn resolve_absolute(&self, position: Position) -> Option<(f32, f32)> {
        let inside = self.regions.iter().any(|region| region.contains(position));
        inside.then_some((position.x as f32, position.y as f32))
    }

    /// A fresh restore token for this endpoint (KVM-017), if the portal issued one.
    #[must_use]
    pub fn grant(&self) -> Option<PortalGrant> {
        let token = self.restore_token.clone()?;
        let compositor = self.compositor?;
        Some(PortalGrant::new(
            REMOTE_DESKTOP_INTERFACE,
            token,
            compositor_signature(compositor, &self.versions),
        ))
    }

    /// The negotiated `ei_connection` protocol version.
    #[must_use]
    pub fn ei_protocol_version(&self) -> Option<u32> {
        let connection = self.connection.connection();
        connection.is_alive().then(|| connection.version())
    }

    /// The portal versions observed when the session was created.
    #[must_use]
    pub fn versions(&self) -> PortalVersions {
        self.versions
    }

    /// The last EIS error the event stream produced, if any.
    pub fn take_ei_error(&mut self) -> Option<reis::Error> {
        self.ei_error.take()
    }

    /// `docs/ipc.proto` `BackendInfo`, filled in from what this injector observed.
    #[must_use]
    pub fn backend_info(&self) -> BackendInfo {
        BackendInfo {
            backend: BackendInfo::BACKEND.to_owned(),
            compositor: self
                .compositor
                .map_or("unknown", Compositor::as_str)
                .to_owned(),
            portals: portal_interfaces(&self.versions),
            has_emergency_hotkey: self.versions.global_shortcuts > 0,
            has_portal_persistence: self.restore_token.is_some(),
            // Handing input capture to us is the InputCapture portal's job; the peer's
            // ability to drive it does not depend on this session.
            has_receiver_takeover: self.versions.input_capture > 0,
            has_absolute_injection: self.capabilities().has_absolute_injection,
        }
    }

    fn handle_event(&mut self, event: EiEvent) {
        match event {
            EiEvent::SeatAdded(seat_added) => {
                tracing::debug!(
                    seat = ?seat_added.seat.name(),
                    requested_capabilities = ?seat_capabilities(),
                    "EIS seat added"
                );
                seat_added.seat.bind_capabilities(seat_capabilities());
                let _ = self.connection.flush();
            }
            EiEvent::DeviceAdded(added) => {
                log_device("done", &added.device);
                declare_device_ready(&added.device);
                if !self
                    .devices
                    .iter()
                    .any(|state| state.device == added.device)
                {
                    self.devices.push(SenderDevice {
                        device: added.device,
                        last_serial: self.connection.serial(),
                        sequence: 0,
                        emulating: false,
                        resumed: false,
                    });
                }
                self.refresh_selection();
                self.log_selection();
            }
            EiEvent::DeviceResumed(resumed) => {
                log_device("resumed", &resumed.device);
                if let Some(state) = self.devices.iter_mut().find(|s| s.device == resumed.device) {
                    state.last_serial = resumed.serial;
                    state.resumed = true;
                    state.emulating = false;
                }
            }
            EiEvent::DevicePaused(paused) => {
                if let Some(state) = self.devices.iter_mut().find(|s| s.device == paused.device) {
                    state.resumed = false;
                    state.emulating = false;
                }
                self.pressed_keys.clear();
                self.pressed_buttons.clear();
                self.scroll_remainder = (0, 0);
            }
            EiEvent::DeviceRemoved(removed) => {
                self.devices.retain(|s| s.device != removed.device);
                self.refresh_selection();
                self.log_selection();
                self.pressed_keys.clear();
                self.pressed_buttons.clear();
                self.scroll_remainder = (0, 0);
            }
            EiEvent::Disconnected(_) => {
                for state in &mut self.devices {
                    state.resumed = false;
                    state.emulating = false;
                }
            }
            _ => {}
        }
    }

    fn refresh_selection(&mut self) {
        let offers: Vec<_> = self
            .devices
            .iter()
            .map(|state| DeviceOffer {
                absolute_with_region: state.device.interface::<ei::PointerAbsolute>().is_some()
                    && !state.device.regions().is_empty(),
                relative_pointer: state.device.interface::<ei::Pointer>().is_some(),
                keyboard: state.device.interface::<ei::Keyboard>().is_some(),
                button: state.device.interface::<ei::Button>().is_some(),
                scroll: state.device.interface::<ei::Scroll>().is_some(),
            })
            .collect();
        self.selection = select_devices(&offers);
        let selection = self.selection;
        let metadata_device = selection.pointer.or(selection.keyboard);
        self.device_name = metadata_device
            .and_then(|index| self.devices[index].device.name().map(ToOwned::to_owned));
        self.device_type = metadata_device
            .map(|index| self.devices[index].device.device_type())
            .unwrap_or(ei::device::DeviceType::Virtual);
        self.pointer_absolute = selection
            .absolute
            .and_then(|i| self.devices[i].device.interface::<ei::PointerAbsolute>());
        self.pointer = selection
            .relative
            .and_then(|i| self.devices[i].device.interface::<ei::Pointer>());
        self.keyboard = selection
            .keyboard
            .and_then(|i| self.devices[i].device.interface::<ei::Keyboard>());
        self.button = selection
            .button
            .and_then(|i| self.devices[i].device.interface::<ei::Button>());
        self.scroll = selection
            .scroll
            .and_then(|i| self.devices[i].device.interface::<ei::Scroll>());
        self.regions = selection
            .absolute
            .map(|i| {
                self.devices[i]
                    .device
                    .regions()
                    .iter()
                    .map(Region::from_ei)
                    .collect()
            })
            .unwrap_or_default();
        self.dimensions = metadata_device.and_then(|i| self.devices[i].device.dimensions());
    }

    fn log_selection(&self) {
        let selected_name =
            |index: Option<usize>| index.and_then(|index| self.devices[index].device.name());
        tracing::info!(
            pointer = ?selected_name(self.selection.pointer),
            absolute_pointer = ?selected_name(self.selection.absolute),
            relative_pointer = ?selected_name(self.selection.relative),
            keyboard = ?selected_name(self.selection.keyboard),
            buttons = ?selected_name(self.selection.button),
            scroll = ?selected_name(self.selection.scroll),
            "EIS injection devices selected"
        );
    }

    fn device_for_event(&self, event: InputEvent) -> Option<usize> {
        route_event(self.selection, event)
    }

    fn ensure_emulating(&mut self, index: usize) -> bool {
        let name = self.devices[index].device.name().map(str::to_owned);
        let state = &mut self.devices[index];
        if !state.resumed {
            return false;
        }
        if !state.emulating {
            state.sequence = state.sequence.wrapping_add(1);
            state
                .device
                .device()
                .start_emulating(state.last_serial, state.sequence);
            state.emulating = true;
            tracing::info!(
                device = index,
                name = ?name,
                sequence = state.sequence,
                "EIS device emulation started"
            );
        }
        true
    }

    fn apply(&mut self, event: InputEvent) -> Result<Option<Position>, WaylandError> {
        match event {
            InputEvent::PointerAbsolute(position) => {
                let pointer = match (self.pointer_absolute.as_ref(), self.regions.is_empty()) {
                    (_, true) => return Err(WaylandError::NoAbsoluteRegion),
                    (None, false) => {
                        return Err(WaylandError::MissingInterface {
                            interface: "ei_pointer_absolute",
                        });
                    }
                    (Some(pointer), false) => pointer,
                };
                let (x, y) =
                    self.resolve_absolute(position)
                        .ok_or(WaylandError::PositionOutOfRegion {
                            x: position.x,
                            y: position.y,
                        })?;
                pointer.motion_absolute(x, y);
                self.cursor = Some(position);
                self.commit_frame();
                Ok(Some(position))
            }
            InputEvent::Key { key, state } => {
                let code = hid_key_to_evdev(key).ok_or(WaylandError::UnmappedUsage {
                    page: key.page,
                    usage: key.usage,
                })?;
                let keyboard = self
                    .keyboard
                    .as_ref()
                    .ok_or(WaylandError::MissingInterface {
                        interface: "ei_keyboard",
                    })?;
                keyboard.key(u32::from(code), key_state(state));
                if state.is_down() {
                    self.pressed_keys.insert(code);
                } else {
                    self.pressed_keys.remove(&code);
                }
                self.commit_frame();
                Ok(None)
            }
            InputEvent::Button { button, state } => {
                let code = button.evdev_code();
                let interface = self.button.as_ref().ok_or(WaylandError::MissingInterface {
                    interface: "ei_button",
                })?;
                interface.button(u32::from(code), button_state(state));
                if state.is_down() {
                    self.pressed_buttons.insert(code);
                } else {
                    self.pressed_buttons.remove(&code);
                }
                self.commit_frame();
                Ok(None)
            }
            InputEvent::Scroll { h120, v120 } => {
                let scroll = self.scroll.as_ref().ok_or(WaylandError::MissingInterface {
                    interface: "ei_scroll",
                })?;
                let steps_x = split_detents(h120, &mut self.scroll_remainder.0);
                let steps_y = split_detents(v120, &mut self.scroll_remainder.1);
                if steps_x != 0 || steps_y != 0 {
                    scroll.scroll_discrete(steps_x, steps_y);
                    self.commit_frame();
                }
                Ok(None)
            }
            InputEvent::PointerRelative { dx, dy } => {
                let pointer = self
                    .pointer
                    .as_ref()
                    .ok_or(WaylandError::MissingInterface {
                        interface: "ei_pointer",
                    })?;
                pointer.motion_relative(dx as f32, dy as f32);
                self.commit_frame();
                Ok(None)
            }
        }
    }

    fn commit_frame(&mut self) {
        if let Some(index) = self.active_device {
            let timestamp = self.timestamp_us();
            let state = &self.devices[index];
            if state.emulating {
                state.device.device().frame(state.last_serial, timestamp);
            }
        }
    }
    fn timestamp_us(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Records the completed capability set at `ei_device.done` and `resumed`. In reis,
/// regions and interface objects are complete before `done` is surfaced.
fn log_device(stage: &str, device: &event::Device) {
    tracing::debug!(
        stage,
        seat = ?device.seat().name(),
        name = ?device.name(),
        device_type = ?device.device_type(),
        pointer = device.has_capability(DeviceCapability::Pointer),
        pointer_absolute = device.has_capability(DeviceCapability::PointerAbsolute),
        button = device.has_capability(DeviceCapability::Button),
        scroll = device.has_capability(DeviceCapability::Scroll),
        keyboard = device.has_capability(DeviceCapability::Keyboard),
        dimensions = ?device.dimensions(),
        regions = ?device.regions(),
        "EIS device capabilities"
    );
}

/// The capabilities we bind on every seat: the product needs pointer, absolute pointer,
/// keyboard, buttons and wheel.
fn seat_capabilities() -> BitFlags<DeviceCapability> {
    DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Button
        | DeviceCapability::Scroll
}

/// Answers a sender's `ei_device.done` with `ei_device.ready`.
///
/// From `ei_device` version 3 on the EIS server may hold back `ei_device.resumed` until
/// the client declares the device configuration complete; without this, a sender on
/// such a server would wait forever for a resume and could never emulate anything. It
/// is a protocol violation to send it more than once per device, and it must only be
/// sent by a sender, which is what this crate is.
fn declare_device_ready(device: &event::Device) {
    let protocol = device.device();
    if protocol.version() >= DEVICE_READY_VERSION {
        protocol.ready();
    }
}

/// Splits `value` (in 120ths of a wheel detent) into whole detents, keeping the
/// sub-detent remainder in `remainder` for the next event.
pub(crate) fn split_detents(value: i32, remainder: &mut i32) -> i32 {
    // `i64` accumulator on purpose: the previous `i32::saturating_add` *lost* units at the
    // integer limits (a remainder of `i32::MAX` plus a positive value saturated instead of
    // carrying the excess into the next event), which broke the conservation invariant
    // `steps * 120 + remainder == value + previous_remainder`.
    let total = i64::from(value) + i64::from(*remainder);
    let steps = total / i64::from(DETENT_UNITS);
    *remainder = (total - steps * i64::from(DETENT_UNITS)) as i32;
    // `|total| <= 2 * i32::MAX` and `DETENT_UNITS == 120`, so `|steps| < 2^31` by a wide
    // margin; the cast cannot truncate.
    steps as i32
}

fn key_state(state: KeyState) -> ei::keyboard::KeyState {
    if state.is_down() {
        ei::keyboard::KeyState::Press
    } else {
        ei::keyboard::KeyState::Released
    }
}

fn button_state(state: KeyState) -> ei::button::ButtonState {
    if state.is_down() {
        ei::button::ButtonState::Press
    } else {
        ei::button::ButtonState::Released
    }
}

#[cfg(test)]
mod tests {

    use super::{route_event, select_devices, split_detents, DeviceOffer, DeviceSelection};
    use input_model::{HidKey, InputEvent, KeyState, MouseButton, Position};

    #[tokio::test]
    async fn eis_click_frames_and_lease_end_release_every_held_button() {
        use super::{InjectSource, PortalVersions, UnixStream, WaylandInjector};
        use reis::request::{EisRequest, EisRequestConverter};
        use reis::{eis, handshake::EisHandshaker};

        // An in-process EIS server, never a desktop or portal connection.
        let (client, server) = UnixStream::pair().expect("socket pair");
        let server = tokio::spawn(async move {
            let context = eis::Context::new(server).expect("server context");
            let mut handshake = EisHandshaker::new(&context, 1);
            let mut converter: Option<EisRequestConverter> = None;
            let mut buttons = Vec::new();
            let mut frames = 0;
            while frames < 8 {
                context.read().expect("read client requests");
                while let Some(request) = context.pending_request() {
                    let reis::PendingRequestResult::Request(request) = request else {
                        panic!("invalid EIS request");
                    };
                    if let Some(converter) = converter.as_mut() {
                        converter.handle_request(request).expect("convert request");
                        while let Some(request) = converter.next_request() {
                            match request {
                                EisRequest::Bind(bind) => {
                                    let device = bind.seat.add_device(
                                        Some("test buttons"),
                                        eis::device::DeviceType::Virtual,
                                        reis::request::DeviceCapability::Button.into(),
                                        |_| {},
                                    );
                                    device.resumed();
                                }
                                EisRequest::Ready(ready) => ready.device.resumed(),
                                EisRequest::Button(button) => {
                                    buttons.push((button.button, button.state));
                                }
                                EisRequest::Frame(_) => frames += 1,
                                _ => {}
                            }
                        }
                    } else if let Some(response) =
                        handshake.handle_request(request).expect("handshake")
                    {
                        let requests = EisRequestConverter::new(&context, response, 1);
                        let _ = requests.handle().add_seat(
                            Some("test seat"),
                            reis::request::DeviceCapability::Button.into(),
                        );
                        converter = Some(requests);
                    }
                }
                context.flush().expect("flush server");
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            buttons
        });
        let mut injector = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            WaylandInjector::attach(
                reis::ei::Context::new(client).expect("client context"),
                InjectSource::Direct,
                None,
                PortalVersions::default(),
                None,
                None,
            ),
        )
        .await
        .expect("bounded handshake")
        .expect("attach test injector");
        let edge = |button, state| InputEvent::Button { button, state };
        let click = [
            edge(MouseButton::Left, KeyState::Down),
            edge(MouseButton::Left, KeyState::Up),
        ];
        let outcome = injector.inject(&click);
        assert!(outcome.os_error.is_none());
        assert_eq!(outcome.applied_count, 2);
        assert_eq!(injector.applied_edges().buttons_down, 1);
        assert_eq!(injector.applied_edges().buttons_up, 1);
        let outcome = injector.inject(&[
            edge(MouseButton::Left, KeyState::Down),
            edge(MouseButton::Right, KeyState::Down),
            edge(MouseButton::Middle, KeyState::Down),
        ]);
        assert!(outcome.os_error.is_none());
        assert_eq!(outcome.applied_count, 3);
        let released = injector.release_all();
        assert!(released.os_error.is_none());
        assert_eq!(released.applied_count, 3);
        assert_eq!(injector.release_all().applied_count, 0);
        let buttons = tokio::time::timeout(std::time::Duration::from_secs(3), server)
            .await
            .expect("bounded server observation")
            .expect("server task");
        use eis::button::ButtonState::{Press, Released};
        assert_eq!(
            buttons,
            [
                (272, Press),
                (272, Released),
                (272, Press),
                (273, Press),
                (274, Press),
                (272, Released),
                (273, Released),
                (274, Released)
            ]
        );
    }

    #[test]
    fn selects_absolute_pointer_over_relative_and_keeps_keyboard() {
        let selection = select_devices(&[
            DeviceOffer {
                absolute_with_region: true,
                button: true,
                scroll: true,
                ..DeviceOffer::default()
            },
            DeviceOffer {
                relative_pointer: true,
                keyboard: true,
                button: true,
                scroll: true,
                ..DeviceOffer::default()
            },
        ]);
        assert_eq!(selection.pointer, Some(0));
        assert_eq!(selection.absolute, Some(0));
        assert_eq!(selection.relative, Some(1));
        assert_eq!(selection.keyboard, Some(1));
        assert_eq!(selection.button, Some(0));
        assert_eq!(selection.scroll, Some(0));
        assert_eq!(
            route_event(selection, InputEvent::PointerRelative { dx: -7, dy: 12 }),
            Some(1),
            "Remote deltas must use the relative device despite absolute preference"
        );
        assert_eq!(
            route_event(
                selection,
                InputEvent::PointerAbsolute(Position::new(1.0, 2.0))
            ),
            Some(0)
        );
    }

    #[test]
    fn relative_pointer_is_fallback_and_keyboard_only_is_degraded() {
        let relative = select_devices(&[
            DeviceOffer {
                relative_pointer: true,
                ..DeviceOffer::default()
            },
            DeviceOffer {
                keyboard: true,
                ..DeviceOffer::default()
            },
        ]);
        assert_eq!(relative.pointer, Some(0));
        assert_eq!(relative.absolute, None);
        assert_eq!(relative.keyboard, Some(1));

        let keyboard_only = select_devices(&[DeviceOffer {
            keyboard: true,
            ..DeviceOffer::default()
        }]);
        assert_eq!(keyboard_only.pointer, None);
        assert_eq!(keyboard_only.keyboard, Some(0));
        let key = InputEvent::Key {
            key: HidKey { page: 7, usage: 4 },
            state: KeyState::Down,
        };
        assert_eq!(route_event(keyboard_only, key), Some(0));
        assert_eq!(
            route_event(
                keyboard_only,
                InputEvent::PointerAbsolute(Position::new(0.0, 0.0))
            ),
            None
        );
    }

    #[test]
    fn routes_each_event_to_its_selected_device() {
        let selection = DeviceSelection {
            absolute: Some(0),
            relative: Some(1),
            pointer: Some(0),
            keyboard: Some(2),
            button: Some(0),
            scroll: Some(1),
        };
        for (event, expected) in [
            (
                InputEvent::PointerAbsolute(Position::new(1.0, 2.0)),
                Some(0),
            ),
            (InputEvent::PointerRelative { dx: 1, dy: 2 }, Some(1)),
            (
                InputEvent::Key {
                    key: HidKey { page: 7, usage: 4 },
                    state: KeyState::Down,
                },
                Some(2),
            ),
            (
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::Down,
                },
                Some(0),
            ),
            (InputEvent::Scroll { h120: 0, v120: 120 }, Some(1)),
        ] {
            assert_eq!(route_event(selection, event), expected);
        }
    }
    #[test]
    fn detents_truncate_toward_zero_and_keep_signed_remainders() {
        for (input, steps, leftover) in [
            (-121, -1, -1),
            (-120, -1, 0),
            (-119, 0, -119),
            (0, 0, 0),
            (119, 0, 119),
            (120, 1, 0),
            (121, 1, 1),
            (i32::MAX, 17895697, 7),
            (i32::MIN, -17895697, -8),
        ] {
            let mut remainder = 0;
            assert_eq!(split_detents(input, &mut remainder), steps);
            assert_eq!(remainder, leftover);
        }
    }

    #[test]
    fn detent_streams_conserve_units_across_direction_changes() {
        let mut remainder = 0;
        let mut input_sum = 0_i64;
        let mut output_sum = 0_i64;
        for delta in [119, 1, -40, -40, -41, 2, 239, -119, -121, 0] {
            input_sum += i64::from(delta);
            output_sum += i64::from(split_detents(delta, &mut remainder)) * 120;
            assert_eq!(output_sum + i64::from(remainder), input_sum);
            assert!(remainder.abs() < 120);
        }
    }

    #[test]
    fn detents_preserve_existing_remainder_at_integer_limits() {
        for (input, previous) in [(i32::MAX, 119), (i32::MIN, -119)] {
            let mut remainder = previous;
            let steps = split_detents(input, &mut remainder);
            assert_eq!(
                i64::from(steps) * 120 + i64::from(remainder),
                i64::from(input) + i64::from(previous)
            );
            assert!(remainder.abs() < 120);
        }
    }
}
