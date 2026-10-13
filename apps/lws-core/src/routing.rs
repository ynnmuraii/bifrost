//! The pure mapping between the FSM's vocabulary and everything outside it.
//!
//! `lws-core` runs one event loop that has three foreign vocabularies around the
//! sans-IO `focus_router`:
//!
//! * [`fr::Output`] → [`Effect`]: what to do with each decision (network, agent,
//!   timer, notice). The network arm is translated by `transport`; the agent arm is
//!   translated here into `protocol::ipc` frames, which is the part worth testing
//!   without a socket or a child process.
//! * `protocol::ipc::AgentFrame` → [`AgentSignal`]: the events the FSM consumes.
//! * [`wire::Hello`]: what this node tells its peers about itself.
//!
//! A few IPC messages cannot answer the FSM on their own, which is why
//! [`AgentContext`] exists: `AgentReady` is the reply to `PrepareInject` (there is no
//! dedicated "ready" message in `ipc.proto`) and carries no lease id, and the
//! `InjectResult` status has to be derived from the batch that was offered.

use std::collections::BTreeMap;

use focus_router as fr;
use geometry::NodeId;
use input_model::{InputEvent, KeyState, MouseButton};
use platform_probe::Capabilities;
use protocol::ipc;
use protocol::wire;

/// One side effect the loop must perform.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// One message to one peer.
    Send { peer: NodeId, message: fr::Message },
    /// A message to every established link (`OwnerClaim`/`OwnerState`).
    Broadcast { message: fr::Message },
    /// One command to the agent; [`agent_command`] turns it into the IPC frame.
    Agent(fr::AgentCommand),
    /// Arm (or re-arm) one timer, on the loop's millisecond clock.
    Arm { timer: fr::Timer, deadline_ms: u64 },
    /// Cancel one timer.
    Cancel { timer: fr::Timer },
    /// Close one link with a reason.
    CloseLink { peer: NodeId, code: fr::ErrorCode },
    /// A state transition or a refusal, for the log and the control channel.
    Notice(fr::Notice),
    /// A bounded lease-liveness snapshot, never a wire or input event.
    LeaseDiagnostic(fr::LeaseDiagnosticSnapshot),
}

/// Classifies every `Output` the FSM produced, in order.
pub fn effects(outputs: &[fr::Output]) -> Vec<Effect> {
    outputs.iter().map(effect).collect()
}

fn effect(output: &fr::Output) -> Effect {
    match output {
        fr::Output::Send { peer, message } => Effect::Send {
            peer: *peer,
            message: message.clone(),
        },
        fr::Output::Broadcast(message) => Effect::Broadcast {
            message: message.clone(),
        },
        fr::Output::Agent(command) => Effect::Agent(command.clone()),
        fr::Output::ArmTimer { timer, deadline_ms } => Effect::Arm {
            timer: *timer,
            deadline_ms: *deadline_ms,
        },
        fr::Output::CancelTimer(timer) => Effect::Cancel { timer: *timer },
        fr::Output::CloseLink { peer, code } => Effect::CloseLink {
            peer: *peer,
            code: *code,
        },
        fr::Output::Notice(notice) => Effect::Notice(notice.clone()),
        fr::Output::LeaseDiagnostic(snapshot) => Effect::LeaseDiagnostic(*snapshot),
    }
}

/// `AgentCommand` → the IPC frame that carries it.
///
/// The mapping is the table in `docs/03-architecture.md` («Соответствие сетевым
/// сообщениям»): `PrepareInject` ← `EnterPrepare`, `ArmDeadline` ←
/// `EnterCommit`/`LeaseRenew`, `Inject` ← `InputBatch`, `ReleaseSession` ← `Leave`.
pub fn agent_command(command: &fr::AgentCommand) -> ipc::agent_frame::Msg {
    use ipc::agent_frame::Msg;
    match command {
        fr::AgentCommand::BeginCapture {
            kind,
            lease_id,
            self_timeout_ms,
        } => Msg::BeginCapture(ipc::BeginCapture {
            kind: capture_kind_to_i32(*kind),
            // `ipc.proto`: `lease_id` is «16 bytes for TEMPORARY/REMOTE» — an
            // observation has no lease, and an empty id is what makes its `CaptureReady`
            // answer not look like a transition (`agent_signal` ignores it).
            lease_id: if *kind == input_model::CaptureKind::Observe {
                Vec::new()
            } else {
                lease_id.to_vec()
            },
            // `ipc.proto`: the agent ends capture by itself, max 1000 ms.
            self_timeout_ms: u32::try_from(*self_timeout_ms)
                .unwrap_or(u32::MAX)
                .min(1000),
            dev_capture_injected: false,
        }),
        fr::AgentCommand::EndCapture {
            lease_id,
            replay_buffered_locally,
        } => Msg::EndCapture(ipc::EndCapture {
            lease_id: lease_id.to_vec(),
            replay_buffered_locally: *replay_buffered_locally,
        }),
        fr::AgentCommand::PrepareInject { lease_id } => Msg::PrepareInject(ipc::PrepareInject {
            lease_id: lease_id.to_vec(),
        }),
        fr::AgentCommand::Inject {
            lease_id,
            input_batch_seq,
            events,
        } => Msg::Inject(ipc::Inject {
            lease_id: lease_id.to_vec(),
            input_batch_seq: *input_batch_seq,
            events: events.iter().filter_map(to_local_event).collect(),
        }),
        fr::AgentCommand::ArmDeadline {
            lease_id,
            remaining_ms,
        } => Msg::ArmDeadline(ipc::ArmDeadline {
            lease_id: lease_id.to_vec(),
            // Rule 4: `ArmDeadline.remaining_ms` is 1..=1000.
            remaining_ms: u32::try_from(*remaining_ms)
                .unwrap_or(u32::MAX)
                .clamp(1, 1000),
        }),
        fr::AgentCommand::ReleaseSession { lease_id } => Msg::ReleaseSession(ipc::ReleaseSession {
            lease_id: lease_id.to_vec(),
        }),
    }
}

/// The `ReleaseKeys` frame core builds from its conservative ledger (`RECOVERY`).
///
/// `docs/03-architecture.md`, scenario S4: core keeps «консервативное множество
/// отправленных down» precisely so a restarted agent can be told what to let go of.
pub fn release_keys(
    keys: &[input_model::HidKey],
    buttons: &[MouseButton],
) -> ipc::agent_frame::Msg {
    ipc::agent_frame::Msg::ReleaseKeys(ipc::ReleaseKeys {
        keys: keys
            .iter()
            .map(|key| ipc::HidKey {
                usage_page: u32::from(key.page),
                usage: u32::from(key.usage),
                // `ReleaseKeys` carries the keys to let go of; `down = false`.
                down: false,
            })
            .collect(),
        buttons: buttons
            .iter()
            .map(|button| button_to_u32(*button))
            .collect(),
    })
}

