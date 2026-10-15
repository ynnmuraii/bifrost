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
//! Wayland is selected as a real portal-backed backend when `allow_portal_sessions`
//! is enabled (never in recovery); when suppressed it is refused explicitly.
//! Recovery agents must not open sessions or prompts that outlive the recovery attempt.

use clipboard::Marker;
use input_model::{CaptureKind, HidKey, InputEvent, MouseButton, Position};
use platform_probe::{BackendOverride, EnvSnapshot, SessionInfo};
use protocol::ipc;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[cfg(target_os = "linux")]
mod wayland;
#[cfg(windows)]
mod win;
#[cfg(all(windows, test))]
pub(crate) use win::SourceMotion;
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

/// A local clipboard change the user made (KVM-041/043, ADR-008).
///
/// Nothing here is logged: sizes and marker names only, never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardChange {
    /// The text now on the local CLIPBOARD selection.
    pub text: String,
    /// The secret markers the platform reported for it. The decision whether it may leave
    /// the machine belongs to `clipboard::policy` on core's side of the link; the agent only
    /// reports what it saw.
    pub markers: Vec<clipboard::Marker>,
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
        /// The current portal activation's actual position and accepted barrier edge.
        activation: ipc::PortalActivation,
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
/// Selects the startup progress shown for a portal request.
fn portal_progress_message(
    portal_interface: &str,
    purpose: &str,
    restoring: bool,
) -> ipc::agent_frame::Msg {
    let portal_interface = portal_interface.to_owned();
    let purpose = purpose.to_owned();
    if restoring {
        ipc::agent_frame::Msg::PortalPermissionRestoring(ipc::PortalPermissionRestoring {
            portal_interface,
            purpose,
        })
    } else {
        ipc::agent_frame::Msg::PortalPermission(ipc::PortalPermissionRequired {
            portal_interface,
            purpose,
        })
    }
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

    /// `CAPABILITY_TEXT_CLIPBOARD`, advertised exactly when the backend can own and read the
    /// local CLIPBOARD selection (ADR-008: the clipboard is what the *platform* can do, so the
    /// capability is the adapter's answer, not the probe's).
    pub(crate) fn set_text_clipboard(&mut self, available: bool) {
        self.capabilities.retain(|name| name != "TEXT_CLIPBOARD");
        if available {
            self.capabilities.push("TEXT_CLIPBOARD".to_owned());
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

impl Backend {
    /// Applies appearance settings without changing input ownership.
    pub fn set_effects(&mut self, edge_effects: bool, hide_cursor: bool) {
        match self {
            #[cfg(windows)]
            Self::Win(backend) => backend.set_effects(edge_effects, hide_cursor),
            #[cfg(target_os = "linux")]
            Self::Wayland(backend) => backend.set_effects(edge_effects, hide_cursor),
            _ => {}
        }
        let _ = (edge_effects, hide_cursor);
    }

    /// Cosmetic updates never enter the capture hook.
    pub fn edge_fx(&mut self, display: &str, update: edgefx::Update) {
        match self {
            #[cfg(windows)]
            Self::Win(backend) => backend.edge_fx(display, update),
            #[cfg(target_os = "linux")]
            Self::Wayland(backend) => backend.edge_fx(update),
            _ => {}
        }
        let _ = (display, update);
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
    /// Only `--dry-motion` fills this. A real agent only produces pointer events while a
    /// capture is armed, so this aid does the same: the steps wait in the queue until
    /// `begin_capture` arms it (the owner's `Observe` is a capture too) and stop flowing
    /// the moment capture is torn down. That is what lets a test see the difference between
    /// an observation core re-armed and one it let latch.
    motion: std::collections::VecDeque<Option<InputEvent>>,
    dry_displays: bool,
    /// `--dry-input-events` flows only under a `Remote` capture, matching the real hook.
    capturing: bool,
    /// Whether *any* capture is armed (the owner's `Observe`, a `Temporary`, a `Remote`).
    ///
    /// This is what [`Dry::next_event`] gates `--dry-motion` on; `end_capture` clears it,
    /// exactly as the real capture hook stops producing events when the OS hook is torn
    /// down.
    armed: bool,
    /// Optional no-desktop fault harness controls; never injects OS input.
    control_dir: Option<PathBuf>,
    local_passthrough: u64,
    control_pointer_jitter: bool,
    /// A test aid for the emergency path with no desktop: when `LWS_DRY_EMERGENCY_FILE`
    /// names a path and that file appears, the dry backend reports the emergency
    /// combination once (and removes the file). `None` disables the aid.
    emergency_file: Option<std::path::PathBuf>,
    /// The clipboard, when `--dry-clipboard-dir` named one. `None` is a dry agent with no
    /// clipboard at all, which is what it reports.
    clipboard: Option<DryClipboard>,
}

/// The `--dry-clipboard-dir` hook: a clipboard made of files.
///
/// `in.txt` is the outside world copying something — a local user copy, which is the only
/// thing that may surface as a change — while `out.txt` mirrors what this node's selection
/// holds, so a smoke can see what the agent applied without a desktop. `set_clipboard` writes
/// `out.txt` and never touches `in.txt`, which is what keeps "the user copied this" (a change
/// to `in.txt`) apart from "core sent this" (a change to `out.txt`) on one filesystem.
///
/// `in.markers`, read only when `in.txt` changes, is how a smoke expresses the copy a password
/// manager would mark (`AC-15`): the five names the IPC carries, separated by commas, spaces or
/// newlines, and an absent or empty file means "plain text". Anything else in it is ignored.
///
/// # Echo suppression
///
/// `in.txt` is only ever written by whoever is driving the smoke, so the way the outside world
/// reports "the selection now holds what you put there" (the OS notification this node caused)
/// is by writing that same text into `in.txt`. The text this backend last applied from core is
/// remembered in `applied`, and one matching observation is swallowed — which is the same rule
/// Windows implements with `GetClipboardSequenceNumber` and X11 with a comparison of the
/// observed text to the applied one. One observation, not all of them: a second identical write
/// with no intervening change is a user copy this hook cannot tell apart from a retry, and
/// reporting it is the safe direction (core's own echo history drops it downstream).
#[derive(Debug)]
struct DryClipboard {
    in_path: PathBuf,
    markers_path: PathBuf,
    out_path: PathBuf,
    /// The text this node's selection holds.
    text: String,
    /// The cheap identity of `in.txt` (`(mtime, length)`, or `None` when it is absent) as of
    /// the last look — what makes the poll a `stat` rather than a read.
    observed: Option<(Option<SystemTime>, u64)>,
    /// When `in.txt` was last looked at, so a 5 ms loop tick does not turn into 200 stats a
    /// second.
    last_poll: Option<Instant>,
    /// The payload this backend last applied from core.
    applied: Option<String>,
}

/// How often the dry hook is allowed to look at `in.txt`.
///
/// `pub(crate)` so the session's tests can wait it out instead of guessing.
pub(crate) const DRY_CLIPBOARD_POLL: Duration = Duration::from_millis(50);

/// The cheap identity of a file: its modification time and length, or `None` when it is not
/// there. Enough to notice a copy without reading the payload every tick.
fn file_stamp(path: &std::path::Path) -> Option<(Option<SystemTime>, u64)> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| (meta.modified().ok(), meta.len()))
}

/// The markers `<dir>/in.markers` names, in the order it names them.
///
/// The tokens are separated by anything whitespace, a comma or a semicolon; a name outside the
/// five the IPC carries is ignored (and logged at debug), and a file that is absent, unreadable
/// or empty means "no markers" — the same tolerance core's own marker decoding has for the wire.
fn read_markers(path: &std::path::Path) -> Vec<Marker> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut markers: Vec<Marker> = Vec::new();
    for token in text.split([',', ';', ' ', '\t', '\r', '\n']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match crate::clipboard_wire::marker_from_name(token) {
            Some(marker) if !markers.contains(&marker) => markers.push(marker),
            Some(_) => {}
            None => tracing::debug!(
                name = token,
                "ignoring an unknown marker name in in.markers"
            ),
        }
    }
    markers
}

impl DryClipboard {
    /// The hook for one directory, which is created when it is not there.
    ///
    /// Whatever `in.txt` holds at this moment is the selection as of now: it is *not* a change,
    /// so it is captured as the baseline instead of being reported.
    fn new(dir: PathBuf) -> Self {
        let in_path = dir.join("in.txt");
        let markers_path = dir.join("in.markers");
        let out_path = dir.join("out.txt");
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                %error,
                dir = dir.display().to_string(),
                "cannot create the dry clipboard directory"
            );
        }
        let observed = file_stamp(&in_path);
        let text = std::fs::read_to_string(&in_path).unwrap_or_default();
        let clipboard = DryClipboard {
            in_path,
            markers_path,
            out_path,
            text,
            observed,
            last_poll: None,
            applied: None,
        };
        clipboard.write_out();
        tracing::info!(
            dir = dir.display().to_string(),
            bytes = clipboard.text.len(),
            "dry clipboard armed: in.txt is a local copy, out.txt is the selection"
        );
        clipboard
    }

    /// Mirrors the selection into `out.txt`. Never logs the text, only its size.
    fn write_out(&self) {
        if let Err(error) = std::fs::write(&self.out_path, &self.text) {
            tracing::warn!(
                %error,
                path = self.out_path.display().to_string(),
                "cannot write the dry clipboard's out.txt"
            );
        }
    }

    /// Looks for a copy the user made, at most every [`DRY_CLIPBOARD_POLL`].
    fn poll(&mut self) -> Option<ClipboardChange> {
        self.poll_at(Instant::now())
    }

    fn poll_at(&mut self, now: Instant) -> Option<ClipboardChange> {
        if let Some(last) = self.last_poll {
            if now.duration_since(last) < DRY_CLIPBOARD_POLL {
                return None;
            }
        }
        self.last_poll = Some(now);
        let current = file_stamp(&self.in_path);
        if current == self.observed {
            return None;
        }
        self.observed = current;
        // A selection that disappeared is not a copy; keep what the node holds.
        current?;
        let Ok(text) = std::fs::read_to_string(&self.in_path) else {
            tracing::warn!(
                path = self.in_path.display().to_string(),
                "cannot read the dry clipboard's in.txt; ignoring the change"
            );
            return None;
        };
        self.text = text.clone();
        if self.applied.as_deref() == Some(text.as_str()) {
            // The payload this node itself applied, showing up as a notification.
            self.applied = None;
            self.write_out();
            tracing::debug!(
                bytes = text.len(),
                "dry clipboard: swallowing its own payload"
            );
            return None;
        }
        // The markers belong to this copy, not to the file: they are read when `in.txt` changes
        // and only for a change that is reported (an echo carries the applied payload, whose
        // markers core already knows).
        let markers = read_markers(&self.markers_path);
        self.write_out();
        Some(ClipboardChange { text, markers })
    }
}

