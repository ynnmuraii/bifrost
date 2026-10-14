//! `platform-wayland` backend: XDG-portal `InputCapture` for capture and
//! `RemoteDesktop` + EIS for injection (KVM-026/KVM-027).
//!
//! `docs/03-architecture.md` («Linux Wayland (GNOME и KDE через XDG-порталы)») is the
//! contract this module implements; the platform crate `platform-wayland` owns the
//! D-Bus and EIS details, and this module only decides *when* to call it and how to
//! map its events onto the agent's vocabulary.
//!
//! # Two sessions, two roles
//!
//! * **inject** (`RemoteDesktop`): connected first and kept for the whole run. A
//!   receiver needs it to inject, and a source needs it for the local replay of a
//!   cancelled transition — «Инъекционная сессия нужна и источнику — ей проигрывается
//!   буфер отменённого перехода».
//! * **capture** (`InputCapture`): armed with barriers on the outer zone edges and
//!   `Enable`d. The *compositor* decides when it becomes active («Активирует захват
//!   композитор, когда указатель доходит до барьера: `Activated(activation_id,
//!   cursor_position, barrier_id)`. Запросить активацию немедленно приложение не
//!   может»), so `begin_capture` never starts suppression by itself: it checks that
//!   the session is already active and only decides what to do with the events.
//!
//! # What this slice does and does not do
//!
//! * Events captured while a transition is being prepared are buffered; cancelling
//!   the transition replays them locally through the inject session, so «нельзя
//!   «потерять» нажатие в Preparing» holds on Wayland too.
//! * A `ZonesChanged` signal attempts to re-arm barriers. Enabled barriers are retained;
//!   a refused re-arm is reported without recreating the session or opening a new dialog.
//! * `ZonesChanged` is *not* turned into `DisplaysChanged`: display enumeration is
//!   KVM-040 and `QueryDisplays` is answered with an explicit refusal by the session
//!   loop.
//! * With absolute EIS regions, `EnterPrepare` positions are resolved by
//!   `platform-wayland` and out-of-region points are refused. Relative-only servers use
//!   the local logical-zone bounds to estimate cursor position; edge clamping is assumed,
//!   and exact placement cannot be guaranteed if the compositor behaves differently.
//! * `RECEIVER_TAKEOVER` is never advertised (a portal receiver cannot see physical
//!   input) and `EMERGENCY_HOTKEY` only when `GlobalShortcuts` registered the
//!   combination.

use std::collections::{HashSet, VecDeque};

use crate::backend::wayland_barriers::{self, BarrierSegment, Zone};
use crate::backend::{ClipboardChange, Event, Injected, Ready, Refusal, Released, POLL_TICK};
use input_model::{CaptureKind, HidKey, InputEvent, KeyState, MouseButton, Position, RepeatFilter};
use platform_probe::SessionInfo;
use platform_wayland::{
    compositor_signature, portal_versions, select_pointer_mode, Barrier, BarrierID,
    BarrierPosition, CapturedEvent, Compositor, EmergencyShortcuts, GrantStore, PointerMode,
    PortalGrant, RelativeBounds, RelativeCursor, WaylandCapture, WaylandError, WaylandInjector,
    INPUT_CAPTURE_INTERFACE, REMOTE_DESKTOP_INTERFACE,
};

/// Which role the agent currently plays on this desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Local: the session is armed, nothing is suppressed and nothing is forwarded.
    Observe,
    /// Preparing: the compositor diverted input to us and core has not committed yet.
    Temporary,
    /// Remote: the lease is active and captured input goes to the peer.
    Remote,
}

/// The activation the compositor granted, kept until `Release`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Activation {
    id: u32,
    cursor: Option<Position>,
    barrier_id: Option<u32>,
}

fn portal_activation(
    cursor: Option<Position>,
    barrier_id: Option<u32>,
    barriers: &[BarrierSegment],
) -> protocol::ipc::PortalActivation {
    use protocol::ipc::{PointerAbs, PortalActivation, PortalEdge};
    let edge = match barrier_id.and_then(|id| wayland_barriers::locate(barriers, id)) {
        Some((_, wayland_barriers::Edge::Left)) => PortalEdge::Left,
        Some((_, wayland_barriers::Edge::Right)) => PortalEdge::Right,
        Some((_, wayland_barriers::Edge::Top)) => PortalEdge::Top,
        Some((_, wayland_barriers::Edge::Bottom)) => PortalEdge::Bottom,
        None => PortalEdge::Unspecified,
    };
    PortalActivation {
        position: cursor.map(|position| PointerAbs {
            display_id: String::new(),
            x: position.x,
            y: position.y,
        }),
        edge: edge as i32,
    }
}

