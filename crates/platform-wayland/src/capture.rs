//! Input capture: `org.freedesktop.portal.InputCapture` plus the EIS receiver
//! (KVM-015).
//!
//! Flow (see `docs/03-architecture.md`): `CreateSession2` -> `Start` (the dialog) ->
//! `ConnectToEIS` -> `GetZones` -> [`WaylandCapture::set_barriers`] ->
//! [`WaylandCapture::enable`]; from then on the compositor decides when capture is
//! active and tells us with the `Activated`/`Deactivated`/`Disabled` signals, while
//! the input itself arrives on the EIS file descriptor.
//!
//! [`WaylandCapture::next_event`] merges both channels into one ordered
//! [`CapturedEvent`] stream. Absolute positions are seeded from
//! `Activated.cursor_position` and then integrated from EIS deltas; relative deltas
//! that arrive before any absolute position is known are counted and dropped rather
//! than guessed.
//!
//! `Disable` is deliberately not exposed. Its only use would be to change the pointer
//! barriers, and on GNOME 46 a disabled session can never be re-enabled, so the
//! supported way to change barriers is to drop the session and open a new one — which
//! works on every compositor.

use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, Capabilities, ConnectToEISOptions, CreateSession2Options,
    Deactivated, Disabled, EnableOptions, GetZonesOptions, InputCapture, Region, ReleaseOptions,
    SetPointerBarriersOptions, StartOptions, ZonesChanged,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use futures_util::stream::StreamExt;
use futures_util::Stream;
use input_model::{evdev_to_hid_usage, HidKey, InputEvent, KeyState, MouseButton, Position};
use reis::ei;
use reis::event::{self, DeviceCapability, EiEvent};
use tokio::sync::OnceCell;

use crate::compositor::{compositor_signature, portal_interfaces, Compositor, PortalVersions};
use crate::grant::PortalGrant;
use crate::{map_portal_error, WaylandError, INPUT_CAPTURE_INTERFACE};

/// `CreateSession2`/`Start` require version 2 of the InputCapture interface.
const INPUT_CAPTURE_V2: u32 = 2;

/// Upper bound for the not-yet-delivered event queue; see [`WaylandCapture::next_event`].
const MAX_BUFFERED_EVENTS: usize = 1024;

/// `ei_scroll.scroll` is smooth; `wl_pointer` (and our wire) counts detents of 120
/// units where 10 axis units are one detent.
const SMOOTH_SCROLL_UNITS_PER_DETENT: f64 = 10.0;
const DETENT_UNITS: f64 = 120.0;

type EiStream = Pin<Box<dyn Stream<Item = Result<EiEvent, reis::Error>>>>;
type SignalStream<T> = Pin<Box<dyn Stream<Item = T>>>;

/// Versions of the four portal interfaces the Wayland backend cares about.
///
/// A portal that is not implemented (or whose D-Bus call fails) reports `0`; that is
/// how "GNOME 46 has no `GlobalShortcuts` portal" shows up here.
pub async fn portal_versions() -> PortalVersions {
    let input_capture = probe_version(InputCapture::new().await.map(|portal| portal.version()));
    let remote_desktop = probe_version(
        ashpd::desktop::remote_desktop::RemoteDesktop::new()
            .await
            .map(|portal| portal.version()),
    );
    let clipboard = probe_version(
        ashpd::desktop::clipboard::Clipboard::new()
            .await
            .map(|portal| portal.version()),
    );
    let global_shortcuts = probe_version(
        ashpd::desktop::global_shortcuts::GlobalShortcuts::new()
            .await
            .map(|portal| portal.version()),
    );
    PortalVersions {
        input_capture,
        remote_desktop,
        clipboard,
        global_shortcuts,
    }
}

fn probe_version(probe: Result<u32, ashpd::Error>) -> u32 {
    probe.unwrap_or(0)
}

/// The one `InputCapture` proxy of this process.
///
/// ashpd 0.13's `receive_activated`/`receive_deactivated`/`receive_disabled`/
/// `receive_zones_changed` return `impl Stream` *without* a `+ use<>` capturing bound,
/// so the stream type borrows the portal value it was created from. [`WaylandCapture`]
/// owns those subscriptions, so the portal they borrow has to outlive the capture; a
/// portal proxy is a stateless handle to the D-Bus interface (all state lives in the
/// session object), so one process-wide instance is enough.
fn shared_portal() -> &'static OnceCell<InputCapture> {
    static PORTAL: OnceCell<InputCapture> = OnceCell::const_new();
    &PORTAL
}