/// Replaces one dry control file that a test polls from another process.
///
/// `fs::write` truncates before it writes, so a reader polling every couple of
/// milliseconds can catch the empty window — and a counter caught that way is not even a
/// number. The replacement is atomic instead: a reader sees the previous text or the new
/// one, never a half-written file.
fn write_control(path: &Path, text: &str) -> std::io::Result<()> {
    let staging = path.with_extension("writing");
    std::fs::write(&staging, text)?;
    std::fs::rename(&staging, path)
}

/// Takes the dry wheel turn a test left in `path`: the signed notch count, with the file removed.
///
/// A writer that is not atomic (`fs::write`) is visible as an empty file for a moment; consuming
/// that would drop the turn and the writer's text would land in an unlinked file. Anything that
/// is not yet a number stays in place for the next poll.
fn take_wheel_control(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    let detents = text.trim().parse::<i32>().ok()?;
    std::fs::remove_file(path).ok()?;
    Some(detents)
}

impl Dry {
    fn new(
        ready: Ready,
        synthetic: usize,
        motion: bool,
        clipboard_dir: Option<PathBuf>,
        dry_displays: bool,
    ) -> Self {
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
        let clipboard = clipboard_dir.map(DryClipboard::new);
        let mut ready = ready;
        ready.set_text_clipboard(clipboard.is_some());
        Dry {
            ready,
            scheduled,
            motion: if motion && std::env::var_os("LWS_DRY_CONTROL_DIR").is_none() {
                motion_schedule()
            } else {
                std::collections::VecDeque::new()
            },
            capturing: false,
            armed: false,
            control_dir: std::env::var_os("LWS_DRY_CONTROL_DIR").map(PathBuf::from),
            local_passthrough: 0,
            control_pointer_jitter: false,
            emergency_file: std::env::var_os("LWS_DRY_EMERGENCY_FILE")
                .map(std::path::PathBuf::from),
            clipboard,
            dry_displays,
        }
    }