/// The Wayland input path.
pub struct Wayland {
    ready: Ready,
    /// `None` while no usable capture session exists (never connected or closed).
    capture: Option<WaylandCapture>,
    injector: WaylandInjector,
    /// `None` when this compositor has no `GlobalShortcuts` portal (GNOME 46).
    shortcuts: Option<EmergencyShortcuts>,
    /// Restore tokens handed over by core in `AgentAttach`; kept in memory, because
    /// `docs/03-architecture.md` has *core* own `portal-restore.toml`.
    grants: GrantStore,
    mode: Mode,
    /// Receiver injection owns its ledger until ReleaseSession.
    receiving: bool,
    activation: Option<Activation>,
    /// The barriers currently registered, so an activation id can be traced back.
    barriers: Vec<BarrierSegment>,
    /// Local pointer estimate for a relative-only EIS server.
    relative_cursor: Option<RelativeCursor>,
    /// Events the compositor handed us that are not (yet) on the wire.
    buffered: Vec<InputEvent>,
    /// Events already mapped to the agent vocabulary, waiting for `next_event`.
    pending: VecDeque<Event>,
    pressed_keys: HashSet<(u16, u16)>,
    pressed_buttons: HashSet<MouseButton>,
    /// Source-side autorepeat filter (KVM-036): EIS repeats a held key on the compositor's
    /// own schedule, and those repeats must not reach the wire.
    repeats: RepeatFilter,
}

impl Wayland {
    /// Connects the inject session (fatal if it fails) and arms the capture session
    /// (degradable), then reports what the portals actually allow.
    ///
    /// `restore_tokens` are the ones core persisted; a token is only used when it
    /// belongs to this compositor and these portal versions.
    pub async fn connect(
        session: &SessionInfo,
        emergency_hotkey: &str,
        restore_tokens: &[protocol::ipc::PortalGrant],
        portal_progress: &tokio::sync::mpsc::UnboundedSender<protocol::ipc::agent_frame::Msg>,
    ) -> Result<(Self, Ready, Vec<Refusal>), Refusal> {
        let mut refusals = Vec::new();

        let compositor = Compositor::detect().map_err(|error| fatal(&error))?;
        let versions = portal_versions().await;
        let signature = compositor_signature(compositor, &versions);

        // A token is only usable for the compositor and portal versions it was issued
        // for (`docs/03-architecture.md`: «при смене композитора или несовпадении
        // `compositor_signature` токен отбрасывается, а не переиспользуется»).
        let mut grants = GrantStore::new();
        for token in restore_tokens {
            grants.accept(
                PortalGrant::new(
                    token.portal_interface.clone(),
                    token.restore_token.clone(),
                    token.compositor_signature.clone(),
                ),
                &signature,
            );
        }

        // Injection first: a source needs it too, for the local replay, and a node
        // that cannot inject cannot be part of a lease in either role.
        let inject_grant = grants.get(REMOTE_DESKTOP_INTERFACE, &signature).cloned();
        let _ = portal_progress.send(crate::backend::portal_progress_message(
            REMOTE_DESKTOP_INTERFACE,
            "inject",
            inject_grant.is_some(),
        ));
        let mut injector = WaylandInjector::connect(inject_grant.as_ref())
            .await
            .map_err(|error| fatal(&error))?;
        let injection_capabilities = injector.capabilities();
        if !injection_capabilities.has_absolute_injection
            && !injection_capabilities.has_relative_injection
        {
            let _ = injector.disconnect().await;
            return Err(fatal(&WaylandError::NoPointerDevice));
        }
        // The emergency hotkey is registered for the whole run (KVM-018); its absence
        // is the documented reason a GNOME 46 node cannot be a source.
        let shortcuts = match EmergencyShortcuts::register(emergency_hotkey).await {
            Ok(shortcuts) => Some(shortcuts),
            Err(error) => {
                refusals.push(Refusal {
                    code: "PORTAL_UNAVAILABLE",
                    detail: format!(
                        "emergency hotkey {emergency_hotkey:?} is not available: {error}; \
                         this node cannot be the source while require_emergency_hotkey is true"
                    ),
                });
                None
            }
        };

        // Capture: degradable. A denied dialog is reported and names its interface,
        // which is what the GUI needs (scenario S8).
        let capture_grant = grants.get(INPUT_CAPTURE_INTERFACE, &signature).cloned();
        let _ = portal_progress.send(crate::backend::portal_progress_message(
            INPUT_CAPTURE_INTERFACE,
            "capture",
            capture_grant.is_some(),
        ));
        let mut capture = match WaylandCapture::connect(capture_grant.as_ref()).await {
            Ok(capture) => Some(capture),
            Err(error) => {
                refusals.push(Refusal {
                    code: portal_code(&error),
                    detail: format!(
                        "portal {INPUT_CAPTURE_INTERFACE} is not usable here: {}",
                        error_chain(&error)
                    ),
                });
                None
            }
        };

        let mut barriers = Vec::new();
        let mut armed = false;
        if let Some(session) = capture.as_mut() {
            match arm_capture(session, &mut barriers).await {
                Ok(()) => armed = true,
                Err(error) => refusals.push(Refusal {
                    code: portal_code(&error),
                    detail: format!("cannot arm the capture session: {}", error_chain(&error)),
                }),
            }
        }
        if !armed {
            // A session that cannot be armed cannot produce activations; drop it rather
            // than leave a half-armed one behind.
            capture = None;
        }

        // New tokens are core's to persist: `AgentAttach.restore_tokens` arrive from
        // it and `PortalGrantChanged` goes back, so `portal-restore.toml` stays core's
        // file (the agent never reads the configuration).
        let mut pending = VecDeque::new();
        let mut reported: Vec<PortalGrant> = Vec::new();
        if let Some(capture) = capture.as_ref() {
            reported.extend(capture.grant());
        }
        reported.extend(injector.grant());
        for grant in reported {
            grants.accept(grant.clone(), &signature);
            pending.push_back(Event::PortalGrant(protocol::ipc::PortalGrant {
                portal_interface: grant.portal_interface.clone(),
                restore_token: grant.restore_token.clone(),
                compositor_signature: grant.compositor_signature.clone(),
            }));
        }

        let ready = refine_ready(
            session,
            capture.is_some(),
            &injector,
            shortcuts.is_some(),
            &refusals,
        );
        let wayland = Wayland {
            ready: ready.clone(),
            capture,
            injector,
            shortcuts,
            grants,
            mode: Mode::Observe,
            receiving: false,
            activation: None,
            barriers,
            relative_cursor: None,
            buffered: Vec::new(),
            pending,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            repeats: RepeatFilter::new(),
        };
        Ok((wayland, ready, refusals))
    }

