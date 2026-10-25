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
//! * Display enumeration prefers capture zones and falls back to xdg-output logical
//!   geometry independently of portal input permissions.
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

type HotkeyCandidate = (config::HotkeyBindings, Result<EmergencyShortcuts, String>);

/// Owns an independently pumped portal session, never blocked on core IPC.
struct ShortcutTask {
    events: tokio::sync::mpsc::UnboundedReceiver<config::HotkeyAction>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ShortcutTask {
    fn start(
        mut shortcuts: EmergencyShortcuts,
        emergency: Option<platform_wayland::PortalEmergencyRelease>,
    ) -> Self {
        let (send, events) = tokio::sync::mpsc::unbounded_channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            loop {
                let id = tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    id = shortcuts.next_activation() => match id {
                        Some(id) => id,
                        None => break,
                    },
                };
                let Some(action) = shortcut_action(&id) else {
                    continue;
                };
                if action == config::HotkeyAction::EmergencyRelease {
                    if let Some(emergency) = &emergency {
                        if let Err(error) = emergency.release().await {
                            tracing::warn!(%error, "autonomous emergency portal release failed");
                        }
                    }
                }
                if send.send(action).is_err() {
                    break;
                }
            }
            let _ = shortcuts.close().await;
        });
        Self {
            events,
            stop: Some(stop),
        }
    }
}

impl Drop for ShortcutTask {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn shortcut_action(id: &str) -> Option<config::HotkeyAction> {
    use config::HotkeyAction;
    match id {
        "emergency" => Some(HotkeyAction::EmergencyRelease),
        "switch_next" => Some(HotkeyAction::SwitchNext),
        "return_local" => Some(HotkeyAction::ReturnLocal),
        "claim_ownership" => Some(HotkeyAction::ClaimOwnership),
        "game_guard_toggle" => Some(HotkeyAction::GameGuardToggle),
        _ => None,
    }
}

async fn register_shortcuts(
    bindings: &config::HotkeyBindings,
) -> Result<EmergencyShortcuts, String> {
    bindings.validate().map_err(|error| error.to_string())?;
    let requested = [
        (
            "emergency",
            "Bifrost: emergency release",
            &bindings.emergency_release,
        ),
        (
            "switch_next",
            "Bifrost: switch to next machine",
            &bindings.switch_next,
        ),
        (
            "return_local",
            "Bifrost: return to this machine",
            &bindings.return_local,
        ),
        (
            "claim_ownership",
            "Bifrost: claim keyboard and pointer ownership",
            &bindings.claim_ownership,
        ),
        (
            "game_guard_toggle",
            "Bifrost: toggle game mode",
            &bindings.game_guard_toggle,
        ),
    ];
    let requested: Vec<_> = requested
        .into_iter()
        .filter(|(_, _, chord)| !chord.trim().is_empty())
        .map(|(id, description, chord)| {
            portal_trigger(chord).map(|trigger| (id, description, trigger))
        })
        .collect::<Result<_, _>>()?;
    let borrowed: Vec<_> = requested
        .iter()
        .map(|(id, description, trigger)| (*id, *description, trigger.as_str()))
        .collect();
    EmergencyShortcuts::register_bindings(&borrowed).await.map_err(|error| format!(
        "GlobalShortcuts registration failed for emergency_release={:?}, switch_next={:?}, return_local={:?}, claim_ownership={:?}: {error}. The compositor may show an approval dialog; cancellation, an unavailable chord, or an omitted action keeps all previous bindings. Preferred chords may be remapped in the compositor shortcut editor.",
        bindings.emergency_release, bindings.switch_next, bindings.return_local, bindings.claim_ownership,
    ))
}

/// XDG shortcuts use xkbcommon modifier and keysym names, not config spelling.
fn portal_trigger(chord: &str) -> Result<String, String> {
    let parsed = config::Hotkey::parse(chord).map_err(|error| error.to_string())?;
    let mut trigger = String::new();
    for modifier in parsed.modifiers() {
        trigger.push_str(match modifier {
            config::Modifier::Ctrl => "CTRL+",
            config::Modifier::Alt => "ALT+",
            config::Modifier::Shift => "SHIFT+",
            config::Modifier::Super => "LOGO+",
        });
    }
    let key = match parsed.key() {
        "Enter" => "Return",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "Backspace" => "BackSpace",
        "Space" => "space",
        key => key,
    };
    if key.len() == 1 {
        trigger.push_str(&key.to_ascii_lowercase());
    } else {
        trigger.push_str(key);
    }
    Ok(trigger)
}

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
    overlay: Option<platform_wayland::overlay::EdgeOverlay>,
    edge_effects: bool,
    overlay_attempted: bool,
    /// `None` when this compositor has no `GlobalShortcuts` portal (GNOME 46).
    shortcuts: Option<ShortcutTask>,
    hotkey_update: Option<tokio::sync::oneshot::Receiver<HotkeyCandidate>>,
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
    /// The local selection through `ext-data-control-v1`.
    ///
    /// `None` when this compositor does not implement it (GNOME has no data-control at all),
    /// in which case this backend has no clipboard and says so in `AgentReady` (ADR-008).
    clipboard: Option<platform_wayland::Clipboard>,
}

