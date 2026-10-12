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

/// An EIS sender connected to a compositor.
///
/// Call [`WaylandInjector::disconnect`] before dropping: it releases the pressed-input
/// ledger and closes the portal session.
pub struct WaylandInjector {
    session: Option<Session<RemoteDesktop>>,
    connection: event::Connection,
    events: EiStream,
    device: event::Device,
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
    last_serial: u32,
    sequence: u32,
    emulating: bool,
    resumed: bool,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    scroll_remainder: (i32, i32),
    cursor: Option<Position>,
    started: Instant,
    ei_error: Option<reis::Error>,
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
        let mut last_serial = connection.serial();
        let mut device: Option<event::Device> = None;

        let handshake = tokio::time::timeout(DEVICE_RESUME_TIMEOUT, async {
            loop {
                let Some(event) = events.next().await else {
                    return Err(WaylandError::EiDisconnected);
                };
                match event? {
                    EiEvent::SeatAdded(seat_added) => {
                        tracing::debug!(
                            seat = ?seat_added.seat.name(),
                            requested_capabilities = ?seat_capabilities(),
                            "EIS seat advertised; binding requested capabilities"
                        );
                        // Answering this is what makes the server advertise a device.
                        seat_added.seat.bind_capabilities(seat_capabilities());
                        connection.flush().map_err(std::io::Error::from)?;
                    }
                    EiEvent::DeviceAdded(added) => {
                        // `DeviceAdded` is queued when `ei_device.done` arrives; its
                        // interfaces, dimensions, and regions are complete at this point.
                        log_device("done", &added.device);
                        declare_device_ready(&added.device);
                        device = Some(added.device);
                    }
                    EiEvent::DeviceResumed(resumed) => {
                        // Nothing may be emulated before this serial. Only a resumed
                        // device may start an emulation sequence.
                        log_device("resumed", &resumed.device);
                        device = Some(resumed.device.clone());
                        last_serial = resumed.serial;
                        break;
                    }
                    EiEvent::Disconnected(_) => return Err(WaylandError::EiDisconnected),
                    _ => {}
                }
            }
            Ok(())
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

        let device = device.ok_or(WaylandError::NoEiDevice)?;
        let injector = Self {
            session: portal_session,
            connection,
            events: Box::pin(events),
            device_name: device.name().map(ToOwned::to_owned),
            device_type: device.device_type(),
            keyboard: device.interface::<ei::Keyboard>(),
            pointer: device.interface::<ei::Pointer>(),
            pointer_absolute: device.interface::<ei::PointerAbsolute>(),
            button: device.interface::<ei::Button>(),
            scroll: device.interface::<ei::Scroll>(),
            regions: device.regions().iter().map(Region::from_ei).collect(),
            dimensions: device.dimensions(),
            device,
            source,
            compositor,
            versions,
            restore_token,
            last_serial,
            sequence: 0,
            emulating: false,
            resumed: true,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            scroll_remainder: (0, 0),
            cursor: None,
            started: Instant::now(),
            ei_error: None,
        };
        tracing::info!(
            device = ?injector.device_name,
            device_type = ?injector.device_type,
            capabilities = ?injector.capabilities(),
            regions = ?injector.regions,
            "EIS injection device ready"
        );
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
        if !self.resumed {
            outcome.os_error = Some(WaylandError::DevicePaused);
            return outcome;
        }
        if !self.emulating {
            // A new sequence number per emulation run; it must strictly increase.
            self.sequence = self.sequence.wrapping_add(1);
            self.device
                .device()
                .start_emulating(self.last_serial, self.sequence);
            self.emulating = true;
        }
        for event in events {
            match self.apply(*event) {
                Ok(actual) => {
                    outcome.applied_count += 1;
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
        outcome
    }

    /// Releases every key and button this injector still holds down.
    ///
    /// A paused device is neutral by protocol, so the ledger is simply dropped then.
    pub fn release_all(&mut self) -> InjectOutcome {
        let mut outcome = InjectOutcome::ok();
        let keyboard = self.keyboard.clone();
        let button = self.button.clone();
        if self.emulating {
            let keys: Vec<u16> = self.pressed_keys.iter().copied().collect();
            for code in keys {
                if let Some(keyboard) = keyboard.as_ref() {
                    keyboard.key(u32::from(code), ei::keyboard::KeyState::Released);
                }
                self.pressed_keys.remove(&code);
                self.commit_frame();
                outcome.applied_count += 1;
            }
            let buttons: Vec<u16> = self.pressed_buttons.iter().copied().collect();
            for code in buttons {
                if let Some(button) = button.as_ref() {
                    button.button(u32::from(code), ei::button::ButtonState::Released);
                }
                self.pressed_buttons.remove(&code);
                self.commit_frame();
                outcome.applied_count += 1;
            }
            if let Err(error) = self.connection.flush() {
                outcome.os_error = Some(WaylandError::Io(error.into()));
            }
        } else {
            self.pressed_keys.clear();
            self.pressed_buttons.clear();
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
        if self.emulating {
            self.device.device().stop_emulating(self.last_serial);
            self.emulating = false;
        }
        let _ = self.connection.flush();
        // Best effort write barrier; the callback is never awaited because the
        // connection is closed immediately afterwards.
        let _callback = self.connection.connection().sync(1);
        let _ = self.connection.flush();
        self.connection.connection().disconnect();
        let _ = self.connection.flush();
        self.resumed = false;
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
            has_absolute_injection: self.pointer_absolute.is_some() && !self.regions.is_empty(),
            has_relative_injection: self.pointer.is_some(),
            has_keyboard: self.keyboard.is_some(),
            has_buttons: self.button.is_some(),
            has_scroll: self.scroll.is_some(),
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

    /// The last serial the EIS server sent us.
    #[must_use]
    pub fn last_serial(&self) -> u32 {
        self.last_serial
    }

    /// Whether the device is resumed, i.e. whether input may be emulated.
    #[must_use]
    pub fn is_resumed(&self) -> bool {
        self.resumed
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
                self.adopt_device(&added.device);
            }
            EiEvent::DeviceResumed(resumed) => {
                log_device("resumed", &resumed.device);
                self.last_serial = resumed.serial;
                self.resumed = true;
                // A resume starts a new emulation sequence; the next inject() starts it.
                self.emulating = false;
            }
            EiEvent::DevicePaused(_) | EiEvent::DeviceRemoved(_) | EiEvent::Disconnected(_) => {
                // Pausing resets the device's logical state: the server releases keys and
                // buttons, so our ledger must not claim them any more.
                self.resumed = false;
                self.emulating = false;
                self.pressed_keys.clear();
                self.pressed_buttons.clear();
                self.scroll_remainder = (0, 0);
            }
            _ => {}
        }
    }

    /// Re-reads the interfaces and regions of a (possibly replaced) device.
    fn adopt_device(&mut self, device: &event::Device) {
        self.device_name = device.name().map(ToOwned::to_owned);
        self.device_type = device.device_type();
        self.keyboard = device.interface::<ei::Keyboard>();
        self.pointer = device.interface::<ei::Pointer>();
        self.pointer_absolute = device.interface::<ei::PointerAbsolute>();
        self.button = device.interface::<ei::Button>();
        self.scroll = device.interface::<ei::Scroll>();
        self.regions = device.regions().iter().map(Region::from_ei).collect();
        self.dimensions = device.dimensions();
        self.device = device.clone();
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
        self.device
            .device()
            .frame(self.last_serial, self.timestamp_us());
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
    use super::split_detents;

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