    /// The capability report sent right after the attach.
    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    /// Whether this backend can own and read the local CLIPBOARD selection.
    ///
    /// It cannot, and it does not pretend to: `crates/platform-wayland/src/clipboard.rs` has no
    /// portal clipboard in this build (the `Clipboard` interface of `RemoteDesktop` is not
    /// wired), so `TEXT_CLIPBOARD` is not advertised (`refine_ready` strips it) and core never
    /// hands this node a payload. ADR-008 wants the feature to be *off* on a node that cannot
    /// honour it, not half-wired.
    pub fn clipboard_available(&self) -> bool {
        false
    }

    /// Never: a Wayland session has no local selection this agent can watch.
    pub async fn clipboard_change(&mut self) -> Option<ClipboardChange> {
        None
    }

    /// Refused with `CLIPBOARD_DISABLED`: the portal clipboard is not implemented in this
    /// build, and core is entitled to an answer rather than silence.
    pub async fn set_clipboard(&mut self, _text: Option<&str>) -> Result<(), String> {
        Err("CLIPBOARD_DISABLED: the portal clipboard is not implemented in this build".to_owned())
    }

    /// Starts (or re-arms) capture for the requested kind.
    ///
    /// `OBSERVE` only makes sure the session is armed; `TEMPORARY` and `REMOTE` require
    /// an activation the compositor already granted, because a Wayland source cannot
    /// suppress input on its own.
    pub async fn begin_capture(&mut self, kind: CaptureKind) -> Result<(), String> {
        match kind {
            CaptureKind::Observe => {
                if self.capture.is_none() {
                    return Err(
                        "the Wayland capture session is not available on this compositor"
                            .to_owned(),
                    );
                }
                self.mode = Mode::Observe;
                self.buffered.clear();
                self.pending.clear();
                Ok(())
            }
            CaptureKind::Temporary | CaptureKind::Remote => {
                let active = self
                    .capture
                    .as_ref()
                    .is_some_and(|capture| capture.is_active());
                if !active {
                    return Err(
                        "the compositor has not activated the Wayland capture; a portal source \
                         cannot suppress input on demand (activation happens when the pointer \
                         reaches a barrier)"
                            .to_owned(),
                    );
                }
                self.mode = if kind == CaptureKind::Temporary {
                    Mode::Temporary
                } else {
                    // A committed transition forwards what was buffered while
                    // preparing; dropping it would lose the press that started it.
                    for event in std::mem::take(&mut self.buffered) {
                        self.pending.push_back(Event::Input(event));
                    }
                    Mode::Remote
                };
                Ok(())
            }
        }
    }