/// `AgentAttach`: the first frame, and the only place the launch token travels.
// One parameter per `AgentAttach` field: the function is the message, and grouping them
// would only hide which field a caller forgot.
#[allow(clippy::too_many_arguments)]
pub fn attach(
    launch_token: Vec<u8>,
    mode: ipc::AgentMode,
    emergency_hotkey: &str,
    backend_override: ipc::BackendKind,
    core_silence_ms: u32,
    require_emergency_hotkey: bool,
    restore_tokens: Vec<ipc::PortalGrant>,
    takeover_detection: ipc::TakeoverDetection,
    takeover_threshold_px: u32,
    repeat_mode: ipc::RepeatMode,
    allow_portal_sessions: bool,
) -> ipc::AgentAttach {
    ipc::AgentAttach {
        launch_token,
        mode: mode as i32,
        emergency_hotkey: emergency_hotkey.to_owned(),
        backend_override: backend_override as i32,
        // Wayland-only tokens, loaded from and persisted to `portal-restore.toml` by
        // [`crate::portal::PortalGrants`].
        restore_tokens,
        require_emergency_hotkey,
        core_silence_ms,
        takeover_detection: takeover_detection as i32,
        takeover_threshold_px,
        repeat_mode: repeat_mode as i32,
        allow_portal_sessions: allow_portal_sessions && mode != ipc::AgentMode::Recovery,
    }
}

/// The ownership and topology gossip `Hello` carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gossip {
    /// `Hello.owner_serial`.
    pub owner_serial: u64,
    /// `Hello.owner_node_id`.
    pub owner: Option<NodeId>,
    /// `Hello.topology_revision`.
    pub topology_revision: u64,
    /// `Hello.displays_revision`.
    pub displays_revision: u64,
}

/// The `Hello` this node sends on every link.
///
/// `docs/03-architecture.md`: «Core превращает это в `capabilities` для `Hello`;
/// ничего платформенного, кроме capability-бит и `session_kind`, по сети не
/// передаётся.» The capabilities come from the local probe, which both processes
/// run; the agent's own report refreshes them later through `AgentReady`.
pub fn hello(
    node_id: NodeId,
    display_name: &str,
    can_own: bool,
    capabilities: Capabilities,
    session_kind: wire::SessionKind,
    gossip: Gossip,
) -> wire::Hello {
    wire::Hello {
        node_id: node_id.as_bytes().to_vec(),
        display_name: display_name.to_owned(),
        capabilities: capability_bits(capabilities),
        displays: Vec::new(),
        required_capabilities: Vec::new(),
        session_kind: session_kind as i32,
        displays_revision: gossip.displays_revision,
        owner_serial: gossip.owner_serial,
        owner_node_id: gossip
            .owner
            .map(|id| id.as_bytes().to_vec())
            .unwrap_or_default(),
        can_own,
        topology_revision: gossip.topology_revision,
        topology_author: Vec::new(),
    }
}

/// `ArmDeadline.remaining_ms` for a local timer the *agent* enforces, or `None`.
///
/// Rule 4 puts the receiver's lease deadline on the agent's own clock and bounds the frame
/// at `1..=1000` ms (`docs/ipc.proto`), and the IPC mapping table of
/// `docs/03-architecture.md` asks for `ArmDeadline(issue_time + lease_ttl_ms − now)` on
/// every valid renew. The deadline itself stays in the core (the FSM owns it); this only
/// translates it for the clock that enforces it, so `EnterCommit` and each `LeaseRenew`
/// re-arm the agent with the remaining time instead of letting its one-shot frame from
/// `EnterCommit` fire `lease_ttl_ms` later.
pub fn deadline_rearm_ms(timer: fr::Timer, deadline_ms: u64, now_ms: u64) -> Option<u32> {
    match timer {
        fr::Timer::ReceiverLeaseDeadline => Some(
            u32::try_from(deadline_ms.saturating_sub(now_ms))
                .unwrap_or(u32::MAX)
                .clamp(1, 1000),
        ),
        _ => None,
    }
}

/// The `Hello.capabilities` numbers for the bits the FSM and the product use.
pub fn capability_bits(capabilities: Capabilities) -> Vec<i32> {
    let mut bits = Vec::new();
    if capabilities.capture {
        bits.push(wire::Capability::Capture as i32);
    }
    if capabilities.inject {
        bits.push(wire::Capability::Inject as i32);
    }
    if capabilities.absolute_pointer {
        bits.push(wire::Capability::AbsolutePointer as i32);
    }
    if capabilities.physical_hid_keys {
        bits.push(wire::Capability::PhysicalHidKeys as i32);
    }
    if capabilities.text_clipboard {
        bits.push(wire::Capability::TextClipboard as i32);
    }
    if capabilities.native_repeat {
        bits.push(wire::Capability::NativeRepeat as i32);
    }
    if capabilities.software_repeat {
        bits.push(wire::Capability::SoftwareRepeat as i32);
    }
    if capabilities.receiver_takeover {
        bits.push(wire::Capability::ReceiverTakeover as i32);
    }
    if capabilities.displays_changed {
        bits.push(wire::Capability::DisplaysChanged as i32);
    }
    if capabilities.emergency_hotkey {
        bits.push(wire::Capability::EmergencyHotkey as i32);
    }
    bits
}

/// Normalised event → `ipc.proto` `LocalEvent`, `None` when it has no wire form.
///
/// The inverse of `lws-agent`'s `events::to_local_event`; `input-model` cannot own
/// either direction because `protocol` already depends on it.
pub fn to_local_event(event: &InputEvent) -> Option<ipc::LocalEvent> {
    let ev = match event {
        InputEvent::PointerAbsolute(position) => ipc::local_event::Ev::Pointer(ipc::PointerAbs {
            // The source integrates deltas into an absolute position and the receiver
            // resolves the display from its own zones/geometry
            // (`transport::convert` documents the same for `InputBatch`).
            display_id: String::new(),
            x: position.x,
            y: position.y,
        }),
        InputEvent::PointerRelative { .. } => return None,
        InputEvent::Key { key, state } => ipc::local_event::Ev::Key(ipc::HidKey {
            usage_page: u32::from(key.page),
            usage: u32::from(key.usage),
            down: *state == KeyState::Down,
        }),
        InputEvent::Button { button, state } => ipc::local_event::Ev::Button(ipc::ButtonEv {
            button: button_to_u32(*button),
            down: *state == KeyState::Down,
        }),
        InputEvent::Scroll { h120, v120 } => ipc::local_event::Ev::Scroll(ipc::ScrollEv {
            h120: *h120,
            v120: *v120,
        }),
    };
    Some(ipc::LocalEvent {
        // Diagnostic only: nothing may derive timing from it (`transport::convert`).
        monotonic_us: 0,
        ev: Some(ev),
    })
}

/// `capture_kind` as the wire number.
pub const fn capture_kind_to_i32(kind: input_model::CaptureKind) -> i32 {
    match kind {
        input_model::CaptureKind::Observe => ipc::CaptureKind::Observe as i32,
        input_model::CaptureKind::Temporary => ipc::CaptureKind::Temporary as i32,
        input_model::CaptureKind::Remote => ipc::CaptureKind::Remote as i32,
    }
}

/// `MouseButton` as the wire number (`ipc.proto` `ButtonEv.button`).
pub const fn button_to_u32(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
    }
}

