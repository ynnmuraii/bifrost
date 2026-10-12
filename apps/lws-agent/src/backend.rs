//! Backend selection and the agent-side adapter surface.
//!
//! `docs/03-architecture.md` («Определение сеанса и выбор платформенного бэкенда»):
//! `platform-probe` decides the session and the backend, and the agent is the only
//! process that links a platform crate («Платформенные crates подключает только
//! agent»). There is no common `PlatformAdapter` trait in the repository yet, so this
//! module is the small dispatch that turns one decision into one concrete backend and
//! gives the session loop a single vocabulary (`begin_capture`/`inject`/…).
//!
//! The probe runs in this process even when core has already probed: an override has
//! to be checked against the real environment («выбранный вручную бэкенд обязан
//! соответствовать сеансу»).
//!
//! # What this slice does *not* wire
//!
//! `platform-wayland` (portal `InputCapture` + EIS through `RemoteDesktop`) is
//! KVM-026/027 in stage E3, and the E2 slice is the Windows → Linux X11 direction.
//! A Wayland session is therefore *selected* as Wayland and then refused explicitly:
//! `AgentReady` carries no capabilities and `degraded_reason` names the task, the
//! node stays `Local`, and core shows the reason. Refusing loudly is what the
//! documents require of a backend that cannot honour the session; a half-wired
//! portal session would look like a defect instead.

use input_model::{CaptureKind, HidKey, InputEvent, MouseButton, Position};
use platform_probe::{BackendOverride, EnvSnapshot, SessionInfo};
use protocol::ipc;

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod win;
#[cfg(target_os = "linux")]
mod x11;
// Pure geometry, so its unit tests run on every host; only the Linux backend calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod wayland_barriers;

/// The platform input path this agent drives.
///
/// No `Debug`: the OS capture handles inside the platform backends do not implement
/// it, and the events the agent logs never need a backend dump (they must stay
/// payload-free anyway).
pub enum Backend {
    /// No OS input path: `Inject` is logged, capture installs nothing.
    Dry(Box<Dry>),
    #[cfg(windows)]
    Win(Box<win::Win>),
    #[cfg(target_os = "linux")]
    X11(Box<x11::X11>),
    /// Portal capture (`InputCapture`) plus EIS injection (`RemoteDesktop`).
    #[cfg(target_os = "linux")]
    Wayland(Box<wayland::Wayland>),
}

/// Whether and how this backend watches the machine for physical input while it is a
/// receiver, and the threshold it watches for (KVM-035).
///
/// `None` is `[input].takeover_detection = "off"`: no detector, no `LocalInputDetected`,
/// and the node must not claim `CAPABILITY_RECEIVER_TAKEOVER` (AC-13). The plan exists
/// only on a backend that can tell physical input from synthetic input, which on this
/// build means Windows and X11 — a portal receiver cannot see its own devices (AC-13a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeoverPlan {
    /// `[input].takeover_threshold_px`.
    pub threshold_px: u32,
}

/// What the agent reports in `AgentReady` right after the attach.
#[derive(Debug, Clone)]
pub struct Ready {
    /// `CAPABILITY_*` names without the prefix (`AgentReady.capabilities`).
    pub capabilities: Vec<String>,
    /// `platform-win` / `platform-x11` / `platform-wayland`.
    pub platform: String,
    /// `windows` / `x11` / `wayland`.
    pub session_kind: String,
    /// The structured detail `AgentReady.backend` carries.
    pub backend: ipc::BackendInfo,
}