    fn write_capture_mode(&self, kind: CaptureKind) {
        if let Some(dir) = &self.control_dir {
            if let Err(error) = write_control(&dir.join("capture-mode.txt"), &format!("{kind:?}")) {
                tracing::warn!(%error, "cannot write dry capture mode");
            }
        }
    }

    async fn poll_control(&mut self) -> Option<InputEvent> {
        let dir = self.control_dir.as_ref()?;
        if std::fs::remove_file(dir.join("click")).is_ok() {
            if self.capturing {
                self.motion.push_front(Some(InputEvent::Button {
                    button: MouseButton::Left,
                    state: input_model::KeyState::Up,
                }));
                return Some(InputEvent::Button {
                    button: MouseButton::Left,
                    state: input_model::KeyState::Down,
                });
            }
            self.local_passthrough += 2;
            if let Err(error) = write_control(
                &dir.join("local-passthrough.txt"),
                &self.local_passthrough.to_string(),
            ) {
                tracing::warn!(%error, "cannot write dry local passthrough count");
            }
        }
        // A dry wheel, the way `click` is a dry click: the file holds the signed number of
        // notches, and every notch becomes its own event — a wheel run may not be merged, so
        // the queue hands them over one by one.
        if let Some(detents) = take_wheel_control(&dir.join("wheel")) {
            if self.capturing && detents != 0 {
                let step = detents.signum().saturating_mul(120);
                for _ in 1..detents.unsigned_abs() {
                    self.motion.push_back(Some(InputEvent::Scroll {
                        h120: 0,
                        v120: step,
                    }));
                }
                return Some(InputEvent::Scroll {
                    h120: 0,
                    v120: step,
                });
            }
            if !self.capturing {
                // Outside Remote the local desktop would have scrolled; this is the same
                // "the edge stayed local" observation the click makes.
                self.local_passthrough += 1;
                if let Err(error) = write_control(
                    &dir.join("local-passthrough.txt"),
                    &self.local_passthrough.to_string(),
                ) {
                    tracing::warn!(%error, "cannot write dry local passthrough count");
                }
            }
        }
        // Drain a control-generated event before generating another held-pointer sample.
        if matches!(
            self.motion.front(),
            Some(Some(InputEvent::Button { .. } | InputEvent::Scroll { .. }))
        ) {
            return self.motion.pop_front().flatten();
        }
        let text = std::fs::read_to_string(dir.join("held-pointer.txt")).ok()?;
        let mut coordinates = text.split_whitespace();
        let x: f64 = coordinates.next()?.parse().ok()?;
        let y: f64 = coordinates.next()?.parse().ok()?;
        self.control_pointer_jitter = !self.control_pointer_jitter;
        let sample_x = x + if self.control_pointer_jitter {
            1.0
        } else {
            0.0
        };
        if let Err(error) = write_control(&dir.join("pointer-observed.txt"), &text) {
            tracing::warn!(%error, "cannot acknowledge dry pointer sample");
        }
        tokio::time::sleep(POLL_TICK).await;
        Some(InputEvent::PointerAbsolute(Position::new(sample_x, y)))
    }