impl Wayland {
    /// Connects the inject session (fatal if it fails) and arms the capture session
    /// (degradable), then reports what the portals actually allow.
    ///
    /// `restore_tokens` are the ones core persisted; a token is only used when it
    /// belongs to this compositor and these portal versions.
    pub async fn connect(
        session: &SessionInfo,
        hotkeys: &config::HotkeyBindings,
        restore_tokens: &[protocol::ipc::PortalGrant],
        portal_progress: &tokio::sync::mpsc::UnboundedSender<protocol::ipc::agent_frame::Msg>,
    ) -> Result<(Self, Ready, Vec<Refusal>), Refusal> {
        platform_wayland::appid::register().await;
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
        // Establish the safety path before trying optional actions. The startup
        // SetHotkeys candidate replaces it only after every enabled action is granted.
        let initial = match portal_trigger(&hotkeys.emergency_release) {
            Ok(trigger) => EmergencyShortcuts::register(&trigger)
                .await
                .map_err(|error| Refusal {
                    code: portal_code(&error),
                    detail: error.to_string(),
                }),
            Err(error) => Err(Refusal {
                code: "PORTAL_DENIED",
                detail: error,
            }),
        };
        let shortcuts = match initial {
            Ok(shortcuts) => Some(shortcuts),
            Err(error) => {
                refusals.push(Refusal {
                    code: error.code,
                    detail: format!(
                        "global shortcuts are not available: {}; this node cannot be the source while require_emergency_hotkey is true", error.detail
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
        let shortcuts = shortcuts.map(|shortcuts| {
            ShortcutTask::start(
                shortcuts,
                capture
                    .as_ref()
                    .map(WaylandCapture::portal_emergency_release),
            )
        });

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

        // The clipboard is the one part of this backend that is neither a portal session nor an
        // EIS device: `ext-data-control-v1` is a privileged-client interface of the compositor
        // itself (KWin), and it is the only route that works outside a capture activation. A
        // compositor without it (GNOME) loses the clipboard and nothing else, so this is a
        // capability, not a refusal.
        let clipboard = match platform_wayland::Clipboard::connect() {
            Some(clipboard) => Some(clipboard),
            None => {
                tracing::info!(
                    "no clipboard: this compositor has no ext_data_control_manager_v1; this \
                     session will not do the clipboard"
                );
                None
            }
        };

        let ready = refine_ready(
            session,
            capture.is_some(),
            &injector,
            shortcuts.is_some(),
            clipboard.is_some(),
            &refusals,
        );
        let wayland = Wayland {
            ready: ready.clone(),
            capture,
            injector,
            overlay: None,
            edge_effects: false,
            overlay_attempted: false,
            shortcuts,
            hotkey_update: None,
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
            clipboard,
        };
        Ok((wayland, ready, refusals))
    }

    /// Registers the candidate first; cancellation or rejection leaves the old
    /// independent emergency task alive, including while the approval dialog is open.
    pub fn request_hotkeys(&mut self, bindings: config::HotkeyBindings) -> Result<(), String> {
        bindings.validate().map_err(|error| error.to_string())?;
        if self.hotkey_update.is_some() {
            return Err("A GlobalShortcuts approval dialog is already pending".to_owned());
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        self.hotkey_update = Some(receive);
        tokio::spawn(async move {
            let candidate = register_shortcuts(&bindings).await;
            if let Err((_, Ok(candidate))) = send.send((bindings, candidate)) {
                let _ = candidate.close().await;
            }
        });
        Ok(())
    }

    pub async fn set_hotkeys(&mut self, bindings: &config::HotkeyBindings) -> Result<(), String> {
        let candidate = register_shortcuts(bindings).await?;
        let candidate = ShortcutTask::start(
            candidate,
            self.capture
                .as_ref()
                .map(WaylandCapture::portal_emergency_release),
        );
        self.shortcuts = Some(candidate);
        Ok(())
    }

    /// The capability report sent right after the attach.
    pub fn ready(&self) -> Ready {
        self.ready.clone()
    }

    /// The capture portal owns cursor visibility; no local cursor override is needed.
    pub fn set_effects(&mut self, edge_effects: bool, _hide_cursor: bool) {
        self.edge_effects = edge_effects;
        if let Some(overlay) = &self.overlay {
            overlay.set_enabled(edge_effects);
        }
    }

    /// Starts the optional overlay only when there is an effect to display.
    pub fn edge_fx(&mut self, update: edgefx::Update) {
        if !self.edge_effects {
            return;
        }
        if !self.overlay_attempted
            && matches!(
                update,
                edgefx::Update::Proximity { .. }
                    | edgefx::Update::Crossed(Some(_))
                    | edgefx::Update::Entered(_)
            )
        {
            self.overlay_attempted = true;
            self.overlay = platform_wayland::overlay::EdgeOverlay::spawn();
        }
        if let Some(overlay) = &self.overlay {
            overlay.send(update);
        }
    }

    /// Whether this backend can own and read the local CLIPBOARD selection.
    ///
    /// It can when the compositor implements `ext-data-control-v1` (KWin, and every compositor
    /// that took the staging protocol): `platform_wayland::Clipboard` then holds a connection
    /// that both reads the selection and takes it over. A compositor without it — Mutter
    /// implements neither `ext-` nor `wlr-data-control` — leaves this `false`, `TEXT_CLIPBOARD`
    /// is not advertised (`refine_ready` drops it) and core never hands this node a payload
    /// (ADR-008: the capability is a promise, and a route that cannot honour it is not
    /// announced as if it could).
    pub fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// The next clipboard change the *user* made.
    ///
    /// Never waits: the session polls this from its own tick, where the core-silence watchdog
    /// lives too. The echo rule (a payload this node applied must not be reported back as the
    /// user's copy) is the adapter's own, because a data-control device is told about every
    /// selection including the one this node set; see `platform_wayland::data_control`.
    pub async fn clipboard_change(&mut self) -> Option<ClipboardChange> {
        let change = self.clipboard.as_mut()?.poll_change()?;
        Some(ClipboardChange {
            text: change.text,
            markers: change.markers,
        })
    }

    /// Owns the local selection with `text`; `None` gives it up.
    ///
    /// Refused with `CLIPBOARD_DISABLED` when there is no data-control connection: core is
    /// entitled to an answer rather than silence.
    pub async fn set_clipboard(&mut self, text: Option<&str>) -> Result<(), String> {
        let Some(clipboard) = self.clipboard.as_ref() else {
            return Err(
                "CLIPBOARD_DISABLED: this compositor has no ext_data_control_manager_v1".to_owned(),
            );
        };
        clipboard.set_text(text).await.map_err(|error| {
            format!("CLIPBOARD_APPLY_FAILED: selection was not confirmed: {error}")
        })?;
        tracing::info!(
            bytes = text.map_or(0, str::len),
            released = text.is_none(),
            "CLIPBOARD selection set"
        );
        Ok(())
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
        self.edge_fx(edgefx::Update::Clear);
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

    /// Capture zones are authoritative; xdg-output is the permission-free fallback.
    /// Both provide logical compositor coordinates, never scaled pixel coordinates.
    pub async fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        let zones = self
            .capture
            .as_ref()
            .map(zones_of)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, zone)| platform_wayland::displays::Display {
                id: format!("zone-{index}"),
                name: String::new(),
                logical: [zone.x, zone.y, zone.width, zone.height],
                scale: 1,
                ..Default::default()
            })
            .collect();
        match tokio::task::spawn_blocking(move || {
            platform_wayland::displays::discover(zones, platform_wayland::displays::enumerate)
        })
        .await
        {
            Ok(Ok((source, outputs))) => {
                tracing::info!(source, displays = outputs.len(), geometry = ?outputs, "display discovery");
                outputs
                    .into_iter()
                    .enumerate()
                    .map(|(index, output)| {
                        let scale = output.logical_scale();
                        let native = output
                            .native_size()
                            .map(|[width, height]| [0, 0, width, height]);
                        crate::displays::RawDisplay {
                            id: output.id,
                            name: output.name,
                            logical: output.logical.map(f64::from),
                            native,
                            scale,
                            primary: index == 0,
                        }
                    })
                    .collect()
            }
            result => {
                tracing::warn!(
                    ?result,
                    "displays not detected: capture zones and Wayland xdg-output unavailable"
                );
                Vec::new()
            }
        }
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
        self.edge_fx(edgefx::Update::Clear);
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
            capture,
            shortcuts,
            hotkey_update,
            ..
        } = self;
        let shortcut = async {
            match shortcuts.as_mut() {
                Some(shortcuts) => shortcuts.events.recv().await,
                None => std::future::pending().await,
            }
        };
        let update = async {
            match hotkey_update.as_mut() {
                Some(update) => update.await,
                None => std::future::pending().await,
            }
        };

        enum Step {
            Captured(Option<CapturedEvent>),
            Shortcut(Option<config::HotkeyAction>),
            Updated,
            Tick,
        }
        // Keep the infrequent configuration result off the per-input-event discriminant,
        // without allocating a box whenever shortcuts are replaced.
        let mut updated = None;

        let step = match capture {
            Some(capture) => tokio::select! {
                biased;
                result = update => { updated = Some(result); Step::Updated },
                action = shortcut => Step::Shortcut(action),
                event = capture.next_event() => Step::Captured(event),
                () = tokio::time::sleep(POLL_TICK) => Step::Tick,
            },
            None => tokio::select! {
                biased;
                result = update => { updated = Some(result); Step::Updated },
                action = shortcut => Step::Shortcut(action),
                () = tokio::time::sleep(POLL_TICK) => Step::Tick,
            },
        };

        match step {
            Step::Tick => None,
            Step::Updated => {
                let result = updated.expect("the update select arm stored its result");
                self.hotkey_update = None;
                match result {
                    Ok((bindings, candidate)) => {
                        let mut descriptions = Vec::new();
                        let result = candidate.map(|candidate| {
                            for (action, chord) in bindings.iter() {
                                let id = match action {
                                    config::HotkeyAction::EmergencyRelease => "emergency",
                                    config::HotkeyAction::SwitchNext => "switch_next",
                                    config::HotkeyAction::ReturnLocal => "return_local",
                                    config::HotkeyAction::ClaimOwnership => "claim_ownership",
                                    config::HotkeyAction::GameGuardToggle => "game_guard_toggle",
                                };
                                descriptions.push(if chord.trim().is_empty() {
                                    format!("{}: disabled", action.as_str())
                                } else {
                                    format!(
                                        "{}: compositor actual trigger {:?}; requested chord {:?}. The compositor may remap preferred chords; change the actual trigger in its shortcut editor.",
                                        action.as_str(), candidate.trigger_description(id).expect("validated candidate grants every enabled action"), chord,
                                    )
                                });
                            }
                            self.shortcuts = Some(ShortcutTask::start(
                                candidate,
                                self.capture.as_ref().map(WaylandCapture::portal_emergency_release),
                            ));
                        });
                        let mut response = crate::backend::hotkey_result(
                            &bindings, result,
                            "GlobalShortcuts approval completed; actual triggers are reported per action.",
                        );
                        if response.success {
                            response.detail = descriptions.join("\n");
                            for (binding, detail) in response.bindings.iter_mut().zip(descriptions) {
                                binding.available = !binding.chord.trim().is_empty();
                                binding.detail = detail;
                            }
                        }
                        Some(Event::HotkeysResult(response))
                    }
                    Err(error) => Some(Event::Failed {
                        code: "HOTKEY_UNAVAILABLE",
                        detail: format!("GlobalShortcuts registration worker stopped: {error}; existing bindings retained"),
                    }),
                }
            }
            Step::Shortcut(Some(config::HotkeyAction::EmergencyRelease)) => {
                if self.capture.is_none() {
                    return Some(Event::Emergency);
                }
                // The independent task already released the portal and raised the
                // capture flag; its Emergency event drains local input state.
                None
            }
            Step::Shortcut(Some(action)) => Some(Event::HotkeyPressed(action)),
            Step::Shortcut(None) => {
                self.shortcuts = None;
                Some(Event::Failed {
                    code: "HOTKEY_UNAVAILABLE",
                    detail: "GlobalShortcuts portal session ended; hotkeys are unavailable"
                        .to_owned(),
                })
            }
            Step::Captured(None) => {
                // The portal session ended (the compositor closed or disabled it).
                self.capture = None;
                self.edge_fx(edgefx::Update::Clear);
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
                self.edge_fx(edgefx::Update::Clear);
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
            CapturedEvent::Emergency => {
                self.edge_fx(edgefx::Update::Clear);
                Some(Event::Emergency)
            }
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
/// * `TEXT_CLIPBOARD` only while the clipboard adapter holds a live data-control connection —
///   «capability объявляется только после `clipboard_enabled`» in ADR-008's words, and
///   `platform_wayland::Clipboard` is what that connection is. A compositor without
///   `ext-data-control-v1` therefore advertises no clipboard, and core never offers this node
///   a payload it would have to refuse.
fn refine_ready(
    session: &SessionInfo,
    has_capture: bool,
    injector: &WaylandInjector,
    has_hotkey: bool,
    has_clipboard: bool,
    refusals: &[Refusal],
) -> Ready {
    let mut ready = Ready::from_session(session, String::new());
    let capabilities = injector.capabilities();
    let absolute = capabilities.has_absolute_injection;
    ready.capabilities = refine_capabilities(
        ready.capabilities,
        has_capture,
        absolute,
        has_hotkey,
        has_clipboard,
    );
    // `Ready::from_session` carries what the *probe* could know, and the probe is honest about
    // the clipboard: it says `false` at probe time because only the adapter knows. So the
    // capability is also *added* here, not merely kept.
    ready.set_text_clipboard(has_clipboard);

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

/// The capability names after the runtime answers, as a pure list.
///
/// Split out of [`refine_ready`] so the rules above are testable without a compositor: the
/// portal answers arrive as plain `bool`s here.
fn refine_capabilities(
    capabilities: Vec<String>,
    has_capture: bool,
    absolute: bool,
    has_hotkey: bool,
    has_clipboard: bool,
) -> Vec<String> {
    let mut capabilities = capabilities;
    capabilities.retain(|name| match name.as_str() {
        "CAPTURE" => has_capture,
        "ABSOLUTE_POINTER" => absolute,
        "EMERGENCY_HOTKEY" => has_hotkey,
        "RECEIVER_TAKEOVER" => false,
        "TEXT_CLIPBOARD" => has_clipboard,
        _ => true,
    });
    if has_clipboard && !capabilities.iter().any(|name| name == "TEXT_CLIPBOARD") {
        capabilities.push("TEXT_CLIPBOARD".to_owned());
    }
    capabilities
}

/// The `AgentError.code` for a portal failure (`docs/ipc.proto`: `PORTAL_DENIED,
/// PORTAL_UNAVAILABLE, PORTAL_SESSION_LOST, PORTAL_FRONTEND_STUCK, …`).
fn portal_code(error: &WaylandError) -> &'static str {
    match error {
        WaylandError::HotkeyNotGranted { .. } => "PORTAL_DENIED",
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
    fn dismissed_and_unbound_shortcuts_are_denied_not_unavailable() {
        assert_eq!(
            portal_code(&WaylandError::Portal(ashpd::Error::Response(
                ashpd::desktop::ResponseError::Cancelled,
            ))),
            "PORTAL_DENIED"
        );
        assert_eq!(
            portal_code(&WaylandError::HotkeyNotGranted {
                missing: "emergency (CTRL+F12)".to_owned(),
            }),
            "PORTAL_DENIED"
        );
        assert_eq!(
            portal_code(&WaylandError::HotkeyUnavailable {
                portal_interface: platform_wayland::GLOBAL_SHORTCUTS_INTERFACE,
            }),
            "PORTAL_UNAVAILABLE"
        );
    }
    #[test]
    fn portal_trigger_uses_xdg_modifier_and_keysym_names() {
        assert_eq!(
            portal_trigger("Ctrl+Alt+Shift+F12").unwrap(),
            "CTRL+ALT+SHIFT+F12"
        );
        assert_eq!(portal_trigger("Super+Enter").unwrap(), "LOGO+Return");
        assert_eq!(portal_trigger("Ctrl+A").unwrap(), "CTRL+a");
        assert_eq!(portal_trigger("Alt+PageDown").unwrap(), "ALT+Next");
    }

    #[test]
    fn portal_ids_map_to_all_four_actions_without_falling_back_to_emergency() {
        use config::HotkeyAction;
        for (id, action) in [
            ("emergency", HotkeyAction::EmergencyRelease),
            ("switch_next", HotkeyAction::SwitchNext),
            ("return_local", HotkeyAction::ReturnLocal),
            ("claim_ownership", HotkeyAction::ClaimOwnership),
        ] {
            assert_eq!(shortcut_action(id), Some(action));
        }
        assert_eq!(shortcut_action("unknown"), None);
    }

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

#[cfg(test)]
mod capability_tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// The clipboard is the one capability the probe cannot decide: it reports `false` at probe
    /// time because only the adapter knows whether this compositor has `ext-data-control-v1`
    /// (ADR-008: «capability объявляется только после ...»). So the runtime answer has to be
    /// able to *add* the name, not merely strip it — which is exactly what the old
    /// unconditional `retain(|name| name != "TEXT_CLIPBOARD")` made impossible.
    #[test]
    fn the_clipboard_capability_follows_the_adapter() {
        let live = refine_capabilities(
            names(&["CAPTURE", "INJECT", "EMERGENCY_HOTKEY"]),
            true,
            true,
            true,
            true,
        );
        assert!(live.iter().any(|name| name == "TEXT_CLIPBOARD"));

        // No data-control (GNOME): no clipboard, whichever way the probe answered.
        let absent = refine_capabilities(
            names(&["CAPTURE", "TEXT_CLIPBOARD"]),
            true,
            true,
            true,
            false,
        );
        assert!(!absent.iter().any(|name| name == "TEXT_CLIPBOARD"));
        assert!(absent.iter().any(|name| name == "CAPTURE"));
    }

    #[test]
    fn the_runtime_rules_keep_their_meaning() {
        let refined = refine_capabilities(
            names(&[
                "CAPTURE",
                "INJECT",
                "ABSOLUTE_POINTER",
                "EMERGENCY_HOTKEY",
                "RECEIVER_TAKEOVER",
                "DISPLAYS_CHANGED",
            ]),
            // No armed capture session, relative-only injection, no hotkey binding.
            false,
            false,
            false,
            false,
        );
        for gone in [
            "CAPTURE",
            "ABSOLUTE_POINTER",
            "EMERGENCY_HOTKEY",
            "RECEIVER_TAKEOVER",
        ] {
            assert!(
                !refined.iter().any(|name| name == gone),
                "{gone} must be gone"
            );
        }
        for kept in ["INJECT", "DISPLAYS_CHANGED"] {
            assert!(
                refined.iter().any(|name| name == kept),
                "{kept} must be kept"
            );
        }
        // A clipboard that is live is never listed twice.
        let doubled = refine_capabilities(names(&["TEXT_CLIPBOARD"]), true, true, true, true);
        assert_eq!(
            doubled
                .iter()
                .filter(|name| *name == "TEXT_CLIPBOARD")
                .count(),
            1
        );
    }
}