/// One thing the backend produced while capturing.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Normalised input to forward (`CapturedBatch`).
    Input(InputEvent),
    /// The emergency combination was recognised; the agent already restored local
    /// control (rule 3).
    Emergency,
    /// The pointer reached the edge of this node and the compositor handed input over.
    ///
    /// `docs/05-protocol.md` opens a transition with «`CapturedBatch`, pointer reaches
    /// link edge», and [`at_edge`](ipc::CapturedBatch::at_edge) is that signal. On
    /// Wayland it is the only one there is: the compositor activates capture at the
    /// barrier, so the pointer never leaves the node on its own.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    AtEdge {
        /// The barrier the compositor named, when it named one.
        barrier_id: Option<u32>,
    },
    /// The bounded capture queue overflowed; the agent must end capture (rule 2).
    ///
    /// Only the Windows capture has a bounded queue that can report overflow; the
    /// X11 backend reads from the X connection and has no equivalent signal.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    QueueOverflow,
    /// A human touched this machine while a lease was injecting here: the receiver
    /// takeover of rule 9 (`docs/03-architecture.md`: «физическое (не XTest/не injected)
    /// событие при активном inject → `LocalInputDetected`. Agent сам освобождает ledger»).
    ///
    /// Raised by [`crate::takeover::Detector`] from an observe-only capture, which is the
    /// only path that sees the machine's own keyboard and mouse while the agent is a
    /// receiver (KVM-035). `reason` is for the log; the wire frame carries the lease id.
    LocalTakeover {
        /// `key` / `button` / `motion`.
        reason: &'static str,
        /// The distance motion accumulated, when the reason is `motion`.
        distance_px: Option<f64>,
    },
    /// The capture path failed or lost its session; the agent ends capture and reports
    /// it to core with `code` (`AgentError.code`: `CAPTURE_FAILED`, `PORTAL_DENIED`,
    /// `PORTAL_SESSION_LOST`, `PORTAL_FRONTEND_STUCK`, …).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Failed {
        /// The `AgentError.code` core should see.
        code: &'static str,
        /// Payload-free, actionable text.
        detail: String,
    },
    /// A fresh portal restore token the agent received and core has to persist.
    ///
    /// Only a portal backend can produce it, so on other platforms the variant exists
    /// for the session loop's exhaustive match and is never built.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    PortalGrant(ipc::PortalGrant),
}

/// Something the selected backend wants core to know without ending the session: a
/// portal that is unavailable, a dialog the user dismissed, a capability the
/// compositor refused.
///
/// The session loop turns each one into an `AgentError` frame after `AgentReady`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// `AgentError.code` (`docs/ipc.proto`: `PORTAL_DENIED`, `PORTAL_UNAVAILABLE`, …).
    pub code: &'static str,
    /// Payload-free, actionable text: never a token, a key or clipboard content.
    pub detail: String,
}

/// The outcome of one `Inject` batch.
#[derive(Debug, Clone, Default)]
pub struct Injected {
    /// Events the backend accepted.
    pub applied: usize,
    /// Backend error text, empty on success.
    pub os_error: String,
    /// Where the local cursor ended up, when the backend knows.
    pub actual: Option<Position>,
}

/// Keys and buttons a release actually let go of.
#[derive(Debug, Clone, Copy, Default)]
pub struct Released {
    /// Keys released.
    pub keys: u32,
    /// Buttons released.
    pub buttons: u32,
}

impl Ready {
    /// Turns a probe decision into the `AgentReady` payload.
    pub fn from_session(session: &SessionInfo, degraded_reason: String) -> Self {
        let info = session.to_backend_info();
        Ready {
            capabilities: session
                .capabilities
                .advertised()
                .into_iter()
                .map(str::to_owned)
                .collect(),
            platform: info.backend.clone(),
            session_kind: session.session_kind.as_str().to_owned(),
            backend: ipc::BackendInfo {
                backend: info.backend,
                compositor: info.compositor,
                portals: info
                    .portals
                    .into_iter()
                    .map(|portal| ipc::PortalInterface {
                        name: portal.name,
                        version: portal.version,
                    })
                    .collect(),
                has_emergency_hotkey: info.has_emergency_hotkey,
                has_portal_persistence: info.has_portal_persistence,
                has_receiver_takeover: info.has_receiver_takeover,
                has_absolute_injection: info.has_absolute_injection,
                // Declared only once the clipboard portal answers
                // `clipboard_enabled` (ADR-008); clipboard is outside this slice.
                has_portal_clipboard: false,
                degraded_reason,
            },
        }
    }

    /// The same report with no capability claimed, for a session this build cannot
    /// drive.
    fn degraded(reason: &str, platform: &str, session_kind: &str) -> Self {
        Ready {
            capabilities: Vec::new(),
            platform: platform.to_owned(),
            session_kind: session_kind.to_owned(),
            backend: ipc::BackendInfo {
                backend: platform.to_owned(),
                degraded_reason: reason.to_owned(),
                ..Default::default()
            },
        }
    }