/// What an incoming agent frame means for the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentSignal {
    /// Feed the FSM.
    Fsm(fr::AgentEvent),
    /// A local condition the FSM models as `LocalInput`.
    Local(fr::LocalInput),
    /// Captured input to forward on the active source lease.
    /// Captured input, in its **wire** form.
    ///
    /// The display id matters: a pointer position is display-local, and the edge trigger
    /// has to know which display the step happened on to resolve it against the layout
    /// document. `input_model::InputEvent` carries no display id, so the wire event is
    /// what travels here and the session converts it for the `InputBatch` it forwards.
    Captured(ipc::CapturedBatch),
    /// The agent's display set changed.
    Displays(ipc::DisplaysReport),
    /// A portal dialog is expected because no persisted grant was available.
    Portal { interface: String, detail: String },
    /// A persisted portal grant is being restored; no dialog is expected.
    PortalRestoring { interface: String, detail: String },
    /// A portal restore token the agent received or lost; the session stores it.
    Grant(ipc::PortalGrantChanged),
    /// Nothing for the loop beyond the watchdog having seen the frame.
    Ignored,
}

/// The state an incoming frame needs to answer the FSM completely.
///
/// One field is one gap in `ipc.proto`: `AgentReady` is reused as the reply to
/// `PrepareInject` and carries no lease id, and `InjectResult` reports only how many
/// events were applied, so the batch that was offered has to be remembered to tell
/// `Ok` from `Partial`.
#[derive(Debug, Default)]
pub struct AgentContext {
    prepared: Option<[u8; 16]>,
    /// Events offered per `Inject` batch sequence.
    ///
    /// One slot was not enough: a captured burst is several batches in flight (the agent
    /// answers one `InjectResult` per `Inject`, in order), and with only the newest count
    /// remembered every earlier result was compared against the wrong batch and reported
    /// `Partial` — which *aborts the lease* (`InjectStatus::aborts_lease`). The map is
    /// keyed by the sequence the result carries, and each entry is taken when its result
    /// arrives, so it only holds the batches still in flight.
    offered: BTreeMap<u64, usize>,
}

impl AgentContext {
    /// Remembers what an outgoing command implies about the next incoming frames.
    pub fn on_command(&mut self, command: &fr::AgentCommand) {
        match command {
            fr::AgentCommand::PrepareInject { lease_id } => self.prepared = Some(*lease_id),
            fr::AgentCommand::Inject {
                input_batch_seq,
                events,
                ..
            } => {
                self.offered.insert(*input_batch_seq, events.len());
                // A batch the agent never answers (it died mid-flight) must not keep the
                // map growing: the newest 64 batches are more than one lease can have in
                // flight.
                while self.offered.len() > 64 {
                    self.offered.pop_first();
                }
            }
            _ => {}
        }
    }

    /// How many events the `Inject` batch with this sequence offered, if it is still in
    /// flight; the entry is consumed, because the agent answers each `Inject` once.
    fn take_offered(&mut self, input_batch_seq: u64) -> Option<usize> {
        self.offered.remove(&input_batch_seq)
    }
}

/// Interprets one agent frame.
pub fn agent_signal(frame: &ipc::agent_frame::Msg, context: &mut AgentContext) -> AgentSignal {
    use ipc::agent_frame::Msg;
    match frame {
        // `Ready` is the reply to `PrepareInject` when one is outstanding, and the
        // attach-time `AgentReady` otherwise (the first frame of a session).
        Msg::Ready(ready) => match context.prepared.take() {
            Some(lease_id) => AgentSignal::Fsm(fr::AgentEvent::Ready { lease_id }),
            None => {
                tracing::debug!(
                    platform = ready.platform.as_str(),
                    session_kind = ready.session_kind.as_str(),
                    capabilities = ready.capabilities.len(),
                    "agent ready"
                );
                AgentSignal::Fsm(fr::AgentEvent::AgentReady)
            }
        },
        Msg::CaptureReady(ready) => match lease_id(&ready.lease_id) {
            Some(lease_id) => AgentSignal::Fsm(fr::AgentEvent::CaptureReady { lease_id }),
            None => AgentSignal::Ignored,
        },
        Msg::Captured(batch) => AgentSignal::Captured(batch.clone()),
        Msg::Released(released) => match lease_id(&released.lease_id) {
            // `ReleaseKeys` answers with an empty lease id: it is a RECOVERY ack and
            // never drives the FSM (the counts are logged by the caller).
            None => AgentSignal::Ignored,
            Some(lease_id) => AgentSignal::Fsm(fr::AgentEvent::Released { lease_id }),
        },
        Msg::DeadlineExpired(expired) => match lease_id(&expired.lease_id) {
            Some(lease_id) => AgentSignal::Fsm(fr::AgentEvent::DeadlineExpired { lease_id }),
            None => AgentSignal::Ignored,
        },
        Msg::InjectResult(result) => match lease_id(&result.lease_id) {
            Some(lease_id) => {
                let offered = context.take_offered(result.input_batch_seq);
                AgentSignal::Fsm(fr::AgentEvent::InjectResult {
                    lease_id,
                    input_batch_seq: result.input_batch_seq,
                    applied_count: result.applied_count,
                    actual_position: result.actual.as_ref().map(|pointer| {
                        geometry::Position::new("".to_owned(), pointer.x, pointer.y)
                    }),
                    status: inject_status(result.applied_count, offered, &result.os_error),
                })
            }
            None => AgentSignal::Ignored,
        },
        Msg::LocalInput(detected) => match lease_id(&detected.lease_id) {
            Some(lease_id) => AgentSignal::Fsm(fr::AgentEvent::LocalInputDetected { lease_id }),
            None => AgentSignal::Ignored,
        },
        Msg::Emergency(_) => {
            // Rule 3: the agent has already restored local control. The FSM's
            // emergency path is what sends `Leave{EMERGENCY}` to the peer.
            AgentSignal::Local(fr::LocalInput::Emergency)
        }
        Msg::SessionEvent(event) => session_signal(event.kind),
        Msg::Displays(report) => AgentSignal::Displays(report.clone()),
        Msg::PortalPermission(permission) => AgentSignal::Portal {
            interface: permission.portal_interface.clone(),
            detail: format!("portal permission required for {}", permission.purpose),
        },
        Msg::PortalPermissionRestoring(progress) => AgentSignal::PortalRestoring {
            interface: progress.portal_interface.clone(),
            detail: format!("restoring portal permission for {}", progress.purpose),
        },
        // The token itself is a secret: it is stored, never logged, so this signal
        // carries the message rather than a description of it.
        Msg::PortalGrant(changed) => AgentSignal::Grant(changed.clone()),
        Msg::Error(error) => AgentSignal::Fsm(fr::AgentEvent::Error {
            lease_id: context.prepared.take(),
            code: error_code(&error.code),
        }),
        Msg::Pong(_) => AgentSignal::Ignored,
        // The clipboard IPC never drives the focus FSM: `ClipboardChanged` is offered to the
        // peers on the clipboard channel and `SetClipboardText` is addressed to the agent, so
        // both are the session loop's business and arrive here only to be classified.
        Msg::ClipboardChanged(_) | Msg::SetClipboardText(_) => AgentSignal::Ignored,
        // Core → agent frames arriving from the agent are a protocol error; the
        // loop's watchdog still counted the frame as liveness.
        _ => AgentSignal::Fsm(fr::AgentEvent::Error {
            lease_id: None,
            code: fr::ErrorCode::InvalidFrame,
        }),
    }
}

