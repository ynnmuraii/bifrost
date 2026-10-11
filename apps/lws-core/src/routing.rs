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
            lease_id: lease_id.to_vec(),
            // `ipc.proto`: the agent ends capture by itself, max 1000 ms.
            self_timeout_ms: u32::try_from(*self_timeout_ms)
                .unwrap_or(u32::MAX)
                .min(1000),
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
pub fn attach(
    launch_token: Vec<u8>,
    mode: ipc::AgentMode,
    emergency_hotkey: &str,
    backend_override: ipc::BackendKind,
    core_silence_ms: u32,
    require_emergency_hotkey: bool,
) -> ipc::AgentAttach {
    ipc::AgentAttach {
        launch_token,
        mode: mode as i32,
        emergency_hotkey: emergency_hotkey.to_owned(),
        backend_override: backend_override as i32,
        // Portal restore tokens are Wayland-only and live in
        // `portal-restore.toml`, which the E2 slice does not read or write.
        restore_tokens: Vec::new(),
        require_emergency_hotkey,
        core_silence_ms,
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
    Captured(Vec<InputEvent>),
    /// The agent's display set changed.
    Displays(ipc::DisplaysReport),
    /// A portal hint for the GUI (`PortalPermissionRequired`/`PortalGrantChanged`).
    Portal { interface: String, detail: String },
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
    offered: Option<(u64, usize)>,
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
            } => self.offered = Some((*input_batch_seq, events.len())),
            _ => {}
        }
    }

    /// How many events the `Inject` batch with this sequence offered, if it is known.
    fn offered_for(&self, input_batch_seq: u64) -> Option<usize> {
        self.offered
            .filter(|(sequence, _)| *sequence == input_batch_seq)
            .map(|(_, count)| count)
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
        Msg::Captured(batch) => AgentSignal::Captured(
            batch
                .events
                .iter()
                .filter_map(input_event_of)
                .collect::<Vec<_>>(),
        ),
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
                let offered = context.offered_for(result.input_batch_seq);
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
        Msg::PortalGrant(grant) => AgentSignal::Portal {
            interface: grant
                .grant
                .as_ref()
                .map(|grant| grant.portal_interface.clone())
                .unwrap_or_default(),
            detail: if grant.revoked {
                "portal grant revoked".to_owned()
            } else {
                "portal grant changed".to_owned()
            },
        },
        Msg::Error(error) => AgentSignal::Fsm(fr::AgentEvent::Error {
            lease_id: context.prepared.take(),
            code: error_code(&error.code),
        }),
        Msg::Pong(_) => AgentSignal::Ignored,
        // Core → agent frames arriving from the agent are a protocol error; the
        // loop's watchdog still counted the frame as liveness.
        _ => AgentSignal::Fsm(fr::AgentEvent::Error {
            lease_id: None,
            code: fr::ErrorCode::InvalidFrame,
        }),
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
        Msg::PortalGrant(_) => "PortalGrantChanged",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(byte: u8) -> [u8; 16] {
        [byte; 16]
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
        });
        match agent_signal(&batch, &mut context) {
            AgentSignal::Captured(events) => assert_eq!(
                events,
                vec![InputEvent::Key {
                    key: input_model::HidKey::keyboard(0x04),
                    state: KeyState::Down,
                }]
            ),
            other => panic!("expected captured events, got {other:?}"),
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