    /// The `AgentReady` frame body.
    ///
    /// `Msg::Ready` is both the attach-time report and the answer to
    /// `PrepareInject` («агент ответил `PrepareInject`»), so the same payload is sent
    /// in both places.
    pub fn to_ipc(&self) -> ipc::AgentReady {
        ipc::AgentReady {
            capabilities: self.capabilities.clone(),
            platform: self.platform.clone(),
            session_kind: self.session_kind.clone(),
            backend: Some(self.backend.clone()),
        }
    }
}

/// Decides whether this session runs a receiver-takeover detector, from `[input]` and the
/// session the probe chose.
///
/// `docs/03-architecture.md`, «Takeover на получателе»: the detector exists where the
/// backend can tell physical input from synthetic input — Windows (low-level hooks drop the
/// OS injected flag and our `dwExtraInfo` tag) and X11 (raw events from an XTEST slave are
/// dropped) — and a portal receiver cannot, which is AC-13a's «автоматический takeover не
/// объявляется». `off` is honoured first, and `evdev` (the opt-in `/dev/input` reader,
/// KVM-028) is *not implemented in this build*: on a portal receiver that is a refusal with
/// `CONFIG_TAKEOVER`, and on Windows/X11 the native detector is used and the setting is
/// reported in the log, because there it answers the same question with fewer privileges.
fn takeover_plan(
    session: &SessionInfo,
    detection: ipc::TakeoverDetection,
    threshold_px: u32,
) -> (Option<TakeoverPlan>, Option<Refusal>) {
    // `Unspecified` (a core that spoke before this field existed) is treated like `Auto`,
    // which is the documented default of `[input].takeover_detection`.
    if detection == ipc::TakeoverDetection::Off {
        tracing::info!(
            "takeover_detection = \"off\": a receiver lease will not end on local input"
        );
        return (None, None);
    }
    if !session.capabilities.receiver_takeover {
        // AC-13a: a portal receiver sees only EIS injection, never its own devices.
        if detection == ipc::TakeoverDetection::Evdev {
            return (
                None,
                Some(Refusal {
                    code: "CONFIG_TAKEOVER",
                    detail: "the opt-in evdev detector is not implemented in this build, and a                              portal receiver cannot see physical input: automatic takeover is off                              for this node (docs/03-architecture.md, AC-13a)"
                        .to_owned(),
                }),
            );
        }
        tracing::info!(
            session = session.backend.name(),
            "this session cannot see physical input: a receiver lease will not end on local input"
        );
        return (None, None);
    }
    if detection == ipc::TakeoverDetection::Evdev {
        tracing::warn!(
            session = session.backend.name(),
            "takeover_detection = \"evdev\" asks for the /dev/input reader; on this session the              native detector is used instead (it sees keyboard and pointer events from the              kernel through the platform, without reading the input devices)"
        );
    }
    tracing::info!(
        session = session.backend.name(),
        threshold_px,
        "receiver takeover enabled: physical input above the threshold ends a lease"
    );
    (Some(TakeoverPlan { threshold_px }), None)
}

/// A backend that touches no OS input path.
///
/// Two callers use it: `--dry-inject` (deliberate, so the whole transition can be
/// exercised without a human at the keyboard) and a session this build cannot drive
/// (the node stays `Local` and keeps answering core, which is the documented
/// behaviour instead of exiting into a restart loop).
#[derive(Debug)]
pub struct Dry {
    ready: Ready,
    /// Synthetic events still to be reported (`--dry-input-events`, a smoke aid).
    scheduled: Vec<InputEvent>,
    /// One entry per poll tick: an event, or `None` for a tick that says nothing.
    ///
    /// Only `--dry-motion` fills this, and it is emitted whether or not a capture is
    /// armed: the whole point of the aid is that the pointer moves while the node is
    /// `Local`, and core's edge trigger is what notices it.
    motion: std::collections::VecDeque<Option<InputEvent>>,
    capturing: bool,
}