    /// Ends capture. `replay_locally` plays buffered events through the inject session
    /// instead of forwarding them, which is the documented cancellation path.
    pub async fn end_capture(&mut self, replay_locally: bool) -> Result<Released, String> {
        if replay_locally && !self.buffered.is_empty() {
            let buffered: Vec<InputEvent> = std::mem::take(&mut self.buffered);
            let outcome = self.inject(&buffered).await;
            let replayed = outcome.applied;
            if !outcome.os_error.is_empty() {
                tracing::warn!(error = %outcome.os_error, "the local replay of the buffered edge failed");
            }
            tracing::info!(events = replayed, "replayed the cancelled edge locally");
        }
        // Source cancellation replay must not leave synthetic downs held, even
        // before BeginCapture. A receiver's unrelated injection lease stays intact.
        let released = if self.receiving {
            Released::default()
        } else {
            self.release_session()
        };
        self.buffered.clear();
        self.pending.clear();
        // The keys held now are released locally; their ups never reach the filter, and a
        // stale pressed set would eat the next real press (KVM-036).
        self.repeats.clear();
        self.mode = Mode::Observe;

        if let Some(capture) = self.capture.as_mut() {
            if capture.is_active() {
                // Return to the local activation position, not the remote integrated cursor.
                let cursor = self.activation.and_then(|activation| activation.cursor);
                if let Err(error) = capture.release(cursor).await {
                    tracing::warn!(%error, "releasing the Wayland capture failed");
                    if let Err(close_error) = capture.close().await {
                        tracing::warn!(%close_error, "closing failed Wayland capture session");
                    }
                    self.capture = None;
                    self.activation = None;
                    return Err(format!("Release of the capture session failed: {error}"));
                }
            }
        }
        self.activation = None;
        Ok(released)
    }