/// The human-facing note an agent frame implies, or `None` when it says nothing new.
///
/// This is what `status` shows next to the node: the last `AgentError` the agent
/// reported (`PORTAL_DENIED`, `CAPTURE_FAILED`, …) and the portal hints
/// (`PortalPermissionRequired`, `PortalPermissionRestoring`, `PortalGrantChanged`). The text is derived
/// here from the frame alone, so it can be unit-tested; the token in `PortalGrantChanged`
/// is never part of it.
pub fn agent_note(message: &ipc::agent_frame::Msg) -> Option<String> {
    use ipc::agent_frame::Msg;

    /// Keep the one-line status one line.
    const LIMIT: usize = 120;

    let shorten = |text: &str| -> String {
        if text.chars().count() <= LIMIT {
            return text.to_owned();
        }
        text.chars().take(LIMIT).collect::<String>() + "…"
    };

    match message {
        Msg::PortalPermission(permission) => Some(shorten(&format!(
            "permission required for {} ({})",
            permission.portal_interface, permission.purpose
        ))),
        Msg::PortalPermissionRestoring(progress) => Some(shorten(&format!(
            "restoring portal grant for {} ({})",
            progress.portal_interface, progress.purpose
        ))),
        Msg::PortalGrant(changed) => {
            let interface = changed
                .grant
                .as_ref()
                .map(|grant| grant.portal_interface.as_str())
                .unwrap_or("");
            Some(if changed.revoked {
                format!("portal grant revoked: {interface}")
            } else if changed.grant.is_some() {
                format!("portal grant received: {interface}")
            } else {
                "portal grants cleared".to_owned()
            })
        }
        Msg::Error(error) => Some(shorten(&format!("{}: {}", error.code, error.detail.trim()))),
        _ => None,
    }
}

/// `SessionEvent` → the FSM's local input.
pub fn session_signal(kind: i32) -> AgentSignal {
    match ipc::SessionEventKind::try_from(kind) {
        Ok(
            ipc::SessionEventKind::SessionEventSuspend | ipc::SessionEventKind::SessionEventLock,
        ) => AgentSignal::Local(fr::LocalInput::SuspendOrLock),
        Ok(
            ipc::SessionEventKind::SessionEventResume | ipc::SessionEventKind::SessionEventUnlock,
        ) => AgentSignal::Local(fr::LocalInput::Resume),
        _ => AgentSignal::Ignored,
    }
}

/// The `InjectAck`/`InjectResult` outcome.
///
/// `Partial`/`Failed` abort the lease, so the classification must not be generous:
/// anything short of the full batch with no error is not `Ok`.
pub fn inject_status(applied: u32, offered: Option<usize>, os_error: &str) -> fr::InjectStatus {
    if !os_error.is_empty() {
        return if applied == 0 {
            fr::InjectStatus::Failed
        } else {
            fr::InjectStatus::Partial
        };
    }
    match offered {
        Some(offered) if applied as usize >= offered => fr::InjectStatus::Ok,
        Some(0) if applied == 0 => fr::InjectStatus::Ok,
        Some(_) if applied == 0 => fr::InjectStatus::Failed,
        Some(_) => fr::InjectStatus::Partial,
        // Without the batch that was offered there is nothing to compare against, and
        // claiming `Ok` would hide a partial injection.
        None if applied == 0 => fr::InjectStatus::Failed,
        None => fr::InjectStatus::Partial,
    }
}

/// `AgentError.code` → the FSM's code.
pub fn error_code(code: &str) -> fr::ErrorCode {
    match code {
        "CAPTURE_FAILED" | "QUEUE_OVERFLOW" => fr::ErrorCode::CaptureFailed,
        "UNSUPPORTED_SESSION" => fr::ErrorCode::UnsupportedSession,
        "INJECTION_FAILED" => fr::ErrorCode::InjectionFailed,
        "AGENT_LOST" => fr::ErrorCode::AgentLost,
        _ => fr::ErrorCode::InvalidFrame,
    }
}

/// The 16 bytes every lease-scoped message carries.
pub fn lease_id(bytes: &[u8]) -> Option<[u8; 16]> {
    bytes.try_into().ok()
}

/// `ipc.proto` `LocalEvent` → the normalised vocabulary.
pub fn input_event_of(event: &ipc::LocalEvent) -> Option<InputEvent> {
    match event.ev.as_ref()? {
        ipc::local_event::Ev::Pointer(pointer) => Some(InputEvent::PointerAbsolute(
            input_model::Position::new(pointer.x, pointer.y),
        )),
        ipc::local_event::Ev::Key(key) => Some(InputEvent::Key {
            key: input_model::HidKey {
                page: u16::try_from(key.usage_page).ok()?,
                usage: u16::try_from(key.usage).ok()?,
            },
            state: if key.down {
                KeyState::Down
            } else {
                KeyState::Up
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
                KeyState::Down
            } else {
                KeyState::Up
            },
        }),
        ipc::local_event::Ev::Scroll(scroll) => Some(InputEvent::Scroll {
            h120: scroll.h120,
            v120: scroll.v120,
        }),
    }
}

/// The wire name of a focus/lease message, for logs.
///
/// `docs/03-architecture.md` («Логи и diagnostic export»): «Никогда: нажатия клавиш,
/// содержимое clipboard, имена окон, ключи.» The name never carries a payload, so it
/// is safe at any level.
pub fn message_name(message: &fr::Message) -> &'static str {
    match message {
        fr::Message::EnterPrepare { .. } => "EnterPrepare",
        fr::Message::EnterAck { .. } => "EnterAck",
        fr::Message::EnterCommit { .. } => "EnterCommit",
        fr::Message::Leave { .. } => "Leave",
        fr::Message::LeaveAck { .. } => "LeaveAck",
        fr::Message::InputBatch { .. } => "InputBatch",
        fr::Message::InjectAck { .. } => "InjectAck",
        fr::Message::LeaseChallenge { .. } => "LeaseChallenge",
        fr::Message::LeaseRenew { .. } => "LeaseRenew",
        fr::Message::LeaseStatus { .. } => "LeaseStatus",
        fr::Message::OwnerClaim { .. } => "OwnerClaim",
        fr::Message::OwnerState { .. } => "OwnerState",
        fr::Message::ProtocolError { .. } => "ProtocolError",
        fr::Message::Shutdown { .. } => "Shutdown",
    }
}