impl Dry {
    fn new(ready: Ready, synthetic: usize, motion: bool) -> Self {
        let mut scheduled = Vec::new();
        // Alternating down/up of HID usage 0x04: visibly synthetic, payload-free and
        // never text. It exists to exercise the batch path without a human at the
        // keyboard; a real capture never uses it.
        for step in 0..synthetic {
            scheduled.push(InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state: if step % 2 == 0 {
                    input_model::KeyState::Down
                } else {
                    input_model::KeyState::Up
                },
            });
        }
        Dry {
            ready,
            scheduled,
            motion: if motion {
                motion_schedule()
            } else {
                std::collections::VecDeque::new()
            },
            capturing: false,
        }
    }
}

impl Backend {
    /// Probes the session, honours a core-supplied override and builds the backend.
    ///
    /// `dry` selects [`Backend::Dry`] whatever the session is; a session this build
    /// cannot drive also ends up in [`Backend::Dry`], but with no capabilities and
    /// the reason in `AgentReady.backend.degraded_reason`.
    /// `restore_tokens` are the portal tokens core persisted; only the Wayland
    /// backend uses them (`AgentAttach.restore_tokens`), so on other platforms the
    /// parameter is accepted and ignored.
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    #[allow(clippy::too_many_arguments)]
    pub async fn select(
        override_kind: BackendKindArg,
        dry: bool,
        synthetic_events: usize,
        dry_motion: bool,
        emergency_hotkey: &str,
        restore_tokens: &[ipc::PortalGrant],
        takeover_detection: ipc::TakeoverDetection,
        takeover_threshold_px: u32,
    ) -> (Self, Ready, Vec<Refusal>) {
        // The probe reads D-Bus and X11 before the session is even known, and its
        // zbus path is the *blocking* API, which builds a tokio runtime of its own:
        // calling it from inside ours panics with «Cannot start a runtime from within a
        // runtime» (measured on Linux). It is blocking work by nature, so it belongs on
        // a blocking thread, where that nested runtime is legal.
        let snapshot = match tokio::task::spawn_blocking(snapshot).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "reading the environment snapshot failed; the session is undetectable"
                );
                EnvSnapshot::new(if cfg!(windows) {
                    platform_probe::HostOs::Windows
                } else {
                    platform_probe::HostOs::Linux
                })
            }
        };
        let requested = override_kind.into_backend_override();
        match platform_probe::decide(&snapshot, requested) {
            Ok(session) => {
                let mut ready = Ready::from_session(&session, String::new());
                let (plan, takeover_refusal) =
                    takeover_plan(&session, takeover_detection, takeover_threshold_px);
                // The capabilities this agent reports must match what it will actually do:
                // with no detector there is no takeover, whatever the probe said.
                if plan.is_none() {
                    ready.backend.has_receiver_takeover = false;
                    ready
                        .capabilities
                        .retain(|name| name != "RECEIVER_TAKEOVER");
                }
                let refusals = takeover_refusal.into_iter().collect::<Vec<_>>();
                if dry {
                    return (
                        Backend::Dry(Box::new(Dry::new(
                            ready.clone(),
                            synthetic_events,
                            dry_motion,
                        ))),
                        ready,
                        refusals,
                    );
                }
                match session.backend {
                    platform_probe::Backend::Windows => {
                        #[cfg(windows)]
                        {
                            match win::Win::new(&ready, emergency_hotkey, plan) {
                                Ok(backend) => (Backend::Win(Box::new(backend)), ready, refusals),
                                Err(reason) => refuse(&snapshot, &reason),
                            }
                        }
                        #[cfg(not(windows))]
                        {
                            refuse(
                                &snapshot,
                                "the probe selected platform-win on a non-Windows host",
                            )
                        }
                    }
                    platform_probe::Backend::X11 => {
                        #[cfg(target_os = "linux")]
                        {
                            match x11::X11::new(&ready, emergency_hotkey, plan) {
                                Ok(backend) => (Backend::X11(Box::new(backend)), ready, refusals),
                                Err(reason) => refuse(&snapshot, &reason),
                            }
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            refuse(
                                &snapshot,
                                "the probe selected platform-x11 on a non-Linux host",
                            )
                        }
                    }
                    platform_probe::Backend::Wayland { .. } => {
                        #[cfg(target_os = "linux")]
                        {
                            match wayland::Wayland::connect(
                                &session,
                                emergency_hotkey,
                                restore_tokens,
                            )
                            .await
                            {
                                Ok((backend, ready, _)) => {
                                    (Backend::Wayland(Box::new(backend)), ready, refusals)
                                }
                                Err(refusal) => {
                                    let (backend, ready, _) = refuse(&snapshot, &refusal.detail);
                                    (backend, ready, vec![refusal])
                                }
                            }
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            refuse(
                                &snapshot,
                                "platform-wayland is a Linux-only backend (XDG portals and EIS)",
                            )
                        }
                    }
                }
            }
            Err(unsupported) => refuse(&snapshot, &unsupported.to_string()),
        }
    }

    /// The `AgentReady` body to send after the attach.
    pub fn ready(&self) -> Ready {
        match self {
            Backend::Dry(dry) => dry.ready.clone(),
            #[cfg(windows)]
            Backend::Win(backend) => backend.ready(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.ready(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.ready(),
        }
    }

    /// `ForgetPortalGrants`: drop in-memory portal tokens so the next session asks
    /// again. Core stays the owner of the persisted file.
    pub async fn forget_portal_grants(&mut self) {
        match self {
            Backend::Dry(_) => {}
            #[cfg(windows)]
            Backend::Win(_) => {}
            #[cfg(target_os = "linux")]
            Backend::X11(_) => {}
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.forget_portal_grants(),
        }
    }

    /// Whether the selected session produced any capability at all.
    pub fn has_capabilities(&self) -> bool {
        !self.ready().capabilities.is_empty()
    }

    /// The OS autorepeat timing the software engine follows (KVM-036).
    ///
    /// Windows answers from `SystemParametersInfo`; X11 would need `XkbGetControls` and
    /// reports the documented defaults (500 ms, 33/s) until that is wired — the guard's
    /// budget is what matters for AC-14 and it does not depend on the rate being exact.
    pub fn repeat_timing(&self) -> crate::repeat::RepeatTiming {
        match self {
            Backend::Dry(_) => crate::repeat::RepeatTiming::default(),
            #[cfg(windows)]
            Backend::Win(_) => {
                let (delay, speed) = platform_win::inject::keyboard_timing();
                crate::repeat::RepeatTiming::from_windows(delay, speed)
            }
            #[cfg(target_os = "linux")]
            Backend::X11(_) => crate::repeat::RepeatTiming::default(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(_) => crate::repeat::RepeatTiming::default(),
        }
    }

    /// Starts (or re-arms) capture.
    pub async fn begin_capture(&mut self, kind: CaptureKind) -> Result<(), String> {
        match self {
            Backend::Dry(dry) => {
                // `--dry-input-events` is documented as «emit this many synthetic key edges
                // after `BeginCapture(REMOTE)`»: the core drops captured input that arrives
                // outside a remote source lease, and `TEMPORARY` is exactly that window, so
                // emitting them there would consume the aid before the lease exists.
                dry.capturing = matches!(kind, CaptureKind::Remote);
                tracing::info!(?kind, "dry mode: capture begins (no OS hooks)");
                Ok(())
            }
            #[cfg(windows)]
            Backend::Win(backend) => backend.begin_capture(kind),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.begin_capture(kind),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.begin_capture(kind).await,
        }
    }

    /// Ends capture and releases everything this session-owned state holds.
    pub async fn end_capture(&mut self, replay_buffered_locally: bool) -> Result<Released, String> {
        match self {
            Backend::Dry(dry) => {
                if replay_buffered_locally {
                    tracing::info!("dry mode: buffered events would be replayed locally");
                }
                dry.capturing = false;
                Ok(Released::default())
            }
            #[cfg(windows)]
            Backend::Win(backend) => backend.end_capture(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.end_capture(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.end_capture(replay_buffered_locally).await,
        }
    }

    /// Prepares the injection path for a lease (`PrepareInject`).
    pub async fn prepare_inject(&mut self) -> Result<(), String> {
        match self {
            Backend::Dry(_) => Ok(()),
            #[cfg(windows)]
            Backend::Win(backend) => backend.prepare_inject(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.prepare_inject(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.prepare_inject().await,
        }
    }

    /// Injects one batch.
    pub async fn inject(&mut self, events: &[InputEvent]) -> Injected {
        match self {
            Backend::Dry(_) => {
                tracing::info!(
                    count = events.len(),
                    "dry mode: Inject logged, not injected"
                );
                Injected {
                    applied: events.len(),
                    os_error: String::new(),
                    actual: events.iter().rev().find_map(|event| match event {
                        InputEvent::PointerAbsolute(position) => Some(*position),
                        _ => None,
                    }),
                }
            }
            #[cfg(windows)]
            Backend::Win(backend) => backend.inject(events),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.inject(events),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.inject(events).await,
        }
    }

    /// Releases the state this session put into the desktop.
    pub async fn release_session(&mut self) -> Released {
        match self {
            Backend::Dry(_) => Released::default(),
            #[cfg(windows)]
            Backend::Win(backend) => backend.release_session(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.release_session(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.release_session(),
        }
    }

    /// The monitors of this node, for `DisplaysReport` (KVM-040).
    ///
    /// Dry mode reports nothing: it touches no OS input path, and inventing a display
    /// would make core's topology disagree with the machine.
    pub fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        match self {
            Backend::Dry(_) => Vec::new(),
            #[cfg(windows)]
            Backend::Win(backend) => backend.displays(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.displays(),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.displays(),
        }
    }

    /// `RECOVERY`: let go of exactly these keys and buttons (rule 7).
    pub async fn release_keys(&mut self, keys: &[HidKey], buttons: &[MouseButton]) -> Released {
        match self {
            Backend::Dry(_) => Released {
                keys: keys.len() as u32,
                buttons: buttons.len() as u32,
            },
            #[cfg(windows)]
            Backend::Win(backend) => backend.release_keys(keys, buttons),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.release_keys(keys, buttons),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.release_keys(keys, buttons).await,
        }
    }

    /// The next captured event, if any.
    ///
    /// Always returns within a few milliseconds so the session loop stays one
    /// `select!`; `None` means "nothing this time".
    pub async fn next_event(&mut self) -> Option<Event> {
        match self {
            Backend::Dry(dry) => {
                if dry.capturing && !dry.scheduled.is_empty() {
                    return Some(Event::Input(dry.scheduled.remove(0)));
                }
                if let Some(step) = dry.motion.pop_front() {
                    return match step {
                        Some(event) => Some(Event::Input(event)),
                        // A tick that only passes time (`--dry-motion`'s pauses).
                        None => {
                            tokio::time::sleep(POLL_TICK).await;
                            None
                        }
                    };
                }
                tokio::time::sleep(POLL_TICK).await;
                None
            }
            #[cfg(windows)]
            Backend::Win(backend) => backend.next_event().await,
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.next_event().await,
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.next_event().await,
        }
    }
}

/// The `--dry-motion` pattern, in the node's logical space.
///
/// Ten steps to the right from x = 1600 (crossing a 1920-wide display at its right edge),
/// a pause long enough for a full `Local -> Preparing -> Remote` transition, twenty steps
/// back to the left (the first of them takes the virtual pointer out of the target display
/// through the entry edge, which is the return), and a final pause so the transcript is
/// stable. `None` entries are ticks that pass the time.
///
/// The pattern starts with a lead-in pause: a peer that has not finished its handshake
/// cannot accept a transition yet, and the FSM refuses one with `ReceiverUnsupported`
/// (which is what a real deployment wants — the check is «проверить capability-биты до
/// EnterPrepare» — but a smoke wants the links up first).
fn motion_schedule() -> std::collections::VecDeque<Option<InputEvent>> {
    /// Height to walk along: the middle of a 1080-tall display.
    const Y: f64 = 540.0;
    /// One second at the loop's tick rate (`POLL_TICK` is 5 ms).
    const TRANSITION_PAUSE: usize = 200;
    /// Three seconds for the mesh to establish links and exchange `Hello`.
    const LEAD_IN: usize = 600;
    let mut schedule = std::collections::VecDeque::new();
    for _ in 0..LEAD_IN {
        schedule.push_back(None);
    }

    let mut x = 1600.0;
    for _ in 0..10 {
        schedule.push_back(Some(InputEvent::PointerAbsolute(
            input_model::Position::new(x, Y),
        )));
        x += 40.0;
    }
    for _ in 0..TRANSITION_PAUSE {
        schedule.push_back(None);
    }
    let mut x = 1960.0;
    for _ in 0..20 {
        schedule.push_back(Some(InputEvent::PointerAbsolute(
            input_model::Position::new(x, Y),
        )));
        x -= 40.0;
    }
    for _ in 0..400 {
        schedule.push_back(None);
    }
    schedule
}

/// The selected backend cannot be driven here: keep the node usable and say why.
fn refuse(snapshot: &EnvSnapshot, reason: &str) -> (Backend, Ready, Vec<Refusal>) {
    let (platform, session_kind) = describe_session(snapshot);
    tracing::warn!(
        reason,
        platform = platform.as_str(),
        session_kind = session_kind.as_str(),
        "session not supported by this build: staying local"
    );
    let ready = Ready::degraded(reason, &platform, &session_kind);
    (
        Backend::Dry(Box::new(Dry::new(ready.clone(), 0, false))),
        ready,
        Vec::new(),
    )
}

/// How long a non-blocking backend poll waits before reporting "no event".
///
/// It is the loop's tick: deadline expiry, the core-silence watchdog and the capture
/// queue are all serviced at this granularity, so it is kept well below the smallest
/// timing the documents name (challenge interval 100 ms, the 200 ms prepare budget,
/// the 300 ms silence limit).
pub(crate) const POLL_TICK: std::time::Duration = std::time::Duration::from_millis(5);

/// `AgentAttach.backend_override`, kept as the enum core sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendKindArg(pub i32);

impl BackendKindArg {
    fn into_backend_override(self) -> BackendOverride {
        match ipc::BackendKind::try_from(self.0).unwrap_or(ipc::BackendKind::Unspecified) {
            ipc::BackendKind::X11 => BackendOverride::X11,
            ipc::BackendKind::Wayland => BackendOverride::Wayland,
            // `AUTO` and an unset value both mean "decide from the environment".
            _ => BackendOverride::Auto,
        }
    }
}

/// `input_model::CaptureKind` from the wire number.
pub fn capture_kind_from_i32(value: i32) -> Result<CaptureKind, String> {
    match ipc::CaptureKind::try_from(value) {
        Ok(ipc::CaptureKind::Observe) => Ok(CaptureKind::Observe),
        Ok(ipc::CaptureKind::Temporary) => Ok(CaptureKind::Temporary),
        Ok(ipc::CaptureKind::Remote) => Ok(CaptureKind::Remote),
        other => Err(format!("unsupported BeginCapture.kind {other:?}")),
    }
}

/// The injected-event vocabulary: wire → normalised.
///
/// The inverse of `lws-core`'s `routing::to_local_event`; `input-model` cannot own
/// either direction because `protocol` already depends on it.
pub fn to_input_event(event: &ipc::LocalEvent) -> Option<InputEvent> {
    match event.ev.as_ref()? {
        ipc::local_event::Ev::Pointer(pointer) => Some(InputEvent::PointerAbsolute(Position::new(
            pointer.x, pointer.y,
        ))),
        ipc::local_event::Ev::Key(key) => Some(InputEvent::Key {
            key: HidKey {
                page: u16::try_from(key.usage_page).ok()?,
                usage: u16::try_from(key.usage).ok()?,
            },
            state: if key.down {
                input_model::KeyState::Down
            } else {
                input_model::KeyState::Up
            },
        }),
        ipc::local_event::Ev::Button(button) => Some(InputEvent::Button {
            button: match button.button {
                1 => MouseButton::Left,
                2 => MouseButton::Right,
                3 => MouseButton::Middle,
                4 => MouseButton::Back,
                5 => MouseButton::Forward,
                _ => return None,
            },
            state: if button.down {
                input_model::KeyState::Down
            } else {
                input_model::KeyState::Up
            },
        }),
        ipc::local_event::Ev::Scroll(scroll) => Some(InputEvent::Scroll {
            h120: scroll.h120,
            v120: scroll.v120,
        }),
    }
}

/// The session probe for this host.
///
/// `platform_probe::read_snapshot` reads the environment and the X11 extensions and
/// is Linux-only; on Windows the host OS *is* the session evidence, so the snapshot
/// only has to say which OS it was taken on.
#[cfg(target_os = "linux")]
fn snapshot() -> EnvSnapshot {
    platform_probe::read_snapshot()
}

#[cfg(not(target_os = "linux"))]
fn snapshot() -> EnvSnapshot {
    EnvSnapshot::new(platform_probe::HostOs::Windows)
}

/// The platform and session kind to report when the probe refused.
fn describe_session(snapshot: &EnvSnapshot) -> (String, String) {
    let wayland = snapshot.xdg_session_type.as_deref() == Some("wayland")
        || snapshot.wayland_display.is_some();
    let x11 = !wayland
        && (snapshot.xdg_session_type.as_deref() == Some("x11") || snapshot.display.is_some());
    let session_kind = if wayland {
        "wayland"
    } else if x11 {
        "x11"
    } else {
        match snapshot.host {
            platform_probe::HostOs::Windows => "windows",
            platform_probe::HostOs::Linux => "tty",
        }
    };
    let platform = match snapshot.host {
        platform_probe::HostOs::Windows => "platform-win",
        platform_probe::HostOs::Linux if session_kind == "wayland" => "platform-wayland",
        platform_probe::HostOs::Linux => "platform-x11",
    };
    (platform.to_owned(), session_kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_kinds_match_the_proto_numbers() {
        assert_eq!(capture_kind_from_i32(1), Ok(CaptureKind::Observe));
        assert_eq!(capture_kind_from_i32(2), Ok(CaptureKind::Temporary));
        assert_eq!(capture_kind_from_i32(3), Ok(CaptureKind::Remote));
        assert!(
            capture_kind_from_i32(0).is_err(),
            "UNSPECIFIED is not a kind"
        );
        assert!(capture_kind_from_i32(99).is_err());
    }

    #[tokio::test]
    async fn the_synthetic_aid_only_fires_once_capture_is_remote() {
        let ready = Ready::degraded("test", "platform-test", "windows");
        let mut backend = Backend::Dry(Box::new(Dry::new(ready, 3, false)));

        // `TEMPORARY` is the window in which core is not yet a remote source and drops
        // captured input, so the aid must not be spent there: it is documented as «after
        // `BeginCapture(REMOTE)`».
        backend
            .begin_capture(CaptureKind::Temporary)
            .await
            .expect("temporary capture");
        assert!(
            backend.next_event().await.is_none(),
            "nothing is emitted while the capture is temporary"
        );

        backend
            .begin_capture(CaptureKind::Remote)
            .await
            .expect("remote capture");
        assert!(
            matches!(backend.next_event().await, Some(Event::Input(_))),
            "the synthetic edges arrive after the remote capture begins"
        );
    }

    #[test]
    fn wire_events_become_normalised_events() {
        let key = ipc::LocalEvent {
            monotonic_us: 7,
            ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                usage_page: 7,
                usage: 4,
                down: true,
            })),
        };
        assert_eq!(
            to_input_event(&key),
            Some(InputEvent::Key {
                key: HidKey::keyboard(4),
                state: input_model::KeyState::Down,
            })
        );

        let pointer = ipc::LocalEvent {
            monotonic_us: 0,
            ev: Some(ipc::local_event::Ev::Pointer(ipc::PointerAbs {
                display_id: String::new(),
                x: 1.5,
                y: 2.5,
            })),
        };
        assert_eq!(
            to_input_event(&pointer),
            Some(InputEvent::PointerAbsolute(Position::new(1.5, 2.5)))
        );

        let empty = ipc::LocalEvent {
            monotonic_us: 0,
            ev: None,
        };
        assert_eq!(to_input_event(&empty), None);
    }

    #[test]
    fn auto_and_unset_override_both_mean_auto() {
        assert_eq!(
            BackendKindArg(ipc::BackendKind::Auto as i32).into_backend_override(),
            BackendOverride::Auto
        );
        assert_eq!(
            BackendKindArg(0).into_backend_override(),
            BackendOverride::Auto
        );
        assert_eq!(
            BackendKindArg(ipc::BackendKind::X11 as i32).into_backend_override(),
            BackendOverride::X11
        );
    }

    #[test]
    fn a_wayland_session_is_reported_as_wayland() {
        let snapshot = EnvSnapshot::from_vars(
            platform_probe::HostOs::Linux,
            [
                ("XDG_SESSION_TYPE", "wayland"),
                ("WAYLAND_DISPLAY", "wayland-0"),
            ],
        );
        assert_eq!(
            describe_session(&snapshot),
            ("platform-wayland".to_owned(), "wayland".to_owned())
        );
    }
}