    /// The portal zones as displays (KVM-040).
    ///
    /// `docs/03-architecture.md`, «Дисплеи и координаты»: «Порталы не отдают физическое
    /// разрешение. Набор дисплеев строится по зонам (логические границы)», so the zones
    /// are the display set and the native bounds stay unknown (`native_known = false`).
    pub async fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        let Some(capture) = self.capture.as_ref() else {
            return Vec::new();
        };
        let zones = zones_of(capture);
        if zones.is_empty() {
            return Vec::new();
        }
        // The first zone is the one the pointer starts in; without a better signal it is
        // reported as primary, which only decides where the node logical origin sits.
        zones
            .iter()
            .enumerate()
            .map(|(index, zone)| {
                crate::displays::RawDisplay::without_native(
                    format!("zone-{index}"),
                    [
                        f64::from(zone.x),
                        f64::from(zone.y),
                        f64::from(zone.width),
                        f64::from(zone.height),
                    ],
                    index == 0,
                )
            })
            .collect()
    }

    /// `PrepareInject` clears the relative estimate; positioning is deferred until the
    /// first pointer event, so an aborted prepare does not move the local cursor.
    pub async fn prepare_inject(&mut self) -> Result<(), String> {
        self.injector.pump().await;
        self.injector.reset_applied_edges();
        self.relative_cursor = None;
        self.receiving = true;
        Ok(())
    }

    pub fn applied_edges(&self) -> input_model::EdgeCounters {
        self.injector.applied_edges()
    }
    fn initialize_relative_cursor(&mut self) {
        if select_pointer_mode(self.injector.capabilities()) != PointerMode::Relative {
            return;
        }
        let bounds = self.capture.as_ref().and_then(relative_bounds_of);
        let Some(bounds) = bounds else {
            tracing::warn!(
                "EIS has relative pointer but no logical capture bounds; keyboard, buttons \
                 and scroll remain available, pointer motion will be refused"
            );
            return;
        };
        let (cursor, reset) = RelativeCursor::enter(bounds);
        let outcome = self.injector.inject(&[InputEvent::PointerRelative {
            dx: reset.dx,
            dy: reset.dy,
        }]);
        if let Some(error) = outcome.os_error {
            tracing::warn!(%error, "could not reset relative pointer estimate at entry");
            return;
        }
        self.relative_cursor = Some(cursor);
        tracing::info!(
            ?bounds,
            "using relative EIS pointer injection; cursor position is an estimate"
        );
    }

    /// Injects one batch through EIS.
    ///
    /// Absolute source positions are converted to clamped relative deltas only when the
    /// compositor has no absolute region; keyboard, button, and scroll events are unchanged.
    pub async fn inject(&mut self, events: &[InputEvent]) -> Injected {
        self.injector.pump().await;
        let relative_only =
            select_pointer_mode(self.injector.capabilities()) == PointerMode::Relative;
        if relative_only
            && events
                .iter()
                .any(|event| matches!(event, InputEvent::PointerAbsolute(_)))
        {
            self.initialize_relative_cursor();
        }
        let mapped: Vec<InputEvent> = events
            .iter()
            .map(|event| match event {
                InputEvent::PointerAbsolute(position) if relative_only => self
                    .relative_cursor
                    .as_mut()
                    .map(|cursor| {
                        let motion = cursor.move_to(*position);
                        InputEvent::PointerRelative {
                            dx: motion.dx,
                            dy: motion.dy,
                        }
                    })
                    .unwrap_or(*event),
                _ => *event,
            })
            .collect();
        let outcome = self.injector.inject(&mapped);
        let relative_actual = if outcome.os_error.is_none()
            && events
                .iter()
                .take(outcome.applied_count)
                .any(|event| matches!(event, InputEvent::PointerAbsolute(_)))
        {
            self.relative_cursor.map(|cursor| cursor.position())
        } else {
            None
        };
        let actual = outcome.actual.or(relative_actual);
        for event in events.iter().take(outcome.applied_count) {
            match event {
                InputEvent::Key { key, state } => {
                    if state.is_down() {
                        self.pressed_keys.insert((key.page, key.usage));
                    } else {
                        self.pressed_keys.remove(&(key.page, key.usage));
                    }
                }
                InputEvent::Button { button, state } => {
                    if state.is_down() {
                        self.pressed_buttons.insert(*button);
                    } else {
                        self.pressed_buttons.remove(button);
                    }
                }
                InputEvent::PointerAbsolute(_)
                | InputEvent::PointerRelative { .. }
                | InputEvent::Scroll { .. } => {}
            }
        }
        Injected {
            applied: outcome.applied_count,
            os_error: outcome
                .os_error
                .map(|error| error.to_string())
                .unwrap_or_default(),
            actual,
        }
    }

    /// Releases everything this session emulated, which is what a receiver must do when
    /// it loses the lease.
    pub fn release_session(&mut self) -> Released {
        self.receiving = false;
        let outcome = self.injector.release_all();
        let released = Released {
            keys: self.pressed_keys.len() as u32,
            buttons: self.pressed_buttons.len() as u32,
        };
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        if let Some(error) = outcome.os_error {
            tracing::warn!(%error, "releasing the emulated devices failed");
        }
        released
    }

    /// `RECOVERY` (rule 7): release exactly these keys and buttons.
    ///
    /// The counts come from what the EIS socket accepted, not from the request, so a
    /// paused device is visible instead of being reported as released.
    pub async fn release_keys(&mut self, keys: &[HidKey], buttons: &[MouseButton]) -> Released {
        let mut events = Vec::with_capacity(keys.len() + buttons.len());
        for key in keys {
            events.push(InputEvent::Key {
                key: *key,
                state: KeyState::Up,
            });
        }
        for button in buttons {
            events.push(InputEvent::Button {
                button: *button,
                state: KeyState::Up,
            });
        }
        let applied = self.inject(&events).await.applied;
        let key_total = keys.len().min(applied) as u32;
        let button_total = applied.saturating_sub(keys.len()) as u32;
        for key in keys {
            self.pressed_keys.remove(&(key.page, key.usage));
        }
        for button in buttons {
            self.pressed_buttons.remove(button);
        }
        Released {
            keys: key_total,
            buttons: button_total,
        }
    }

    /// `ForgetPortalGrants`: drop the in-memory tokens so the next session asks again.
    pub fn forget_portal_grants(&mut self) {
        self.grants.clear();
        tracing::info!("portal restore tokens dropped; the next session will ask again");
    }

    /// The next thing the session loop should act on.
    pub async fn next_event(&mut self) -> Option<Event> {
        // Anything already mapped outranks a new portal event: it is older.
        if let Some(event) = self.pending.pop_front() {
            return Some(event);
        }
        // The EIS stream carries resumptions and the cursor; it must not stall.
        self.injector.pump().await;

        let Wayland {
            capture, shortcuts, ..
        } = self;
        let shortcut = async {
            match shortcuts.as_mut() {
                Some(shortcuts) => shortcuts.next_activation().await,
                None => std::future::pending().await,
            }
        };

        enum Step {
            Captured(Option<CapturedEvent>),
            Shortcut,
            Tick,
        }

        let step = match capture {
            Some(capture) => tokio::select! {
                biased;
                event = capture.next_event() => Step::Captured(event),
                _ = shortcut => Step::Shortcut,
                () = tokio::time::sleep(POLL_TICK) => Step::Tick,
            },
            None => tokio::select! {
                biased;
                _ = shortcut => Step::Shortcut,
                () = tokio::time::sleep(POLL_TICK) => Step::Tick,
            },
        };

        match step {
            Step::Tick => None,
            Step::Shortcut => {
                if let Some(capture) = self.capture.as_ref() {
                    capture.notify_emergency();
                    tracing::warn!("emergency shortcut fired; releasing the capture locally");
                }
                None
            }
            Step::Captured(None) => {
                // The portal session ended (the compositor closed or disabled it).
                self.capture = None;
                if self.mode == Mode::Observe {
                    tracing::info!("the Wayland capture session ended");
                    None
                } else {
                    self.mode = Mode::Observe;
                    Some(Event::Failed {
                        code: "PORTAL_SESSION_LOST",
                        detail: "the compositor ended the capture session during a lease"
                            .to_owned(),
                    })
                }
            }
            Step::Captured(Some(event)) => self.map_event(event).await,
        }
    }

    /// Turns one portal event into at most one agent event, doing the async work
    /// (re-arming barriers) that `ZoneChanged` needs.
    async fn map_event(&mut self, event: CapturedEvent) -> Option<Event> {
        match event {
            CapturedEvent::Input(input) => {
                // EIS carries no per-event repeat flag, so the pressed-set rule decides
                // (KVM-036): a down for a key already held is the compositor repeating it.
                // Dropped for every mode, so a buffered replay cannot re-send a repeat
                // either.
                if self.repeats.is_repeat(&input, false) {
                    tracing::trace!(?input, "dropping an OS autorepeat");
                    return None;
                }
                if self.mode == Mode::Remote {
                    Some(Event::Input(input))
                } else {
                    // Preparing (or a hand-off with no lease yet): keep it for a local
                    // replay if the transition is cancelled.
                    self.buffered.push(input);
                    None
                }
            }
            CapturedEvent::Activated {
                activation_id,
                cursor,
                barrier_id,
            } => {
                tracing::info!(
                    activation_id,
                    barrier_id,
                    has_cursor = cursor.is_some(),
                    "the compositor activated the capture session"
                );
                self.activation = Some(Activation {
                    id: activation_id,
                    cursor,
                    barrier_id,
                });
                if let Some((zone, edge)) =
                    barrier_id.and_then(|id| wayland_barriers::locate(&self.barriers, id))
                {
                    tracing::info!(
                        zone,
                        edge = edge.as_str(),
                        "activation came from this barrier"
                    );
                }
                // The activation *is* the edge crossing on Wayland; core needs it to
                // start the transition.
                Some(Event::AtEdge {
                    barrier_id,
                    activation: portal_activation(cursor, barrier_id, &self.barriers),
                })
            }
            CapturedEvent::Deactivated => {
                self.activation = None;
                if self.mode == Mode::Observe {
                    // The pointer left the barrier; the session stays armed.
                    tracing::debug!("the compositor deactivated the capture session");
                    None
                } else {
                    self.mode = Mode::Observe;
                    Some(Event::Failed {
                        code: "PORTAL_SESSION_LOST",
                        detail: "the compositor deactivated the capture session during a lease"
                            .to_owned(),
                    })
                }
            }
            CapturedEvent::ZonesChanged { zone_set } => {
                tracing::info!(zone_set, "the zone set changed; re-arming the barriers");
                // `ZonesChanged` is the Wayland equivalent of a RandR notify
                // (`docs/03-architecture.md`), but display enumeration is KVM-040, so
                // nothing is reported to core here.
                match self.rearm().await {
                    Ok(()) => None,
                    Err(detail) => Some(Event::Failed {
                        code: "PORTAL_FRONTEND_STUCK",
                        detail,
                    }),
                }
            }
            CapturedEvent::Emergency => Some(Event::Emergency),
        }
    }

    /// Re-reads the zones and sets the barriers again, recreating the session when the
    /// compositor refuses to change barriers on a live one (GNOME 46).
    async fn rearm(&mut self) -> Result<(), String> {
        let Some(capture) = self.capture.as_mut() else {
            return Err("there is no capture session to re-arm".to_owned());
        };
        if let Err(error) = capture.refresh_zones().await {
            return Err(format!("GetZones failed: {error}"));
        }
        let zones: Vec<Zone> = zones_of(capture);
        let barriers = wayland_barriers::derive(&zones);
        let request: Vec<Barrier> = barriers
            .iter()
            .filter_map(|segment| {
                let id = BarrierID::new(segment.id)?;
                Some(Barrier::new(
                    id,
                    BarrierPosition::new(segment.x1, segment.y1, segment.x2, segment.y2),
                ))
            })
            .collect();

        match capture.set_barriers(&request).await {
            Ok(rejected) => {
                if !rejected.is_empty() {
                    tracing::warn!(
                        rejected = ?rejected,
                        "the compositor refused some barriers; retaining accepted barriers"
                    );
                }
                let accepted: Vec<BarrierSegment> = barriers
                    .into_iter()
                    .filter(|barrier| !rejected.contains(&barrier.id))
                    .collect();
                tracing::info!(zones = ?zones, barriers = ?accepted, "Wayland zones and accepted barriers");
                self.barriers = accepted;
                Ok(())
            }
            Err(WaylandError::BarriersWhileEnabled) => {
                tracing::warn!(
                    "the compositor will not update barriers on this live session; retaining \
                     the existing accepted barriers rather than opening another portal session"
                );
                Err(
                    "the active session cannot re-arm barriers without reopening InputCapture"
                        .to_owned(),
                )
            }
            Err(error) => Err(format!(
                "SetPointerBarriers failed: {}",
                error_chain(&error)
            )),
        }
    }
}