/// The name of an IPC frame, for logs. Payload-free for the same reason.
pub fn agent_frame_name(frame: &ipc::agent_frame::Msg) -> &'static str {
    use ipc::agent_frame::Msg;
    match frame {
        Msg::Attach(_) => "AgentAttach",
        Msg::BeginCapture(_) => "BeginCapture",
        Msg::EndCapture(_) => "EndCapture",
        Msg::PrepareInject(_) => "PrepareInject",
        Msg::Inject(_) => "Inject",
        Msg::ArmDeadline(_) => "ArmDeadline",
        Msg::ReleaseSession(_) => "ReleaseSession",
        Msg::ReleaseKeys(_) => "ReleaseKeys",
        Msg::QueryDisplays(_) => "QueryDisplays",
        Msg::Shutdown(_) => "AgentShutdown",
        Msg::Ping(_) => "Ping",
        Msg::ForgetPortalGrants(_) => "ForgetPortalGrants",
        Msg::StopRepeat(_) => "StopRepeat",
        Msg::Ready(_) => "AgentReady",
        Msg::CaptureReady(_) => "CaptureReady",
        Msg::Captured(_) => "CapturedBatch",
        Msg::Emergency(_) => "EmergencyFired",
        Msg::InjectResult(_) => "InjectResult",
        Msg::DeadlineExpired(_) => "DeadlineExpired",
        Msg::Released(_) => "Released",
        Msg::Displays(_) => "DisplaysReport",
        Msg::LocalInput(_) => "LocalInputDetected",
        Msg::SessionEvent(_) => "SessionEvent",
        Msg::Pong(_) => "Pong",
        Msg::Error(_) => "AgentError",
        Msg::PortalPermission(_) => "PortalPermissionRequired",
        Msg::PortalPermissionRestoring(_) => "PortalPermissionRestoring",
        Msg::PortalGrant(_) => "PortalGrantChanged",
        Msg::SetClipboardText(_) => "SetClipboardText",
        Msg::ClipboardChanged(_) => "ClipboardChanged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(byte: u8) -> [u8; 16] {
        [byte; 16]
    }
    #[test]
    fn recovery_attach_never_allows_portal_sessions() {
        let make = |mode| {
            attach(
                vec![0; 32],
                mode,
                "",
                ipc::BackendKind::Auto,
                300,
                false,
                Vec::new(),
                ipc::TakeoverDetection::Auto,
                12,
                ipc::RepeatMode::ReceiverNative,
                true,
            )
        };
        assert!(!make(ipc::AgentMode::Recovery).allow_portal_sessions);
        assert!(make(ipc::AgentMode::Normal).allow_portal_sessions);
    }

    #[test]
    fn every_agent_command_has_a_frame() {
        let id = lease(1);
        let cases = [
            fr::AgentCommand::BeginCapture {
                kind: input_model::CaptureKind::Temporary,
                lease_id: id,
                self_timeout_ms: 200,
            },
            fr::AgentCommand::EndCapture {
                lease_id: id,
                replay_buffered_locally: true,
            },
            fr::AgentCommand::PrepareInject { lease_id: id },
            fr::AgentCommand::ArmDeadline {
                lease_id: id,
                remaining_ms: 500,
            },
            fr::AgentCommand::ReleaseSession { lease_id: id },
        ];
        for command in cases {
            let frame = agent_command(&command);
            assert!(!matches!(
                frame,
                ipc::agent_frame::Msg::Ready(_) | ipc::agent_frame::Msg::Pong(_)
            ));
        }

        match agent_command(&fr::AgentCommand::ArmDeadline {
            lease_id: id,
            remaining_ms: 5_000,
        }) {
            ipc::agent_frame::Msg::ArmDeadline(arm) => assert_eq!(arm.remaining_ms, 1000),
            other => panic!("expected ArmDeadline, got {other:?}"),
        }
        // The receiver's lease deadline is enforced by the agent (rule 4), so the core has
        // to translate every re-arm of it into a fresh `ArmDeadline`: the frame sent at
        // `EnterCommit` is a one-shot on the agent's clock, and a `LeaseRenew` that only
        // moved the core's timer would still let the agent's deadline fire.
        assert_eq!(
            deadline_rearm_ms(fr::Timer::ReceiverLeaseDeadline, 1_500, 1_000),
            Some(500),
            "a renew moves the deadline, so the agent gets the remaining time"
        );
        assert_eq!(
            deadline_rearm_ms(fr::Timer::ReceiverLeaseDeadline, 1_000, 1_000),
            Some(1),
            "rule 4: the frame is never 0"
        );
        assert_eq!(
            deadline_rearm_ms(fr::Timer::ReceiverLeaseDeadline, 9_000, 1_000),
            Some(1_000),
            "rule 4: the frame is never above 1000 ms"
        );
        assert_eq!(
            deadline_rearm_ms(fr::Timer::PrepareTimeout, 1_500, 1_000),
            None,
            "only the timer the agent enforces travels to it"
        );

        match agent_command(&fr::AgentCommand::BeginCapture {
            kind: input_model::CaptureKind::Remote,
            lease_id: id,
            self_timeout_ms: 9_000,
        }) {
            ipc::agent_frame::Msg::BeginCapture(begin) => {
                assert_eq!(begin.kind, 3, "REMOTE is 3 in ipc.proto");
                assert_eq!(begin.self_timeout_ms, 1000);
                assert_eq!(begin.lease_id, id.to_vec());
            }
            other => panic!("expected BeginCapture, got {other:?}"),
        }
    }

    #[test]
    fn outputs_become_effects_in_order() {
        let peer = NodeId::new([9; 32]);
        let outputs = vec![
            fr::Output::Agent(fr::AgentCommand::PrepareInject { lease_id: lease(2) }),
            fr::Output::Send {
                peer,
                message: fr::Message::EnterAck {
                    lease_id: lease(2),
                    accepted: true,
                    reason: None,
                },
            },
            fr::Output::ArmTimer {
                timer: fr::Timer::PrepareTimeout,
                deadline_ms: 1234,
            },
            fr::Output::CancelTimer(fr::Timer::PrepareTimeout),
            fr::Output::CloseLink {
                peer,
                code: fr::ErrorCode::LeaseMismatch,
            },
            fr::Output::Notice(fr::Notice::Focus(fr::FocusState::Preparing)),
        ];
        let effects = effects(&outputs);
        assert_eq!(effects.len(), outputs.len());
        assert!(matches!(
            effects[0],
            Effect::Agent(fr::AgentCommand::PrepareInject { .. })
        ));
        assert!(matches!(effects[1], Effect::Send { .. }));
        assert!(matches!(
            effects[2],
            Effect::Arm {
                timer: fr::Timer::PrepareTimeout,
                deadline_ms: 1234
            }
        ));
        assert!(matches!(effects[3], Effect::Cancel { .. }));
        assert!(matches!(effects[4], Effect::CloseLink { .. }));
        assert!(matches!(effects[5], Effect::Notice(_)));
    }

    #[test]
    fn a_ready_frame_is_the_prepare_inject_ack_only_while_one_is_outstanding() {
        let mut context = AgentContext::default();
        let ready = ipc::agent_frame::Msg::Ready(ipc::AgentReady {
            capabilities: vec!["CAPTURE".to_owned()],
            platform: "platform-win".to_owned(),
            session_kind: "windows".to_owned(),
            backend: None,
        });
        assert_eq!(
            agent_signal(&ready, &mut context),
            AgentSignal::Fsm(fr::AgentEvent::AgentReady)
        );

        context.on_command(&fr::AgentCommand::PrepareInject { lease_id: lease(3) });
        assert_eq!(
            agent_signal(&ready, &mut context),
            AgentSignal::Fsm(fr::AgentEvent::Ready { lease_id: lease(3) })
        );
        assert_eq!(
            agent_signal(&ready, &mut context),
            AgentSignal::Fsm(fr::AgentEvent::AgentReady),
            "the lease answer is consumed once"
        );
    }

    #[test]
    fn an_inject_result_is_measured_against_the_batch_that_was_offered() {
        let mut context = AgentContext::default();
        context.on_command(&fr::AgentCommand::Inject {
            lease_id: lease(4),
            input_batch_seq: 7,
            events: vec![
                InputEvent::Scroll { h120: 0, v120: 120 },
                InputEvent::Scroll {
                    h120: 0,
                    v120: -120,
                },
            ],
        });
        let result = |applied: u32, os_error: &str| {
            ipc::agent_frame::Msg::InjectResult(ipc::InjectResult {
                lease_id: lease(4).to_vec(),
                input_batch_seq: 7,
                applied_count: applied,
                os_error: os_error.to_owned(),
                actual: None,
            })
        };

        match agent_signal(&result(2, ""), &mut context) {
            AgentSignal::Fsm(fr::AgentEvent::InjectResult { status, .. }) => {
                assert_eq!(status, fr::InjectStatus::Ok);
            }
            other => panic!("expected an InjectResult, got {other:?}"),
        }
        match agent_signal(&result(1, ""), &mut context) {
            AgentSignal::Fsm(fr::AgentEvent::InjectResult { status, .. }) => {
                assert_eq!(status, fr::InjectStatus::Partial);
            }
            other => panic!("expected an InjectResult, got {other:?}"),
        }
        match agent_signal(&result(0, "ACCESS_DENIED"), &mut context) {
            AgentSignal::Fsm(fr::AgentEvent::InjectResult { status, .. }) => {
                assert_eq!(status, fr::InjectStatus::Failed);
            }
            other => panic!("expected an InjectResult, got {other:?}"),
        }
    }

    #[test]
    fn a_result_is_ok_while_an_earlier_batch_is_still_in_flight() {
        let mut context = AgentContext::default();
        for seq in [7u64, 8] {
            context.on_command(&fr::AgentCommand::Inject {
                lease_id: lease(4),
                input_batch_seq: seq,
                events: vec![InputEvent::Scroll { h120: 0, v120: 120 }],
            });
        }
        let result = |seq: u64| {
            ipc::agent_frame::Msg::InjectResult(ipc::InjectResult {
                lease_id: lease(4).to_vec(),
                input_batch_seq: seq,
                applied_count: 1,
                os_error: String::new(),
                actual: None,
            })
        };

        // The agent answers the first batch while the second is still in flight. One
        // remembered slot compared this result against batch 8 and reported `Partial`,
        // which aborts the lease (`InjectStatus::aborts_lease`).
        for seq in [7u64, 8] {
            match agent_signal(&result(seq), &mut context) {
                AgentSignal::Fsm(fr::AgentEvent::InjectResult { status, .. }) => {
                    assert_eq!(
                        status,
                        fr::InjectStatus::Ok,
                        "batch {seq} is measured against its own offer"
                    );
                }
                other => panic!("expected an InjectResult, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_status_never_calls_a_short_batch_ok() {
        assert_eq!(inject_status(3, Some(3), ""), fr::InjectStatus::Ok);
        assert_eq!(inject_status(2, Some(3), ""), fr::InjectStatus::Partial);
        assert_eq!(inject_status(0, Some(3), ""), fr::InjectStatus::Failed);
        assert_eq!(
            inject_status(1, Some(3), "some error"),
            fr::InjectStatus::Partial
        );
        assert_eq!(
            inject_status(0, Some(3), "some error"),
            fr::InjectStatus::Failed
        );
        assert_eq!(inject_status(1, None, ""), fr::InjectStatus::Partial);
        assert_eq!(inject_status(0, None, ""), fr::InjectStatus::Failed);
    }

    #[test]
    fn release_keys_is_a_recovery_ack_the_fsm_ignores() {
        let mut context = AgentContext::default();
        let released = ipc::agent_frame::Msg::Released(ipc::Released {
            // `ReleaseKeys` answers with an empty lease id.
            lease_id: Vec::new(),
            keys_released: 2,
            buttons_released: 1,
        });
        assert_eq!(agent_signal(&released, &mut context), AgentSignal::Ignored);
    }

    #[test]
    fn an_emergency_is_the_local_emergency_path() {
        let mut context = AgentContext::default();
        let emergency = ipc::agent_frame::Msg::Emergency(ipc::EmergencyFired {
            lease_id: lease(5).to_vec(),
        });
        assert_eq!(
            agent_signal(&emergency, &mut context),
            AgentSignal::Local(fr::LocalInput::Emergency)
        );
    }

    #[test]
    fn session_events_map_to_lock_and_resume() {
        assert_eq!(
            session_signal(ipc::SessionEventKind::SessionEventSuspend as i32),
            AgentSignal::Local(fr::LocalInput::SuspendOrLock)
        );
        assert_eq!(
            session_signal(ipc::SessionEventKind::SessionEventLock as i32),
            AgentSignal::Local(fr::LocalInput::SuspendOrLock)
        );
        assert_eq!(
            session_signal(ipc::SessionEventKind::SessionEventUnlock as i32),
            AgentSignal::Local(fr::LocalInput::Resume)
        );
    }

    #[test]
    fn captured_events_lose_only_their_display_id() {
        let mut context = AgentContext::default();
        let batch = ipc::agent_frame::Msg::Captured(ipc::CapturedBatch {
            events: vec![ipc::LocalEvent {
                monotonic_us: 0,
                ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                    usage_page: 7,
                    usage: 0x04,
                    down: true,
                })),
            }],
            at_edge: true,
            activation: None,
        });
        match agent_signal(&batch, &mut context) {
            // The wire form is kept on purpose: the display id is what the edge trigger
            // needs and `input_model::InputEvent` has no room for it.
            AgentSignal::Captured(events) => {
                let events = events.events;
                assert_eq!(events.len(), 1);
                match events[0].ev.as_ref() {
                    Some(ipc::local_event::Ev::Key(key)) => {
                        assert_eq!(key.usage, 0x04);
                        assert!(key.down);
                    }
                    other => panic!("expected a key event, got {other:?}"),
                }
                // The normalised form is still one call away, which is what the
                // `InputBatch` core forwards needs.
                assert_eq!(
                    input_event_of(&events[0]),
                    Some(InputEvent::Key {
                        key: input_model::HidKey::keyboard(0x04),
                        state: KeyState::Down,
                    })
                );
            }
            other => panic!("expected captured events, got {other:?}"),
        }
    }

    #[test]
    fn a_captured_pointer_keeps_its_display_id() {
        // A `Position` is display-local, so the id decides which display's edges the step
        // is resolved against; it survives into the agent signal precisely because the
        // signal carries the wire event.
        let batch = ipc::agent_frame::Msg::Captured(ipc::CapturedBatch {
            events: vec![ipc::LocalEvent {
                monotonic_us: 0,
                ev: Some(ipc::local_event::Ev::Pointer(ipc::PointerAbs {
                    display_id: "edid:7a3f".to_owned(),
                    x: 1919.0,
                    y: 540.0,
                })),
            }],
            at_edge: false,
            activation: None,
        });
        let mut context = AgentContext::default();
        match agent_signal(&batch, &mut context) {
            AgentSignal::Captured(batch) => match batch.events[0].ev.as_ref() {
                Some(ipc::local_event::Ev::Pointer(pointer)) => {
                    assert_eq!(pointer.display_id, "edid:7a3f");
                    assert_eq!((pointer.x, pointer.y), (1919.0, 540.0));
                }
                other => panic!("expected a pointer event, got {other:?}"),
            },
            other => panic!("expected captured events, got {other:?}"),
        }
    }

    #[test]
    fn portal_edge_activation_survives_routing_without_input_events() {
        let batch = ipc::CapturedBatch {
            at_edge: true,
            activation: Some(ipc::PortalActivation {
                position: Some(ipc::PointerAbs {
                    display_id: String::new(),
                    x: -1280.0,
                    y: 512.0,
                }),
                edge: ipc::PortalEdge::Left as i32,
            }),
            events: Vec::new(),
        };
        let signal = agent_signal(
            &ipc::agent_frame::Msg::Captured(batch.clone()),
            &mut AgentContext::default(),
        );
        match signal {
            AgentSignal::Captured(captured) => assert_eq!(captured, batch),
            other => panic!("expected captured activation, got {other:?}"),
        }
    }

    #[test]
    fn the_hello_carries_the_capability_bits() {
        let mut capabilities = crate::no_capabilities();
        capabilities.capture = true;
        capabilities.inject = true;
        capabilities.absolute_pointer = true;
        capabilities.emergency_hotkey = true;
        let hello = hello(
            NodeId::new([7; 32]),
            "Workstation",
            true,
            capabilities,
            wire::SessionKind::Windows,
            Gossip {
                owner_serial: 1,
                ..Gossip::default()
            },
        );
        assert_eq!(hello.node_id.len(), 32);
        assert_eq!(hello.capabilities, vec![1, 2, 3, 10]);
        assert!(hello.can_own);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    // Explicit, so `ipc` cannot be ambiguous between the glob import and the crate name.
    use protocol::ipc;

    fn lease(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    #[test]
    fn every_output_variant_becomes_its_effect() {
        let peer = NodeId::new([1; 32]);
        let outputs = [
            fr::Output::Send {
                peer,
                message: fr::Message::LeaveAck { lease_id: lease(1) },
            },
            // The only output that fans out; it must survive as `Broadcast`, not become
            // a `Send` to a single peer.
            fr::Output::Broadcast(fr::Message::OwnerState {
                serial: 2,
                owner: Some(peer),
                granted: true,
            }),
            fr::Output::Agent(fr::AgentCommand::ReleaseSession { lease_id: lease(1) }),
            fr::Output::ArmTimer {
                timer: fr::Timer::ClaimGrant,
                deadline_ms: 600,
            },
            fr::Output::CancelTimer(fr::Timer::ClaimGrant),
            fr::Output::CloseLink {
                peer,
                code: fr::ErrorCode::CannotOwn,
            },
            fr::Output::Notice(fr::Notice::Focus(fr::FocusState::Local)),
        ];
        let effects = effects(&outputs);
        assert_eq!(effects.len(), outputs.len(), "no output may be swallowed");
        assert!(matches!(effects[0], Effect::Send { peer: p, .. } if p == peer));
        assert!(matches!(effects[1], Effect::Broadcast { .. }));
        assert!(matches!(effects[2], Effect::Agent(_)));
        assert!(matches!(
            effects[3],
            Effect::Arm {
                timer: fr::Timer::ClaimGrant,
                deadline_ms: 600
            }
        ));
        assert!(matches!(
            effects[4],
            Effect::Cancel {
                timer: fr::Timer::ClaimGrant
            }
        ));
        assert!(matches!(
            effects[5],
            Effect::CloseLink {
                code: fr::ErrorCode::CannotOwn,
                ..
            }
        ));
        assert!(matches!(effects[6], Effect::Notice(_)));
    }

    #[test]
    fn an_agent_error_carries_the_outstanding_prepare_lease_once() {
        let mut context = AgentContext::default();
        context.on_command(&fr::AgentCommand::PrepareInject { lease_id: lease(4) });

        let error = ipc::agent_frame::Msg::Error(ipc::AgentError {
            code: "CAPTURE_FAILED".to_owned(),
            detail: "no capture".to_owned(),
        });
        assert_eq!(
            agent_signal(&error, &mut context),
            AgentSignal::Fsm(fr::AgentEvent::Error {
                lease_id: Some(lease(4)),
                code: fr::ErrorCode::CaptureFailed,
            }),
            "the lease the failure belongs to is remembered"
        );
        assert_eq!(
            agent_signal(&error, &mut context),
            AgentSignal::Fsm(fr::AgentEvent::Error {
                lease_id: None,
                code: fr::ErrorCode::CaptureFailed,
            }),
            "and it is consumed, so a later failure cannot claim it"
        );

        // With nothing outstanding, a `Ready` frame is the attach-time `AgentReady`.
        assert_eq!(
            agent_signal(
                &ipc::agent_frame::Msg::Ready(ipc::AgentReady::default()),
                &mut context
            ),
            AgentSignal::Fsm(fr::AgentEvent::AgentReady)
        );
    }

    #[test]
    fn portal_startup_progress_distinguishes_permission_from_restore() {
        let mut context = AgentContext::default();
        let permission = ipc::agent_frame::Msg::PortalPermission(ipc::PortalPermissionRequired {
            portal_interface: "org.freedesktop.portal.RemoteDesktop".to_owned(),
            purpose: "inject".to_owned(),
        });
        assert_eq!(
            agent_signal(&permission, &mut context),
            AgentSignal::Portal {
                interface: "org.freedesktop.portal.RemoteDesktop".to_owned(),
                detail: "portal permission required for inject".to_owned(),
            }
        );

        let restoring =
            ipc::agent_frame::Msg::PortalPermissionRestoring(ipc::PortalPermissionRestoring {
                portal_interface: "org.freedesktop.portal.InputCapture".to_owned(),
                purpose: "capture".to_owned(),
            });
        assert_eq!(
            agent_signal(&restoring, &mut context),
            AgentSignal::PortalRestoring {
                interface: "org.freedesktop.portal.InputCapture".to_owned(),
                detail: "restoring portal permission for capture".to_owned(),
            }
        );
    }

    #[test]
    fn every_wire_message_has_its_own_log_name() {
        let peer = NodeId::new([2; 32]);
        let cases: [(fr::Message, &str); 14] = [
            (
                fr::Message::EnterPrepare {
                    lease_id: lease(1),
                    topology_revision: 1,
                    position: geometry::Position::new("", 0.0, 0.0),
                    owner_serial: 1,
                },
                "EnterPrepare",
            ),
            (
                fr::Message::EnterAck {
                    lease_id: lease(1),
                    accepted: true,
                    reason: None,
                },
                "EnterAck",
            ),
            (
                fr::Message::EnterCommit { lease_id: lease(1) },
                "EnterCommit",
            ),
            (
                fr::Message::Leave {
                    lease_id: lease(1),
                    reason: fr::LeaveReason::Emergency,
                },
                "Leave",
            ),
            (fr::Message::LeaveAck { lease_id: lease(1) }, "LeaveAck"),
            (
                fr::Message::InputBatch {
                    lease_id: lease(1),
                    input_batch_seq: 1,
                    events: Vec::new(),
                },
                "InputBatch",
            ),
            (
                fr::Message::InjectAck {
                    lease_id: lease(1),
                    input_batch_seq: 1,
                    applied_count: 0,
                    actual_position: None,
                    status: fr::InjectStatus::Ok,
                },
                "InjectAck",
            ),
            (
                fr::Message::LeaseChallenge {
                    lease_id: lease(1),
                    nonce: [0; 16],
                    ttl_ms: 500,
                },
                "LeaseChallenge",
            ),
            (
                fr::Message::LeaseRenew {
                    lease_id: lease(1),
                    nonce: [0; 16],
                },
                "LeaseRenew",
            ),
            (
                fr::Message::LeaseStatus {
                    lease_id: lease(1),
                    nonce: [0; 16],
                    accepted: true,
                    last_input_batch_seq: 1,
                },
                "LeaseStatus",
            ),
            (
                fr::Message::OwnerClaim {
                    serial: 1,
                    reason: fr::ClaimReason::Startup,
                },
                "OwnerClaim",
            ),
            (
                fr::Message::OwnerState {
                    serial: 1,
                    owner: Some(peer),
                    granted: true,
                },
                "OwnerState",
            ),
            (
                fr::Message::ProtocolError {
                    code: fr::ErrorCode::NotOwner,
                    related_seq: 1,
                },
                "ProtocolError",
            ),
            (
                fr::Message::Shutdown {
                    reason: fr::LeaveReason::Shutdown,
                },
                "Shutdown",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(message_name(&message), expected, "{message:?}");
        }
    }

    #[test]
    fn every_agent_frame_has_a_distinct_log_name() {
        use ipc::agent_frame::Msg;
        let frames: Vec<(Msg, &str)> = vec![
            (Msg::Attach(ipc::AgentAttach::default()), "AgentAttach"),
            (
                Msg::BeginCapture(ipc::BeginCapture::default()),
                "BeginCapture",
            ),
            (Msg::EndCapture(ipc::EndCapture::default()), "EndCapture"),
            (
                Msg::PrepareInject(ipc::PrepareInject::default()),
                "PrepareInject",
            ),
            (Msg::Inject(ipc::Inject::default()), "Inject"),
            (Msg::ArmDeadline(ipc::ArmDeadline::default()), "ArmDeadline"),
            (
                Msg::ReleaseSession(ipc::ReleaseSession::default()),
                "ReleaseSession",
            ),
            (Msg::ReleaseKeys(ipc::ReleaseKeys::default()), "ReleaseKeys"),
            (Msg::QueryDisplays(ipc::QueryDisplays {}), "QueryDisplays"),
            (Msg::Shutdown(ipc::AgentShutdown {}), "AgentShutdown"),
            (Msg::Ping(ipc::Ping::default()), "Ping"),
            (
                Msg::ForgetPortalGrants(ipc::ForgetPortalGrants {}),
                "ForgetPortalGrants",
            ),
            (Msg::Ready(ipc::AgentReady::default()), "AgentReady"),
            (
                Msg::CaptureReady(ipc::CaptureReady::default()),
                "CaptureReady",
            ),
            (
                Msg::Captured(ipc::CapturedBatch::default()),
                "CapturedBatch",
            ),
            (
                Msg::Emergency(ipc::EmergencyFired::default()),
                "EmergencyFired",
            ),
            (
                Msg::InjectResult(ipc::InjectResult::default()),
                "InjectResult",
            ),
            (
                Msg::DeadlineExpired(ipc::DeadlineExpired::default()),
                "DeadlineExpired",
            ),
            (Msg::Released(ipc::Released::default()), "Released"),
            (
                Msg::Displays(ipc::DisplaysReport::default()),
                "DisplaysReport",
            ),
            (
                Msg::LocalInput(ipc::LocalInputDetected::default()),
                "LocalInputDetected",
            ),
            (
                Msg::SessionEvent(ipc::SessionEvent::default()),
                "SessionEvent",
            ),
            (Msg::Pong(ipc::Pong::default()), "Pong"),
            (Msg::Error(ipc::AgentError::default()), "AgentError"),
            (
                Msg::PortalPermission(ipc::PortalPermissionRequired::default()),
                "PortalPermissionRequired",
            ),
            (
                Msg::PortalPermissionRestoring(ipc::PortalPermissionRestoring::default()),
                "PortalPermissionRestoring",
            ),
            (
                Msg::PortalGrant(ipc::PortalGrantChanged::default()),
                "PortalGrantChanged",
            ),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for (frame, expected) in frames {
            assert_eq!(agent_frame_name(&frame), expected, "{frame:?}");
            assert!(seen.insert(expected), "`{expected}` is used twice");
        }
    }

    /// Every capability the probe models must reach `Hello.capabilities`, otherwise a peer
    /// cannot learn what this node offers.
    #[test]
    fn capability_bits_cover_every_bit_the_probe_models() {
        let capabilities = platform_probe::Capabilities {
            capture: true,
            inject: true,
            absolute_pointer: true,
            physical_hid_keys: true,
            text_clipboard: true,
            native_repeat: true,
            software_repeat: true,
            receiver_takeover: true,
            displays_changed: true,
            emergency_hotkey: true,
        };

        let expected: Vec<i32> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(
            capability_bits(capabilities),
            expected,
            "capability_bits must map every bit `Capabilities::advertised()` names"
        );
    }

    #[test]
    fn error_codes_map_or_fall_back_to_invalid_frame() {
        assert_eq!(error_code("CAPTURE_FAILED"), fr::ErrorCode::CaptureFailed);
        assert_eq!(error_code("QUEUE_OVERFLOW"), fr::ErrorCode::CaptureFailed);
        assert_eq!(
            error_code("UNSUPPORTED_SESSION"),
            fr::ErrorCode::UnsupportedSession
        );
        assert_eq!(
            error_code("INJECTION_FAILED"),
            fr::ErrorCode::InjectionFailed
        );
        assert_eq!(error_code("AGENT_LOST"), fr::ErrorCode::AgentLost);
        assert_eq!(error_code("ANYTHING_ELSE"), fr::ErrorCode::InvalidFrame);
        assert_eq!(error_code(""), fr::ErrorCode::InvalidFrame);
    }

    #[test]
    fn local_events_round_trip_or_are_refused() {
        let button = |number: u32, down: bool| ipc::LocalEvent {
            monotonic_us: 0,
            ev: Some(ipc::local_event::Ev::Button(ipc::ButtonEv {
                button: number,
                down,
            })),
        };
        assert_eq!(
            input_event_of(&button(5, false)),
            Some(InputEvent::Button {
                button: MouseButton::Forward,
                state: KeyState::Up,
            })
        );
        assert_eq!(
            input_event_of(&button(9, true)),
            None,
            "an unknown button is refused, not relabelled"
        );

        let scroll = ipc::LocalEvent {
            monotonic_us: 0,
            ev: Some(ipc::local_event::Ev::Scroll(ipc::ScrollEv {
                h120: -120,
                v120: 240,
            })),
        };
        assert_eq!(
            input_event_of(&scroll),
            Some(InputEvent::Scroll {
                h120: -120,
                v120: 240
            })
        );

        // Out of range for the normalised model: refused rather than truncated.
        let huge = ipc::LocalEvent {
            monotonic_us: 0,
            ev: Some(ipc::local_event::Ev::Key(ipc::HidKey {
                usage_page: 0x1_0000,
                usage: 0x04,
                down: true,
            })),
        };
        assert_eq!(input_event_of(&huge), None);

        let empty = ipc::LocalEvent {
            monotonic_us: 0,
            ev: None,
        };
        assert_eq!(input_event_of(&empty), None);
    }

    #[test]
    fn relative_motion_is_the_only_event_without_a_wire_form() {
        assert_eq!(
            to_local_event(&InputEvent::PointerRelative { dx: 1, dy: 2 }),
            None,
            "relative motion is integrated before it is sent, never forwarded"
        );
        let absolute = to_local_event(&InputEvent::PointerAbsolute(input_model::Position::new(
            1.0, 2.0,
        )))
        .expect("an absolute position has a wire form");
        match absolute.ev {
            Some(ipc::local_event::Ev::Pointer(pointer)) => {
                assert_eq!(pointer.display_id, "", "the receiver resolves the display");
                assert_eq!((pointer.x, pointer.y), (1.0, 2.0));
            }
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }
}