async fn input_capture_portal() -> Result<&'static InputCapture, WaylandError> {
    shared_portal()
        .get_or_try_init(|| async {
            InputCapture::new()
                .await
                .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))
        })
        .await
}

/// How many pointer events of each kind the EIS receiver delivered.
///
/// This answers the spike's question 3: whether a capture receiver is fed relative
/// deltas only (counts `relative`), absolute positions only, or a mix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MotionCounts {
    /// `ei_pointer.motion_relative` events.
    pub relative: u64,
    /// `ei_pointer_absolute.motion_absolute` events.
    pub absolute: u64,
}

/// A cloneable trigger for the emergency path (KVM-018).
///
/// The emergency hotkey is handled by the agent itself, which is not the task that
/// pumps [`WaylandCapture::next_event`]; this handle lets that task raise the flag.
#[derive(Debug, Clone)]
pub struct EmergencyHandle(Arc<AtomicBool>);

impl EmergencyHandle {
    /// Raises the emergency flag; the next [`WaylandCapture::next_event`] reports
    /// [`CapturedEvent::Emergency`].
    pub fn trigger(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the flag is currently raised.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One event from the capture side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapturedEvent {
    /// Normalized input captured from the compositor.
    Input(InputEvent),
    /// The compositor handed input to us; input events follow until `Deactivated`.
    Activated {
        /// Activation id to pass back to [`WaylandCapture::release`].
        activation_id: u32,
        /// Where the cursor was when capture started.
        ///
        /// `None` when the portal did not report a position and no absolute position
        /// was known before: relative deltas then cannot be integrated and are
        /// dropped, because inventing an origin would teleport the peer's cursor.
        cursor: Option<Position>,
        /// The barrier that triggered the activation, if the portal identified it.
        barrier_id: Option<u32>,
    },
    /// Capture stopped: either `Deactivated` or `Disabled`.
    ///
    /// The portal's `Disabled` signal has no activation id, so both are reported here;
    /// [`WaylandCapture::is_enabled`] distinguishes them.
    Deactivated,
    /// The set of zones changed; barriers have to be set again for the new zone set.
    ZonesChanged {
        /// The new zone set id.
        zone_set: u32,
    },
    /// The emergency hotkey fired: drop pending input and release the session.
    Emergency,
}

/// An active input capture session.
///
/// At most one capture session may be alive per process: the portal's
/// `Activated`/`Deactivated`/`Disabled`/`ZonesChanged` signals carry the session handle
/// in their body but are subscribed per D-Bus interface, so two live sessions would see
/// each other's signals.
///
/// # GNOME 46
///
/// `Disable` breaks the session and `SetPointerBarriers` is rejected while the session
/// is enabled, so changing barriers needs a new session: drop this value and call
/// [`WaylandCapture::connect`] again with the grant returned by
/// [`WaylandCapture::grant`].
pub struct WaylandCapture {
    /// Process-wide proxy; see [`shared_portal`].
    input_capture: &'static InputCapture,
    session: Session<InputCapture>,
    connection: event::Connection,
    events: EiStream,
    activated: SignalStream<Activated>,
    deactivated: SignalStream<Deactivated>,
    disabled: SignalStream<Disabled>,
    zones_changed: SignalStream<ZonesChanged>,
    zones: Vec<Region>,
    zone_set: u32,
    capabilities: BitFlags<Capabilities>,
    clipboard_enabled: bool,
    compositor: Compositor,
    versions: PortalVersions,
    restore_token: Option<String>,
    enabled: bool,
    active: bool,
    connected: bool,
    activation_id: Option<u32>,
    cursor: Option<Position>,
    buffered: VecDeque<CapturedEvent>,
    buffered_dropped: u64,
    dropped_unseeded_motion: u64,
    unmapped: u64,
    motion: MotionCounts,
    scroll_remainder: (f64, f64),
    ei_error: Option<reis::Error>,
    emergency: Arc<AtomicBool>,
}

/// What one `select!` round produced, before it is applied to `self`.
enum Step {
    Activated {
        activation_id: u32,
        cursor: Option<Position>,
        barrier_id: Option<u32>,
    },
    Deactivated,
    Disabled,
    ZonesChanged(u32),
    Ei(EiEvent),
    End,
}

impl WaylandCapture {
    /// Connects, starts the capture session and subscribes to its signals.
    ///
    /// `grant` is a stored token (KVM-017); it is only sent when it belongs to this
    /// compositor and these portal versions. The session is *not* enabled yet — set
    /// barriers first, then call [`WaylandCapture::enable`].
    pub async fn connect(grant: Option<&PortalGrant>) -> Result<Self, WaylandError> {
        let compositor = Compositor::detect()?;
        let versions = portal_versions().await;
        let signature = compositor_signature(compositor, &versions);

        let input_capture = input_capture_portal().await?;
        let portal_version = input_capture.version();
        if portal_version < INPUT_CAPTURE_V2 {
            return Err(WaylandError::PortalTooOld {
                portal_interface: INPUT_CAPTURE_INTERFACE,
                required: INPUT_CAPTURE_V2,
                found: portal_version,
            });
        }

        let restore_token = grant
            .filter(|grant| {
                grant.portal_interface == INPUT_CAPTURE_INTERFACE
                    && grant.compositor_signature == signature
            })
            .map(|grant| grant.token().to_owned());

        let capabilities = Capabilities::Keyboard | Capabilities::Pointer;
        let session = input_capture
            .create_session2(CreateSession2Options::default())
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;

        let start_options = StartOptions::default()
            .set_capabilities(capabilities)
            .set_restore_token(restore_token.clone())
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        let start = input_capture
            .start(&session, None, start_options)
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?
            .response()
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let granted = start.capabilities();
        let clipboard_enabled = start.is_clipboard_enabled();
        // The token the portal issues *now* is the one worth storing; a token we sent
        // in is not echoed back verbatim, so `restore_token` is only an input.
        let fresh_token = start.restore_token().map(ToOwned::to_owned);

        let fd = input_capture
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        context.flush().map_err(std::io::Error::from)?;
        let (connection, events) = context
            .handshake_tokio("lan-workspace", ei::handshake::ContextType::Receiver)
            .await?;

        let activated = input_capture
            .receive_activated()
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let deactivated = input_capture
            .receive_deactivated()
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let disabled = input_capture
            .receive_disabled()
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let zones_changed = input_capture
            .receive_zones_changed()
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;

        let zones = input_capture
            .zones(&session, GetZonesOptions::default())
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?
            .response()
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        let zone_set = zones.zone_set();

        Ok(Self {
            input_capture,
            session,
            connection,
            events: Box::pin(events),
            activated: Box::pin(activated),
            deactivated: Box::pin(deactivated),
            disabled: Box::pin(disabled),
            zones_changed: Box::pin(zones_changed),
            zones: zones.regions().to_vec(),
            zone_set,
            capabilities: granted,
            clipboard_enabled,
            compositor,
            versions,
            restore_token: fresh_token,
            enabled: false,
            active: false,
            connected: true,
            activation_id: None,
            cursor: None,
            buffered: VecDeque::new(),
            buffered_dropped: 0,
            dropped_unseeded_motion: 0,
            unmapped: 0,
            motion: MotionCounts::default(),
            scroll_remainder: (0.0, 0.0),
            ei_error: None,
            emergency: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Subscribes to the zone set; call after [`CapturedEvent::ZonesChanged`] to get the
    /// new regions and pass the new zone set to [`WaylandCapture::set_barriers`].
    pub async fn refresh_zones(&mut self) -> Result<(), WaylandError> {
        let zones = self
            .input_capture
            .zones(&self.session, GetZonesOptions::default())
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?
            .response()
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        self.zones = zones.regions().to_vec();
        self.zone_set = zones.zone_set();
        Ok(())
    }

    /// Sets the pointer barriers that trigger capture.
    ///
    /// # Errors
    ///
    /// [`WaylandError::BarriersWhileEnabled`] when the session is enabled or active:
    /// GNOME 46 rejects `SetPointerBarriers` on an enabled session and cannot re-enable
    /// a disabled one, so barriers have to be set before [`WaylandCapture::enable`] or
    /// the session has to be recreated.
    ///
    /// On success the returned ids are the barriers the compositor *rejected*.
    pub async fn set_barriers(&mut self, barriers: &[Barrier]) -> Result<Vec<u32>, WaylandError> {
        if self.enabled || self.active {
            return Err(WaylandError::BarriersWhileEnabled);
        }
        let response = self
            .input_capture
            .set_pointer_barriers(
                &self.session,
                barriers,
                self.zone_set,
                SetPointerBarriersOptions::default(),
            )
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?
            .response()
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        Ok(response
            .failed_barriers()
            .iter()
            .map(|id| id.get())
            .collect())
    }

    /// Enables capture; the compositor still decides when it activates.
    pub async fn enable(&mut self) -> Result<(), WaylandError> {
        self.input_capture
            .enable(&self.session, EnableOptions::default())
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        self.enabled = true;
        Ok(())
    }

    /// The next merged portal/EIS event, or `None` when the session ended.
    ///
    /// Events that arrive while capture is not active (the hand-off window between the
    /// compositor activating and us reading the signal) are queued and handed out on
    /// later calls so the caller can replay them locally.
    pub async fn next_event(&mut self) -> Option<CapturedEvent> {
        loop {
            if self.emergency.swap(false, Ordering::SeqCst) {
                return Some(CapturedEvent::Emergency);
            }
            if let Some(event) = self.buffered.pop_front() {
                return Some(event);
            }
            if !self.connected {
                return None;
            }

            let zone_set = self.zone_set;
            let step = tokio::select! {
                Some(activated) = self.activated.next() => Step::Activated {
                    activation_id: activated.activation_id().unwrap_or(0),
                    cursor: activated
                        .cursor_position()
                        .map(|(x, y)| Position::new(f64::from(x), f64::from(y))),
                    barrier_id: match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => Some(id.get()),
                        _ => None,
                    },
                },
                Some(_) = self.deactivated.next() => Step::Deactivated,
                Some(_) = self.disabled.next() => Step::Disabled,
                Some(changed) = self.zones_changed.next() => {
                    Step::ZonesChanged(changed.zone_set().unwrap_or(zone_set))
                }
                event = self.events.next() => match event {
                    Some(Ok(event)) => Step::Ei(event),
                    Some(Err(error)) => {
                        self.ei_error = Some(error);
                        Step::End
                    }
                    None => Step::End,
                },
                else => Step::End,
            };

            match step {
                Step::Activated {
                    activation_id,
                    cursor,
                    barrier_id,
                } => {
                    self.active = true;
                    self.activation_id = Some(activation_id);
                    if cursor.is_some() {
                        self.cursor = cursor;
                    }
                    return Some(CapturedEvent::Activated {
                        activation_id,
                        cursor: cursor.or(self.cursor),
                        barrier_id,
                    });
                }
                Step::Deactivated => {
                    self.active = false;
                    self.activation_id = None;
                    return Some(CapturedEvent::Deactivated);
                }
                Step::Disabled => {
                    self.active = false;
                    self.enabled = false;
                    self.activation_id = None;
                    return Some(CapturedEvent::Deactivated);
                }
                Step::ZonesChanged(zone_set) => {
                    self.zone_set = zone_set;
                    return Some(CapturedEvent::ZonesChanged { zone_set });
                }
                Step::Ei(event) => {
                    if let Some(event) = self.handle_ei_event(event) {
                        if self.active {
                            return Some(event);
                        }
                        self.buffer(event);
                    }
                }
                Step::End => {
                    self.connected = false;
                    self.active = false;
                    return None;
                }
            }
        }
    }

    /// Returns input to the compositor after the peer is done with it.
    ///
    /// `cursor` is where the local cursor should continue; callers usually nudge it
    /// just past the barrier that was crossed.
    pub async fn release(&mut self, cursor: Position) -> Result<(), WaylandError> {
        let activation_id = self.activation_id.ok_or(WaylandError::CaptureNotActive)?;
        self.input_capture
            .release(
                &self.session,
                ReleaseOptions::default()
                    .set_activation_id(activation_id)
                    .set_cursor_position((cursor.x, cursor.y)),
            )
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))?;
        self.active = false;
        self.activation_id = None;
        self.cursor = Some(cursor);
        Ok(())
    }

    /// Ends the session and the EIS connection.
    ///
    /// This is the "agent is shutting down" path; a normal hand-off uses
    /// [`WaylandCapture::release`]. The EIS socket itself is closed when this value is
    /// dropped.
    pub async fn close(&mut self) -> Result<(), WaylandError> {
        self.active = false;
        self.connected = false;
        // Best-effort barrier so the disconnect is not reordered before pending writes;
        // the callback itself is never awaited because the session is over.
        let _callback = self.connection.connection().sync(1);
        let _ = self.connection.flush();
        self.connection.connection().disconnect();
        let _ = self.connection.flush();
        self.session
            .close()
            .await
            .map_err(|error| map_portal_error(INPUT_CAPTURE_INTERFACE, error))
    }

    /// `true` while the compositor is routing input to us.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// `true` while the portal session is enabled (false after a `Disabled` signal).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// `true` while the EIS connection is alive.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// The activation id of the current activation, for [`WaylandCapture::release`].
    #[must_use]
    pub fn activation_id(&self) -> Option<u32> {
        self.activation_id
    }

    /// Last known absolute cursor position in compositor logical coordinates.
    #[must_use]
    pub fn cursor(&self) -> Option<Position> {
        self.cursor
    }

    /// The zones the compositor offers for this session.
    #[must_use]
    pub fn zones(&self) -> &[Region] {
        &self.zones
    }

    /// The zone set id the current zones belong to.
    #[must_use]
    pub fn zone_set(&self) -> u32 {
        self.zone_set
    }

    /// The capabilities the portal granted for this session.
    #[must_use]
    pub fn capabilities(&self) -> BitFlags<Capabilities> {
        self.capabilities
    }

    /// Whether the portal reported the clipboard as enabled (recorded, never driven).
    #[must_use]
    pub fn clipboard_enabled(&self) -> bool {
        self.clipboard_enabled
    }

    /// The compositor this session runs on.
    #[must_use]
    pub fn compositor(&self) -> Compositor {
        self.compositor
    }

    /// The portal versions observed when the session was created.
    #[must_use]
    pub fn versions(&self) -> PortalVersions {
        self.versions
    }

    /// A fresh restore token for this endpoint (KVM-017), if the portal issued one.
    #[must_use]
    pub fn grant(&self) -> Option<PortalGrant> {
        let token = self.restore_token.clone()?;
        Some(PortalGrant::new(
            INPUT_CAPTURE_INTERFACE,
            token,
            compositor_signature(self.compositor, &self.versions),
        ))
    }

    /// The negotiated `ei_connection` protocol version.
    #[must_use]
    pub fn ei_protocol_version(&self) -> Option<u32> {
        let connection = self.connection.connection();
        connection.is_alive().then(|| connection.version())
    }

    /// Relative vs absolute pointer event counts.
    #[must_use]
    pub fn motion_counts(&self) -> MotionCounts {
        self.motion
    }

    /// EIS input events that could not be expressed as a normalized event (evdev codes
    /// the mapping table does not cover, for example multimedia keys).
    #[must_use]
    pub fn unmapped_input_count(&self) -> u64 {
        self.unmapped
    }

    /// Relative pointer events dropped because no absolute position was known yet.
    #[must_use]
    pub fn dropped_unseeded_motion_count(&self) -> u64 {
        self.dropped_unseeded_motion
    }

    /// Queued events dropped because the caller stopped draining.
    #[must_use]
    pub fn buffered_dropped_count(&self) -> u64 {
        self.buffered_dropped
    }

    /// The last EIS error the event stream produced, if any.
    pub fn take_ei_error(&mut self) -> Option<reis::Error> {
        self.ei_error.take()
    }

    /// Raises the emergency flag; the next [`WaylandCapture::next_event`] reports it.
    pub fn notify_emergency(&self) {
        self.emergency.store(true, Ordering::SeqCst);
    }

    /// Whether an emergency was raised and not yet reported.
    #[must_use]
    pub fn emergency_flag(&self) -> bool {
        self.emergency.load(Ordering::SeqCst)
    }

    /// A handle other tasks can use to raise the emergency flag.
    #[must_use]
    pub fn emergency_handle(&self) -> EmergencyHandle {
        EmergencyHandle(Arc::clone(&self.emergency))
    }

    /// Throws away queued events (the emergency path does not replay stale input).
    ///
    /// Returns how many events were dropped.
    pub fn discard_buffer(&mut self) -> usize {
        let dropped = self.buffered.len();
        self.buffered.clear();
        dropped
    }

    /// The portal interfaces available at the versions recorded for this session.
    ///
    /// This is the capture side's contribution to `docs/ipc.proto`'s `BackendInfo`;
    /// [`crate::WaylandInjector::backend_info`] is the authoritative source for
    /// `has_absolute_injection`, because only the EIS sender knows whether a region was
    /// established.
    #[must_use]
    pub fn available_portals(&self) -> Vec<&'static str> {
        portal_interfaces(&self.versions)
    }

    fn buffer(&mut self, event: CapturedEvent) {
        if self.buffered.len() >= MAX_BUFFERED_EVENTS {
            self.buffered.pop_front();
            self.buffered_dropped += 1;
        }
        self.buffered.push_back(event);
    }

    /// Turns one EIS event into a normalized event, updating the tracked cursor.
    fn handle_ei_event(&mut self, event: EiEvent) -> Option<CapturedEvent> {
        match event {
            EiEvent::SeatAdded(seat_added) => {
                // Must be answered, otherwise the compositor never advertises a device.
                seat_added.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
                let _ = self.connection.flush();
                None
            }
            EiEvent::PointerMotion(motion) => {
                self.motion.relative += 1;
                let Some(cursor) = self.cursor else {
                    self.dropped_unseeded_motion += 1;
                    return None;
                };
                let cursor = Position::new(
                    cursor.x + f64::from(motion.dx),
                    cursor.y + f64::from(motion.dy),
                );
                self.cursor = Some(cursor);
                Some(CapturedEvent::Input(InputEvent::PointerAbsolute(cursor)))
            }
            EiEvent::PointerMotionAbsolute(motion) => {
                self.motion.absolute += 1;
                let cursor =
                    Position::new(f64::from(motion.dx_absolute), f64::from(motion.dy_absolute));
                self.cursor = Some(cursor);
                Some(CapturedEvent::Input(InputEvent::PointerAbsolute(cursor)))
            }
            EiEvent::KeyboardKey(key) => {
                match u16::try_from(key.key).ok().and_then(evdev_to_hid_usage) {
                    Some(usage) => Some(CapturedEvent::Input(InputEvent::Key {
                        key: HidKey::keyboard(usage),
                        state: key_state(key.state),
                    })),
                    None => {
                        self.unmapped += 1;
                        None
                    }
                }
            }
            EiEvent::Button(button) => match u16::try_from(button.button)
                .ok()
                .and_then(MouseButton::from_evdev_code)
            {
                Some(kind) => Some(CapturedEvent::Input(InputEvent::Button {
                    button: kind,
                    state: button_state(button.state),
                })),
                None => {
                    self.unmapped += 1;
                    None
                }
            },
            EiEvent::ScrollDiscrete(scroll) => Some(CapturedEvent::Input(InputEvent::Scroll {
                h120: scroll.discrete_dx.saturating_mul(DETENT_UNITS as i32),
                v120: scroll.discrete_dy.saturating_mul(DETENT_UNITS as i32),
            })),
            EiEvent::ScrollDelta(delta) => self.smooth_scroll(delta.dx, delta.dy),
            // Frames, emulation markers, modifiers and touch/text events do not map onto
            // the product's normalized event set.
            _ => None,
        }
    }

    /// Converts pixel-precise scroll into 120ths of a detent, keeping the remainder
    /// (and the direction) for the next event.
    fn smooth_scroll(&mut self, dx: f32, dy: f32) -> Option<CapturedEvent> {
        let (mut rem_h, mut rem_v) = self.scroll_remainder;
        rem_h += f64::from(dx) * DETENT_UNITS / SMOOTH_SCROLL_UNITS_PER_DETENT;
        rem_v += f64::from(dy) * DETENT_UNITS / SMOOTH_SCROLL_UNITS_PER_DETENT;
        let units_h = rem_h.trunc();
        let units_v = rem_v.trunc();
        rem_h -= units_h;
        rem_v -= units_v;
        self.scroll_remainder = (rem_h, rem_v);
        if units_h == 0.0 && units_v == 0.0 {
            return None;
        }
        Some(CapturedEvent::Input(InputEvent::Scroll {
            h120: clamp_h120(units_h),
            v120: clamp_h120(units_v),
        }))
    }
}

/// Clamps a 120th-of-a-detent count into `i32`.
fn clamp_h120(units: f64) -> i32 {
    units.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

fn key_state(state: ei::keyboard::KeyState) -> KeyState {
    match state {
        ei::keyboard::KeyState::Press => KeyState::Down,
        ei::keyboard::KeyState::Released => KeyState::Up,
    }
}

fn button_state(state: ei::button::ButtonState) -> KeyState {
    match state {
        ei::button::ButtonState::Press => KeyState::Down,
        ei::button::ButtonState::Released => KeyState::Up,
    }
}