/// The portal zones as the pure geometry's own type.
///
/// `ashpd`'s `Region` is `(width, height, x, y)` behind accessors; the sign of the
/// offsets is whatever the compositor uses and is carried through.
fn zones_of(capture: &WaylandCapture) -> Vec<Zone> {
    capture
        .zones()
        .iter()
        .map(|region| {
            Zone::new(
                region.x_offset(),
                region.y_offset(),
                region.width() as i32,
                region.height() as i32,
            )
        })
        .collect()
}

/// Calculates a bounding box directly from the capture portal's logical zones.
fn relative_bounds_of(capture: &WaylandCapture) -> Option<RelativeBounds> {
    relative_bounds(capture.zones().iter().map(|region| {
        Zone::new(
            region.x_offset(),
            region.y_offset(),
            region.width() as i32,
            region.height() as i32,
        )
    }))
}

fn relative_bounds(zones: impl Iterator<Item = Zone>) -> Option<RelativeBounds> {
    let mut usable = zones.filter(Zone::is_usable);
    let first = usable.next()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (
        first.x.checked_add(first.width)?,
        first.y.checked_add(first.height)?,
    );
    for zone in usable {
        left = left.min(zone.x);
        top = top.min(zone.y);
        right = right.max(zone.x.checked_add(zone.width)?);
        bottom = bottom.max(zone.y.checked_add(zone.height)?);
    }
    Some(RelativeBounds {
        x: left,
        y: top,
        width: u32::try_from(right.checked_sub(left)?).ok()?,
        height: u32::try_from(bottom.checked_sub(top)?).ok()?,
    })
}

