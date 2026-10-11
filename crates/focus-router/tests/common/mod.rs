#![allow(dead_code)] // Shared fixtures are intentionally used differently by each suite.
use focus_router::*;
use geometry::{NodeId, Position};
use input_model::{CaptureKind, InputEvent};

pub const LOCAL: NodeId = NodeId::new([1; 32]);
pub const PEER: NodeId = NodeId::new([2; 32]);
pub const OTHER: NodeId = NodeId::new([3; 32]);
pub const L: LeaseId = [9; 16];
pub const N: Nonce = [8; 16];
pub fn id(n: u64) -> LeaseId {
    let mut id = [0; 16];
    id[..8].copy_from_slice(&n.to_be_bytes());
    id
}
pub fn capabilities() -> Capabilities {
    Capabilities {
        capture: true,
        inject: true,
        absolute_pointer: true,
        emergency_hotkey: true,
    }
}
pub fn position() -> Position {
    Position::new("display", 2.0, 3.0)
}
pub fn hello(peer: NodeId, owner: NodeId, serial: u64) -> Input {
    Input::Hello {
        peer,
        hello: PeerHello {
            can_own: true,
            capabilities: capabilities(),
            owner_serial: serial,
            owner: Some(owner),
        },
    }
}
pub fn router(owner: NodeId) -> FocusRouter {
    let mut r = FocusRouter::new(
        LOCAL,
        Timing::default(),
        SequentialIds::default(),
        capabilities(),
    )
    .with_persisted_ownership(1, Some((1, owner)));
    r.handle(0, hello(PEER, owner, 1));
    r.handle(0, hello(OTHER, owner, 1));
    r
}
pub fn net(message: Message) -> Input {
    Input::Net {
        from: PEER,
        message,
    }
}
pub fn send(message: Message) -> Output {
    Output::Send {
        peer: PEER,
        message,
    }
}
pub fn switch(trigger: SwitchTrigger) -> Input {
    Input::Local(LocalInput::SwitchRequest {
        trigger,
        target: PEER,
        entry_position: position(),
        topology_revision: 0,
    })
}
pub fn prepare(lease_id: LeaseId, owner_serial: u64) -> Message {
    Message::EnterPrepare {
        lease_id,
        owner_serial,
        topology_revision: 0,
        position: position(),
    }
}
pub fn ack(lease_id: LeaseId, reason: Option<ErrorCode>) -> Message {
    Message::EnterAck {
        lease_id,
        accepted: reason.is_none(),
        reason,
    }
}
pub fn end(lease_id: LeaseId, replay: bool) -> Output {
    Output::Agent(AgentCommand::EndCapture {
        lease_id,
        replay_buffered_locally: replay,
    })
}
pub fn release(lease_id: LeaseId) -> Output {
    Output::Agent(AgentCommand::ReleaseSession { lease_id })
}
pub fn leave(lease_id: LeaseId, reason: LeaveReason) -> Output {
    send(Message::Leave { lease_id, reason })
}
pub fn arm(timer: Timer, deadline_ms: u64) -> Output {
    Output::ArmTimer { timer, deadline_ms }
}
pub fn begin(kind: CaptureKind, lease_id: LeaseId, self_timeout_ms: u64) -> Output {
    Output::Agent(AgentCommand::BeginCapture {
        kind,
        lease_id,
        self_timeout_ms,
    })
}
pub fn notice(n: Notice) -> Output {
    Output::Notice(n)
}
// Compare all externally meaningful actions and diagnostic notices in order.
// State-change notices are checked through focus()/ownership(); timers separately.
pub fn effects(outputs: &[Output]) -> Vec<Output> {
    outputs
        .iter()
        .filter(|o| {
            !matches!(
                o,
                Output::ArmTimer { .. }
                    | Output::CancelTimer(_)
                    | Output::Notice(Notice::Focus(_) | Notice::Ownership(_))
            )
        })
        .cloned()
        .collect()
}
pub fn preparing() -> FocusRouter {
    let mut r = router(LOCAL);
    r.handle(10, switch(SwitchTrigger::Edge));
    assert_eq!(r.focus(), FocusState::Preparing);
    r
}
pub fn remote() -> FocusRouter {
    let mut r = preparing();
    r.handle(
        11,
        Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
    );
    r.handle(12, net(ack(id(1), None)));
    r.handle(
        13,
        net(Message::LeaseChallenge {
            lease_id: id(1),
            nonce: N,
            ttl_ms: 500,
        }),
    );
    r.handle(
        14,
        Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
    );
    assert_eq!(r.focus(), FocusState::Remote);
    r
}
pub fn prepared() -> FocusRouter {
    let mut r = router(PEER);
    r.handle(10, net(prepare(L, 1)));
    r.handle(11, Input::Agent(AgentEvent::Ready { lease_id: L }));
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Prepared);
    r
}
pub fn active() -> FocusRouter {
    let mut r = prepared();
    r.handle(12, net(Message::EnterCommit { lease_id: L }));
    assert_eq!(r.focus(), FocusState::Remote);
    r
}
pub fn batch(seq: u64, count: usize) -> Message {
    Message::InputBatch {
        lease_id: L,
        input_batch_seq: seq,
        events: vec![InputEvent::PointerAbsolute(input_model::Position::new(3.0, 4.0)); count],
    }
}
pub fn status(nonce: Nonce, accepted: bool, last_input_batch_seq: u64) -> Output {
    send(Message::LeaseStatus {
        lease_id: L,
        nonce,
        accepted,
        last_input_batch_seq,
    })
}
pub fn grant(serial: u64, owner: NodeId) -> Output {
    Output::Broadcast(Message::OwnerState {
        serial,
        owner: Some(owner),
        granted: true,
    })
}
pub fn claim() -> Input {
    Input::Local(LocalInput::Claim {
        reason: ClaimReason::User,
    })
}
pub fn higher_claim() -> Input {
    net(Message::OwnerClaim {
        serial: 2,
        reason: ClaimReason::User,
    })
}
pub fn no_grant(outputs: &[Output]) {
    assert!(
        !outputs.iter().any(|o| matches!(
            o,
            Output::Broadcast(Message::OwnerState { granted: true, .. })
                | Output::Send {
                    message: Message::OwnerState { granted: true, .. },
                    ..
                }
        )),
        "premature grant: {outputs:?}"
    );
}