    /// Whether this dry agent has a clipboard at all (`--dry-clipboard-dir`).
    fn clipboard_available(&self) -> bool {
        self.clipboard.is_some()
    }

    /// A copy the user made, per [`DryClipboard::poll`].
    fn clipboard_change(&mut self) -> Option<ClipboardChange> {
        self.clipboard.as_mut().and_then(DryClipboard::poll)
    }

    /// Owns the selection: `Some` puts `text` on it, `None` releases it and forgets the text.
    ///
    /// Only `out.txt` is written — `in.txt` (and `in.markers`) belong to the outside world, and
    /// that is exactly what makes "core applied this" and "the user copied this" two different
    /// observations of the same directory.
    fn set_clipboard(&mut self, text: Option<&str>) -> Result<(), String> {
        let Some(clipboard) = self.clipboard.as_mut() else {
            return Err("CLIPBOARD_DISABLED: this dry agent has no --dry-clipboard-dir".to_owned());
        };
        match text {
            Some(text) => {
                clipboard.text = text.to_owned();
                clipboard.applied = Some(text.to_owned());
            }
            None => {
                clipboard.text.clear();
                // The selection is gone, so the payload is not "ours" any more: a later copy
                // of the same text is a real local copy.
                clipboard.applied = None;
            }
        }
        clipboard.write_out();
        tracing::info!(
            bytes = clipboard.text.len(),
            released = text.is_none(),
            "dry clipboard: selection set"
        );
        Ok(())
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
        dry_displays: bool,
        dry_motion: bool,
        dry_clipboard_dir: Option<PathBuf>,
        emergency_hotkey: &str,
        restore_tokens: &[ipc::PortalGrant],
        allow_portal_sessions: bool,
        portal_progress: tokio::sync::mpsc::UnboundedSender<ipc::agent_frame::Msg>,
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
                    let backend = Dry::new(
                        ready.clone(),
                        synthetic_events,
                        dry_motion,
                        dry_clipboard_dir,
                        dry_displays,
                    );
                    // The adapter is what knows whether it has a clipboard at all, so the
                    // report comes back from it rather than from the probe.
                    let ready = backend.ready.clone();
                    return (Backend::Dry(Box::new(backend)), ready, refusals);
                }
                match session.backend {
                    platform_probe::Backend::Windows => {
                        #[cfg(windows)]
                        {
                            match win::Win::new(&ready, emergency_hotkey, plan) {
                                Ok(backend) => {
                                    let ready = backend.ready();
                                    (Backend::Win(Box::new(backend)), ready, refusals)
                                }
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
                                Ok(backend) => {
                                    let ready = backend.ready();
                                    (Backend::X11(Box::new(backend)), ready, refusals)
                                }
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
                            if !allow_portal_sessions {
                                return refuse(
                                    &snapshot,
                                    "PORTAL_SESSION_SUPPRESSED: portal sessions are disabled in RECOVERY or after a portal failure; use manual retry to try again",
                                );
                            }
                            match wayland::Wayland::connect(
                                &session,
                                emergency_hotkey,
                                restore_tokens,
                                &portal_progress,
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
            Err(unsupported) if dry => unsupported_dry(
                &snapshot,
                &unsupported.to_string(),
                synthetic_events,
                dry_displays,
                dry_motion,
                dry_clipboard_dir,
            ),
            Err(unsupported) => refuse(&snapshot, &unsupported.to_string()),
        }
    }

    /// Whether this backend can own and read the local CLIPBOARD selection.
    ///
    /// The answer is the adapter's, not the probe's: a backend whose notification window or
    /// X connection could not be established reports `false` here and therefore advertises no
    /// `TEXT_CLIPBOARD` (ADR-008 — the capability is a promise about what this node will do).
    pub fn clipboard_available(&self) -> bool {
        match self {
            Backend::Dry(dry) => dry.clipboard_available(),
            #[cfg(windows)]
            Backend::Win(backend) => backend.clipboard_available(),
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.clipboard_available(),
            // `platform-wayland` answers for itself: the portal clipboard is not implemented
            // in this build, so a Wayland session reports no clipboard (KVM-041).
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.clipboard_available(),
        }
    }

    /// Polls for a clipboard change the *user* made.
    ///
    /// Returns as soon as the platform has nothing to say: the session calls this from its own
    /// tick, so a blocking wait here would stall the deadline and the core-silence watchdog.
    /// A payload this agent itself applied from core (`set_clipboard`) is never reported —
    /// ADR-008's echo rule at the agent's edge, which each backend implements with the
    /// mechanism its platform offers (see `clipboard_change` in `backend/win.rs`,
    /// `backend/x11.rs` and [`DryClipboard`]).
    ///
    /// Async because the real backends do Win32/X work: the response rides a blocking
    /// thread (Windows) or the selection's own thread (X11), so the runtime thread — the
    /// poll tick, the IPC frames and the watchdog — never waits for the platform.
    pub async fn clipboard_change(
        &mut self,
    ) -> Result<Option<ClipboardChange>, clipboard::ClipboardError> {
        match self {
            Backend::Dry(dry) => Ok(dry.clipboard_change()),
            #[cfg(windows)]
            Backend::Win(backend) => backend.clipboard_change().await,
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => Ok(backend.clipboard_change().await),
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => Ok(backend.clipboard_change().await),
        }
    }

    /// Owns the local selection and puts `text` on it; `None` releases it and forgets the
    /// text.
    ///
    /// The error text is for the `AgentError.detail` of the frame core gets, never a payload.
    pub async fn set_clipboard(&mut self, text: Option<&str>) -> Result<(), String> {
        match self {
            Backend::Dry(dry) => dry.set_clipboard(text),
            #[cfg(windows)]
            Backend::Win(backend) => backend.set_clipboard(text).await,
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.set_clipboard(text).await,
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.set_clipboard(text).await,
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

    pub fn hook_edges(&self) -> Option<ipc::EdgeCounters> {
        match self {
            #[cfg(windows)]
            Backend::Win(backend) => backend.hook_edges().map(crate::events::edge_counters),
            _ => None,
        }
    }

    pub fn applied_edges(&self) -> Option<ipc::EdgeCounters> {
        match self {
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => {
                Some(crate::events::edge_counters(backend.applied_edges()))
            }
            _ => None,
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
    pub async fn begin_capture(
        &mut self,
        kind: CaptureKind,
        capture_injected: bool,
    ) -> Result<(), String> {
        let _ = capture_injected;
        match self {
            Backend::Dry(dry) => {
                // `--dry-input-events` is documented as «emit this many synthetic key edges
                // after `BeginCapture(REMOTE)`: the core drops captured input that arrives
                // outside a remote source lease, and `TEMPORARY` is exactly that window, so
                // emitting them there would consume the aid before the lease exists.
                dry.capturing = kind.is_suppressed();
                // Every kind of capture produces events while it is armed.
                // `Observe` included: that is what `--dry-motion` rides on.
                dry.armed = true;
                dry.write_capture_mode(kind);
                tracing::info!(?kind, "dry mode: capture begins (no OS hooks)");
                Ok(())
            }
            #[cfg(windows)]
            Backend::Win(backend) => backend.begin_capture(kind, capture_injected),
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
                dry.armed = false;
                dry.write_capture_mode(CaptureKind::Observe);
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
            Backend::Dry(dry) => {
                if let Some(dir) = &dry.control_dir {
                    use std::fmt::Write as _;
                    use std::io::Write as _;
                    // What this node actually applied, one line per event, in order: the wheel
                    // is recorded beside the buttons because it is the input whose delivery a
                    // test cannot infer from a balanced pair of edges.
                    let mut buttons = String::new();
                    let mut wheels = String::new();
                    for event in events {
                        match event {
                            InputEvent::Button { button, state } => {
                                let _ = writeln!(buttons, "{button:?} {state:?}");
                            }
                            InputEvent::Scroll { h120, v120 } => {
                                let _ = writeln!(wheels, "{h120} {v120}");
                            }
                            _ => {}
                        }
                    }
                    for (name, lines) in [
                        ("injected-buttons.txt", buttons),
                        ("injected-wheel.txt", wheels),
                    ] {
                        if lines.is_empty() {
                            continue;
                        }
                        if let Err(error) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join(name))
                            .and_then(|mut file| file.write_all(lines.as_bytes()))
                        {
                            tracing::warn!(%error, %name, "cannot record dry injected input");
                        }
                    }
                }
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
    /// Dry mode reports nothing unless `--dry-displays` explicitly opts into the synthetic
    /// monitor used by multi-core tests.
    /// Async because the real backends enumerate synchronously (GDI, X round-trips); the
    /// enumeration runs on a blocking thread so `QueryDisplays` never stalls the poll tick
    /// and the heartbeat on the way to its answer.
    pub async fn displays(&mut self) -> Vec<crate::displays::RawDisplay> {
        match self {
            Backend::Dry(dry) if dry.dry_displays => vec![crate::displays::RawDisplay {
                id: "dry-0".to_owned(),
                name: String::new(),
                logical: [0.0, 0.0, 1920.0, 1080.0],
                native: Some([0, 0, 1920, 1080]),
                scale: 1.0,
                primary: true,
            }],
            Backend::Dry(_) => Vec::new(),
            #[cfg(windows)]
            Backend::Win(backend) => backend.displays().await,
            #[cfg(target_os = "linux")]
            Backend::X11(backend) => backend.displays().await,
            #[cfg(target_os = "linux")]
            Backend::Wayland(backend) => backend.displays().await,
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
                // The no-desktop emergency aid: when the harness drops the marker file,
                // report the combination once (a real agent gets this from its capture
                // hook, which no dry session has).
                if let Some(path) = &dry.emergency_file {
                    if std::fs::remove_file(path).is_ok() {
                        dry.capturing = false;
                        dry.write_capture_mode(CaptureKind::Observe);
                        return Some(Event::Emergency);
                    }
                }
                if dry.armed {
                    if let Some(event) = dry.poll_control().await {
                        return Some(Event::Input(event));
                    }
                }
                if dry.capturing && !dry.scheduled.is_empty() {
                    return Some(Event::Input(dry.scheduled.remove(0)));
                }
                // Motion flows only while a capture is armed, like the real hook: a
                // torn-down (and not re-armed) observation must stop producing events, or
                // core would keep seeing an edge it can no longer act on.
                if dry.armed {
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

/// An explicitly requested dry backend remains useful when session detection fails.
fn unsupported_dry(
    snapshot: &EnvSnapshot,
    reason: &str,
    synthetic_events: usize,
    dry_displays: bool,
    dry_motion: bool,
    dry_clipboard_dir: Option<PathBuf>,
) -> (Backend, Ready, Vec<Refusal>) {
    let (platform, session_kind) = describe_session(snapshot);
    tracing::warn!(
        reason,
        platform = platform.as_str(),
        session_kind = session_kind.as_str(),
        "session not supported by this build: staying local"
    );
    let ready = Ready::degraded(reason, &platform, &session_kind);
    let backend = Dry::new(
        ready,
        synthetic_events,
        dry_motion,
        dry_clipboard_dir,
        dry_displays,
    );
    let ready = backend.ready.clone();
    (Backend::Dry(Box::new(backend)), ready, Vec::new())
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
        Backend::Dry(Box::new(Dry::new(ready.clone(), 0, false, None, false))),
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
    fn a_wheel_file_caught_empty_mid_write_is_left_for_the_next_poll() {
        let dir = std::env::temp_dir().join(format!("lws-wheel-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wheel");
        // What a truncating writer shows for a moment before its text lands.
        std::fs::write(&path, "").unwrap();
        assert_eq!(
            take_wheel_control(&path),
            None,
            "an empty file is not a turn yet"
        );
        assert!(path.exists(), "and it must not be consumed");
        std::fs::write(&path, "-3").unwrap();
        assert_eq!(take_wheel_control(&path), Some(-3));
        assert!(!path.exists());
        assert_eq!(take_wheel_control(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_restore_token_announces_restoration_instead_of_a_permission_dialog() {
        const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
        const INPUT_CAPTURE: &str = "org.freedesktop.portal.InputCapture";
        let restoring = portal_progress_message(REMOTE_DESKTOP, "inject", true);
        assert!(matches!(
            restoring,
            ipc::agent_frame::Msg::PortalPermissionRestoring(progress)
                if progress.portal_interface == REMOTE_DESKTOP
                    && progress.purpose == "inject"
        ));

        let permission = portal_progress_message(INPUT_CAPTURE, "capture", false);
        assert!(matches!(
            permission,
            ipc::agent_frame::Msg::PortalPermission(progress)
                if progress.portal_interface == INPUT_CAPTURE
                    && progress.purpose == "capture"
        ));
    }

    #[tokio::test]
    async fn dry_displays_returns_the_synthetic_monitor_only_when_enabled() {
        let ready = Ready::degraded("test", "platform-test", "windows");
        let mut backend = Backend::Dry(Box::new(Dry::new(ready.clone(), 0, false, None, true)));

        let displays = backend.displays().await;
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].id, "dry-0");
        assert_eq!(displays[0].logical, [0.0, 0.0, 1920.0, 1080.0]);

        let mut empty_backend = Backend::Dry(Box::new(Dry::new(ready, 0, false, None, false)));
        assert!(empty_backend.displays().await.is_empty());
    }
    #[test]
    fn explicit_dry_clipboard_survives_an_unsupported_session_probe() {
        let dir = temp_dir("unsupported");
        let snapshot = EnvSnapshot::new(if cfg!(windows) {
            platform_probe::HostOs::Windows
        } else {
            platform_probe::HostOs::Linux
        });
        let (backend, ready, _) = unsupported_dry(
            &snapshot,
            "probe could not detect the session",
            0,
            false,
            false,
            Some(dir.clone()),
        );

        assert!(backend.clipboard_available());
        assert!(ready
            .capabilities
            .iter()
            .any(|name| name == "TEXT_CLIPBOARD"));
        let Backend::Dry(mut backend) = backend else {
            panic!("an unsupported probe must remain a dry backend");
        };
        backend
            .set_clipboard(Some("received from peer"))
            .expect("the dry fallback applies a peer payload");
        assert_eq!(read(&dir.join("out.txt")), "received from peer");
    }

    /// A private directory under the system temp root, for the dry clipboard's files.
    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lws-agent-dry-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temporary directory must be creatable");
        dir
    }

    /// A dry backend with a clipboard in `dir`, as `--dry-inject --dry-clipboard-dir <dir>`.
    fn dry_with_clipboard(dir: &std::path::Path) -> Dry {
        Dry::new(
            Ready::degraded("test", "platform-test", "windows"),
            0,
            false,
            Some(dir.to_path_buf()),
            false,
        )
    }

    /// Waits out [`DRY_CLIPBOARD_POLL`] so the next poll looks at `in.txt`.
    fn wait_for_poll() {
        std::thread::sleep(DRY_CLIPBOARD_POLL + Duration::from_millis(10));
    }

    fn read(path: &std::path::Path) -> String {
        std::fs::read_to_string(path).expect("the dry clipboard file must be readable")
    }

    fn write(path: &std::path::Path, text: &str) {
        std::fs::write(path, text).expect("the dry clipboard file must be writable");
    }

    #[tokio::test]
    async fn dry_temporary_click_is_local_only_and_emergency_releases_immediately() {
        let dir = temp_dir("local-passthrough");
        let mut dry = Dry::new(
            Ready::degraded("test", "platform-test", "windows"),
            0,
            false,
            None,
            false,
        );
        dry.control_dir = Some(dir.clone());
        dry.emergency_file = Some(dir.join("emergency"));
        let mut backend = Backend::Dry(Box::new(dry));
        backend
            .begin_capture(CaptureKind::Temporary, false)
            .await
            .expect("prepare");
        write(&dir.join("click"), "");
        assert!(backend.next_event().await.is_none());
        assert_eq!(read(&dir.join("local-passthrough.txt")), "2");
        backend
            .begin_capture(CaptureKind::Remote, false)
            .await
            .expect("commit");
        assert!(
            backend.next_event().await.is_none(),
            "local click must not be buffered"
        );
        write(&dir.join("emergency"), "");
        assert!(matches!(backend.next_event().await, Some(Event::Emergency)));
        let Backend::Dry(dry) = &backend else {
            panic!("dry backend");
        };
        assert!(!dry.capturing, "release precedes emergency delivery");
        assert_eq!(read(&dir.join("capture-mode.txt")), "Observe");
        std::fs::remove_dir_all(dir).expect("remove test controls");
    }

    /// The dry hook: `out.txt` mirrors the selection, a payload applied from core is never
    /// reported as a local change, and a copy the user makes always is.
    #[test]
    fn the_dry_clipboard_mirrors_the_selection_and_swallows_its_own_payload() {
        let dir = temp_dir("echo");
        let in_path = dir.join("in.txt");
        let out_path = dir.join("out.txt");
        let mut dry = dry_with_clipboard(&dir);
        assert!(
            dry.clipboard_available(),
            "--dry-clipboard-dir arms the clipboard"
        );
        assert!(
            dry.ready
                .capabilities
                .iter()
                .any(|name| name == "TEXT_CLIPBOARD"),
            "and it is advertised: {:?}",
            dry.ready.capabilities
        );
        assert!(dry.clipboard_change().is_none(), "nothing copied yet");
        assert_eq!(read(&out_path), "", "the selection starts empty");

        dry.set_clipboard(Some("привет"))
            .expect("the selection is taken");
        assert_eq!(read(&out_path), "привет", "out.txt mirrors the selection");
        assert!(
            !in_path.exists(),
            "applying from core must not write in.txt: that file is the user's"
        );

        // The outside world reporting the item this node caused — what a platform sends as the
        // notification for our own `set` — is not a local change.
        write(&in_path, "привет");
        wait_for_poll();
        assert!(
            dry.clipboard_change().is_none(),
            "the payload this node applied is not a local copy"
        );

        // A real copy is, and it carries no marker when `in.markers` says nothing.
        write(&in_path, "copied by the user");
        wait_for_poll();
        let change = dry.clipboard_change().expect("a local copy");
        assert_eq!(change.text, "copied by the user");
        assert!(change.markers.is_empty());
        assert_eq!(read(&out_path), "copied by the user");

        // Releasing forgets the text: the selection is empty, and `in.txt` is untouched.
        dry.set_clipboard(None).expect("the selection is released");
        assert_eq!(read(&out_path), "");
        assert_eq!(read(&in_path), "copied by the user");
        assert_eq!(dry.clipboard_change(), None);

        // Filesystem operations and descheduling may take longer than the poll interval.
        // Drive the production pacing check with explicit timestamps, not wall-clock guesses.
        write(&in_path, "another copy");
        let clipboard = dry.clipboard.as_mut().expect("dry clipboard");
        let last = clipboard.last_poll.expect("previous poll");
        assert!(clipboard
            .poll_at(last + DRY_CLIPBOARD_POLL - Duration::from_nanos(1))
            .is_none());
        let boundary = last + DRY_CLIPBOARD_POLL;
        assert_eq!(
            clipboard
                .poll_at(boundary)
                .expect("copy at interval boundary")
                .text,
            "another copy"
        );
        assert!(clipboard.poll_at(boundary).is_none());
    }

    /// `in.markers` is what makes `AC-15` testable without a password manager: the names are
    /// the ones the IPC carries, and nothing else in the file is guessed at.
    #[test]
    fn the_marker_file_names_the_markers_of_a_copy() {
        let dir = temp_dir("markers");
        let in_path = dir.join("in.txt");
        let markers_path = dir.join("in.markers");
        let mut dry = dry_with_clipboard(&dir);

        write(&in_path, "secret");
        write(&markers_path, "kde_secret\n");
        wait_for_poll();
        let change = dry.clipboard_change().expect("a marked copy");
        assert_eq!(change.text, "secret");
        assert_eq!(change.markers, vec![Marker::KdeSecret]);

        // Separators may be commas, spaces or newlines, an unknown name is ignored, and a
        // repeated one is not reported twice.
        write(&in_path, "another secret");
        write(
            &markers_path,
            "exclude_from_monitor, no_history  viewer_ignore\nportal_secret\nnot_a_marker\nno_history\n",
        );
        wait_for_poll();
        let change = dry.clipboard_change().expect("a marked copy");
        assert_eq!(
            change.markers,
            vec![
                Marker::ExcludeFromMonitor,
                Marker::NoHistory,
                Marker::ViewerIgnore,
                Marker::PortalSecret
            ]
        );

        // An empty file means plain text, and a later copy with no markers at all is plain too.
        write(&in_path, "plain again");
        write(&markers_path, "");
        wait_for_poll();
        assert!(dry.clipboard_change().expect("a copy").markers.is_empty());
        std::fs::remove_file(&markers_path).expect("the marker file goes away");
        write(&in_path, "plain once more");
        wait_for_poll();
        assert!(dry.clipboard_change().expect("a copy").markers.is_empty());

        // Rewriting the marker file alone is not a copy: only `in.txt` starts one.
        write(&markers_path, "kde_secret");
        wait_for_poll();
        assert!(
            dry.clipboard_change().is_none(),
            "markers without a new in.txt are not a change"
        );
    }

    /// The dry backend without a directory keeps today's behaviour: no clipboard, no
    /// capability, and a refusal instead of a silent no-op.
    #[test]
    fn a_dry_session_without_a_directory_has_no_clipboard() {
        let mut dry = Dry::new(
            Ready::degraded("test", "platform-test", "windows"),
            0,
            false,
            None,
            false,
        );
        assert!(!dry.clipboard_available());
        assert!(dry.clipboard_change().is_none());
        assert!(dry.set_clipboard(Some("x")).is_err());
        assert!(dry.set_clipboard(None).is_err());
        assert!(
            !dry.ready
                .capabilities
                .iter()
                .any(|name| name == "TEXT_CLIPBOARD"),
            "no clipboard, no capability: {:?}",
            dry.ready.capabilities
        );
    }

    /// The probe's own clipboard bit is never trusted: the capability is added and removed by
    /// the adapter's answer.
    #[test]
    fn the_text_clipboard_capability_follows_the_adapter() {
        let mut ready = Ready::degraded("test", "platform-test", "windows");
        ready.capabilities.push("CAPTURE".to_owned());
        ready.set_text_clipboard(true);
        assert!(ready.capabilities.contains(&"TEXT_CLIPBOARD".to_owned()));
        // The probe may already have claimed it; the adapter's answer is what stands.
        ready.set_text_clipboard(true);
        assert_eq!(
            ready
                .capabilities
                .iter()
                .filter(|name| *name == "TEXT_CLIPBOARD")
                .count(),
            1,
            "the capability is never duplicated: {:?}",
            ready.capabilities
        );
        ready.set_text_clipboard(false);
        assert!(!ready.capabilities.contains(&"TEXT_CLIPBOARD".to_owned()));
        assert!(
            ready.capabilities.contains(&"CAPTURE".to_owned()),
            "no other capability is touched"
        );
    }

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
        let mut backend = Backend::Dry(Box::new(Dry::new(ready, 3, false, None, false)));

        // `TEMPORARY` is the window in which core is not yet a remote source and drops
        // captured input, so the aid must not be spent there: it is documented as «after
        // `BeginCapture(REMOTE)`».
        backend
            .begin_capture(CaptureKind::Temporary, false)
            .await
            .expect("temporary capture");
        assert!(
            backend.next_event().await.is_none(),
            "nothing is emitted while the capture is temporary"
        );

        backend
            .begin_capture(CaptureKind::Remote, false)
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