async fn arm_capture(
    capture: &mut WaylandCapture,
    installed: &mut Vec<BarrierSegment>,
) -> Result<(), WaylandError> {
    capture.refresh_zones().await?;
    let zones: Vec<Zone> = zones_of(capture);
    let segments = wayland_barriers::derive(&zones);
    let request: Vec<Barrier> = segments
        .iter()
        .filter_map(|segment| {
            let id = BarrierID::new(segment.id)?;
            Some(Barrier::new(
                id,
                BarrierPosition::new(segment.x1, segment.y1, segment.x2, segment.y2),
            ))
        })
        .collect();
    let rejected = capture.set_barriers(&request).await?;
    if !rejected.is_empty() {
        tracing::warn!(
            rejected = ?rejected,
            "the compositor refused some barriers; retaining accepted barriers"
        );
    }
    let accepted: Vec<BarrierSegment> = segments
        .into_iter()
        .filter(|segment| !rejected.contains(&segment.id))
        .collect();
    tracing::info!(zones = ?zones, barriers = ?accepted, "Wayland zones and accepted barriers");
    // A session that is already enabled cannot be enabled twice.
    if !capture.is_enabled() {
        capture.enable().await?;
    }
    *installed = accepted;
    Ok(())
}

/// The capability list after the portals have answered.
///
/// The probe can only reason about the session; these rules are the ones the documents
/// attach to the *runtime* answers:
///
/// * `ABSOLUTE_POINTER` only with an absolute region; relative-only devices use the
///   bounded relative fallback and do not advertise this hardware capability.
/// * `EMERGENCY_HOTKEY` only when `GlobalShortcuts` registered the combination —
///   «Если backend отсутствует, `CAPABILITY_EMERGENCY_HOTKEY` не объявляется».
/// * `RECEIVER_TAKEOVER` never: «Портальный получатель не видит физический ввод …
///   Автоматический takeover поэтому не объявляется».
/// * `CAPTURE` only while a capture session is armed.
fn refine_ready(
    session: &SessionInfo,
    has_capture: bool,
    injector: &WaylandInjector,
    has_hotkey: bool,
    refusals: &[Refusal],
) -> Ready {
    let mut ready = Ready::from_session(session, String::new());
    let capabilities = injector.capabilities();
    let absolute = capabilities.has_absolute_injection;
    ready.capabilities.retain(|name| match name.as_str() {
        "CAPTURE" => has_capture,
        "ABSOLUTE_POINTER" => absolute,
        "EMERGENCY_HOTKEY" => has_hotkey,
        "RECEIVER_TAKEOVER" => false,
        _ => true,
    });
    // A clipboard capability is declared only once the portal says so (ADR-008); the
    // capture session is the only place that answer exists, and clipboard is outside
    // this slice, so it stays off.
    ready.capabilities.retain(|name| name != "TEXT_CLIPBOARD");

    if !absolute {
        ready.capabilities.retain(|name| name != "ABSOLUTE_POINTER");
    }
    ready.backend.has_absolute_injection = absolute;
    ready.backend.has_emergency_hotkey = has_hotkey;
    ready.backend.has_receiver_takeover = false;
    let mut degraded: Vec<&str> = refusals
        .iter()
        .map(|refusal| refusal.detail.as_str())
        .filter(|detail| !detail.is_empty())
        .collect();
    if !capabilities.has_keyboard {
        degraded.push("EIS keyboard device unavailable; keyboard injection is disabled");
    }
    ready.backend.degraded_reason = degraded.join("; ");
    ready
}

/// The `AgentError.code` for a portal failure (`docs/ipc.proto`: `PORTAL_DENIED,
/// PORTAL_UNAVAILABLE, PORTAL_SESSION_LOST, PORTAL_FRONTEND_STUCK, …`).
fn portal_code(error: &WaylandError) -> &'static str {
    match error {
        WaylandError::Portal(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)) => {
            "PORTAL_DENIED"
        }
        WaylandError::NoPointerDevice | WaylandError::NoEiDevice => "CAPTURE_FAILED",
        WaylandError::PortalUnavailable { .. }
        | WaylandError::PortalTooOld { .. }
        | WaylandError::HotkeyUnavailable { .. }
        | WaylandError::UnsupportedCompositor { .. }
        | WaylandError::NoAbsoluteRegion => "PORTAL_UNAVAILABLE",
        WaylandError::BarriersWhileEnabled => "PORTAL_FRONTEND_STUCK",
        WaylandError::EiDisconnected | WaylandError::EiSocketUnavailable => "PORTAL_SESSION_LOST",
        _ => "PORTAL_UNAVAILABLE",
    }
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        chain.push_str(": ");
        chain.push_str(&error.to_string());
        source = error.source();
    }
    chain
}

/// A failure that keeps the node local, carrying the code core should report.
fn fatal(error: &WaylandError) -> Refusal {
    Refusal {
        code: portal_code(error),
        detail: format!(
            "platform-wayland is not usable on this session: {}",
            error_chain(error)
        ),
    }
}
#[cfg(test)]
mod activation_tests {
    use super::*;

    #[test]
    fn activation_uses_only_actual_cursor_and_accepted_barrier() {
        let barriers = wayland_barriers::derive(&[Zone::new(0, 0, 100, 100)]);
        for barrier in &barriers {
            let activation =
                portal_activation(Some(Position::new(12.0, 34.0)), Some(barrier.id), &barriers);
            assert_eq!(
                activation.position,
                Some(protocol::ipc::PointerAbs {
                    display_id: String::new(),
                    x: 12.0,
                    y: 34.0
                })
            );
            let expected = match barrier.edge {
                wayland_barriers::Edge::Left => protocol::ipc::PortalEdge::Left,
                wayland_barriers::Edge::Right => protocol::ipc::PortalEdge::Right,
                wayland_barriers::Edge::Top => protocol::ipc::PortalEdge::Top,
                wayland_barriers::Edge::Bottom => protocol::ipc::PortalEdge::Bottom,
            };
            assert_eq!(activation.edge, expected as i32);
        }
        for id in [None, Some(u32::MAX)] {
            let activation = portal_activation(None, id, &barriers);
            assert_eq!(activation.position, None);
            assert_eq!(
                activation.edge,
                protocol::ipc::PortalEdge::Unspecified as i32
            );
        }
    }
}
