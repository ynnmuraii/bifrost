//! The pairing ceremony: the state diagram of `docs/05-protocol.md` driven without a
//! socket, plus the window's admission limits.

use geometry::NodeId;
use identity::{
    fingerprint_grouped, PairingFsm, PairingInput, PairingOutcome, PairingOutput, PairingRole,
    PairingState, PairingWindow, RejectionReason, SasCode, MAX_ATTEMPTS, MAX_PENDING_PEERS,
    WINDOW_MS,
};

const ATTEMPT: [u8; 16] = [7u8; 16];

fn node(byte: u8) -> NodeId {
    NodeId::new([byte; 32])
}

fn local() -> NodeId {
    node(1)
}

fn remote() -> NodeId {
    node(2)
}

fn sas() -> SasCode {
    SasCode::from_exporter_bytes([0, 0, 0x30, 0x39]) // 12345
}

fn opener() -> PairingFsm {
    PairingFsm::window_opener(local(), "node-1", ATTEMPT)
}

fn dialer() -> PairingFsm {
    PairingFsm::dialer(local(), "node-1")
}

/// Drives the window opener up to `Comparing` and returns it.
fn opener_at_comparing() -> PairingFsm {
    let mut fsm = opener();
    fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    fsm.handle(PairingInput::PeerAdmitted);
    fsm.handle(PairingInput::HandshakeEstablished {
        peer_node_id: remote(),
    });
    fsm.handle(PairingInput::AttemptMaterial { sas: sas() });
    fsm.handle(PairingInput::RemoteHello {
        attempt_id: ATTEMPT,
        node_id: remote(),
        display_name: "node-2".to_owned(),
        supports_sas: true,
    });
    assert_eq!(fsm.state(), PairingState::Comparing);
    fsm
}

#[test]
fn the_window_opener_listens_and_sends_hello_first() {
    let mut fsm = opener();
    assert_eq!(fsm.state(), PairingState::Off);
    assert_eq!(fsm.role(), PairingRole::WindowOpener);
    assert_eq!(fsm.outcome(), PairingOutcome::InProgress);

    assert!(fsm
        .handle(PairingInput::WindowOpened { now_ms: 0 })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Listening);

    assert!(fsm.handle(PairingInput::PeerAdmitted).is_empty());
    assert_eq!(fsm.state(), PairingState::Handshaking);

    // «узел, открывший окно сопряжения, — TLS-сервер и отправляет PairHello первым».
    let outputs = fsm.handle(PairingInput::HandshakeEstablished {
        peer_node_id: remote(),
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::SendHello {
            node_id: local(),
            display_name: "node-1".to_owned(),
            supports_sas: true,
        }]
    );
    assert!(fsm.hello_sent());
    assert_eq!(fsm.peer_node_id(), Some(&remote()));
}

#[test]
fn the_dialer_computes_the_code_only_after_the_hello_brought_the_attempt_id() {
    let mut fsm = dialer();
    fsm.handle(PairingInput::DialStarted { now_ms: 0 });
    assert_eq!(fsm.state(), PairingState::Dialing);
    assert_eq!(
        *fsm.attempt_id(),
        [0u8; 16],
        "the dialer cannot know it yet"
    );

    assert!(fsm
        .handle(PairingInput::HandshakeEstablished {
            peer_node_id: remote()
        })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Handshaking);

    // A code without the opener's hello cannot show anything yet.
    assert!(fsm
        .handle(PairingInput::AttemptMaterial { sas: sas() })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Handshaking);

    let outputs = fsm.handle(PairingInput::RemoteHello {
        attempt_id: ATTEMPT,
        node_id: remote(),
        display_name: "node-2".to_owned(),
        supports_sas: true,
    });
    assert_eq!(
        *fsm.attempt_id(),
        ATTEMPT,
        "the dialer adopts the opened window id"
    );
    assert_eq!(fsm.state(), PairingState::Comparing);
    assert_eq!(
        outputs,
        vec![
            PairingOutput::SendHello {
                node_id: local(),
                display_name: "node-1".to_owned(),
                supports_sas: true,
            },
            PairingOutput::ShowSas {
                sas: sas(),
                peer_node_id: remote(),
            },
        ]
    );
    assert_eq!(fsm.sas(), Some(sas()));
    assert_eq!(fsm.peer_display_name(), Some("node-2"));
    assert_eq!(fsm.peer_fingerprint(), Some(fingerprint_grouped(&remote())));
}

#[test]
fn a_hello_that_disagrees_with_the_handshake_or_the_window_is_refused() {
    // The peer claims an identity that is not the one it proved possession of.
    let mut fsm = opener();
    fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    fsm.handle(PairingInput::PeerAdmitted);
    fsm.handle(PairingInput::HandshakeEstablished {
        peer_node_id: remote(),
    });
    let outputs = fsm.handle(PairingInput::RemoteHello {
        attempt_id: ATTEMPT,
        node_id: node(3),
        display_name: "liar".to_owned(),
        supports_sas: true,
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Rejected {
            reason: RejectionReason::NodeIdMismatch
        }]
    );
    assert_eq!(
        fsm.outcome(),
        PairingOutcome::Rejected(RejectionReason::NodeIdMismatch)
    );
    assert_eq!(fsm.outcome().as_str(), "MISMATCH");

    // The hello must carry the attempt id of this window.
    let mut fsm = opener();
    fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    fsm.handle(PairingInput::PeerAdmitted);
    fsm.handle(PairingInput::HandshakeEstablished {
        peer_node_id: remote(),
    });
    fsm.handle(PairingInput::RemoteHello {
        attempt_id: [9u8; 16],
        node_id: remote(),
        display_name: "node-2".to_owned(),
        supports_sas: true,
    });
    assert_eq!(fsm.rejection(), Some(RejectionReason::AttemptMismatch));
    assert_eq!(fsm.outcome().as_str(), "MISMATCH");

    // A hello cannot arrive before the handshake-derived identity exists: the session is
    // still `Off`, so the input is ignored rather than acted upon.
    let mut fsm = opener();
    assert!(fsm
        .handle(PairingInput::RemoteHello {
            attempt_id: ATTEMPT,
            node_id: remote(),
            display_name: "node-2".to_owned(),
            supports_sas: true,
        })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Off);
    assert_eq!(fsm.rejection(), None);
}

#[test]
fn a_peer_without_the_comparison_code_is_not_pairable() {
    let mut fsm = opener();
    fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    fsm.handle(PairingInput::PeerAdmitted);
    fsm.handle(PairingInput::HandshakeEstablished {
        peer_node_id: remote(),
    });
    let outputs = fsm.handle(PairingInput::RemoteHello {
        attempt_id: ATTEMPT,
        node_id: remote(),
        display_name: "node-2".to_owned(),
        supports_sas: false,
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Rejected {
            reason: RejectionReason::SasUnsupported
        }]
    );
    assert_eq!(fsm.outcome().as_str(), "REJECTED");
}

#[test]
fn both_approvals_are_needed_and_the_commit_happens_once() {
    let mut fsm = opener_at_comparing();

    // Local approval alone is not a commit.
    let outputs = fsm.handle(PairingInput::LocalDecision { approved: true });
    assert_eq!(
        outputs,
        vec![PairingOutput::SendDecision {
            approved: true,
            peer_node_id: remote(),
        }]
    );
    assert_eq!(fsm.state(), PairingState::LocalApproved);
    assert_eq!(fsm.outcome(), PairingOutcome::InProgress);

    // The peer's decision arrives; `peer_node_id` is the addressee, i.e. us.
    let outputs = fsm.handle(PairingInput::RemoteDecision {
        approved: true,
        peer_node_id: local(),
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Commit {
            peer_node_id: remote(),
            display_name: "node-2".to_owned(),
            address: None,
        }]
    );
    assert_eq!(fsm.state(), PairingState::Committed);
    assert_eq!(fsm.outcome(), PairingOutcome::Committed);
    assert_eq!(fsm.outcome().as_str(), "COMMITTED");

    // Terminal: nothing else can happen, and the record is written exactly once.
    for input in [
        PairingInput::LocalDecision { approved: true },
        PairingInput::RemoteDecision {
            approved: true,
            peer_node_id: local(),
        },
        PairingInput::Tick {
            now_ms: WINDOW_MS * 10,
        },
        PairingInput::Abort,
    ] {
        assert!(fsm.handle(input).is_empty(), "a committed ceremony is over");
    }
}

#[test]
fn the_peer_may_answer_before_the_local_user_does() {
    let mut fsm = opener_at_comparing();

    // The peer approved first: with no local answer there is nothing to emit yet.
    assert!(fsm
        .handle(PairingInput::RemoteDecision {
            approved: true,
            peer_node_id: local(),
        })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Comparing);

    // Our approval then completes both halves at once.
    let outputs = fsm.handle(PairingInput::LocalDecision { approved: true });
    assert_eq!(
        outputs,
        vec![
            PairingOutput::SendDecision {
                approved: true,
                peer_node_id: remote(),
            },
            PairingOutput::Commit {
                peer_node_id: remote(),
                display_name: "node-2".to_owned(),
                address: None,
            },
        ]
    );
    assert_eq!(fsm.outcome(), PairingOutcome::Committed);
}

#[test]
fn a_decision_addressed_to_someone_else_is_a_mismatch() {
    // `PairDecision.peer_node_id` is the addressee; a decision for another node is not
    // ours to accept.
    let mut fsm = opener_at_comparing();
    let outputs = fsm.handle(PairingInput::RemoteDecision {
        approved: true,
        peer_node_id: node(9),
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Rejected {
            reason: RejectionReason::NodeIdMismatch
        }]
    );
    assert_eq!(fsm.outcome().as_str(), "MISMATCH");
}

#[test]
fn a_local_rejection_tells_the_peer_and_writes_nothing() {
    let mut fsm = opener_at_comparing();
    let outputs = fsm.handle(PairingInput::LocalDecision { approved: false });

    assert_eq!(
        outputs,
        vec![
            PairingOutput::Rejected {
                reason: RejectionReason::UserRejected
            },
            PairingOutput::SendDecision {
                approved: false,
                peer_node_id: remote(),
            },
        ]
    );
    assert!(outputs
        .iter()
        .all(|output| !matches!(output, PairingOutput::Commit { .. })));
    assert_eq!(
        fsm.outcome(),
        PairingOutcome::Rejected(RejectionReason::UserRejected)
    );
    assert_eq!(fsm.outcome().as_str(), "REJECTED");
}

#[test]
fn a_peer_rejection_rejects_the_ceremony() {
    let mut fsm = opener_at_comparing();
    let outputs = fsm.handle(PairingInput::RemoteDecision {
        approved: false,
        peer_node_id: local(),
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Rejected {
            reason: RejectionReason::PeerRejected
        }]
    );
    assert_eq!(fsm.outcome().as_str(), "REJECTED");
}

#[test]
fn audit_outcome_names_match_the_documented_values() {
    // `PAIRING_AUDIT.outcome` is `COMMITTED | REJECTED | EXPIRED | MISMATCH`.
    assert_eq!(RejectionReason::NodeIdMismatch.as_str(), "MISMATCH");
    assert_eq!(RejectionReason::AttemptMismatch.as_str(), "MISMATCH");
    assert_eq!(RejectionReason::SasUnsupported.as_str(), "REJECTED");
    assert_eq!(RejectionReason::PeerRejected.as_str(), "REJECTED");
    assert_eq!(RejectionReason::UserRejected.as_str(), "REJECTED");
    assert_eq!(RejectionReason::TooManyAttempts.as_str(), "REJECTED");
    assert_eq!(RejectionReason::Aborted.as_str(), "REJECTED");
    assert_eq!(RejectionReason::Expired.as_str(), "EXPIRED");
    assert_eq!(PairingOutcome::Committed.as_str(), "COMMITTED");
    assert_eq!(PairingOutcome::Expired.as_str(), "EXPIRED");
    assert_eq!(PairingOutcome::InProgress.as_str(), "IN_PROGRESS");
    assert_eq!(PairingRole::WindowOpener.as_str(), "window-opener");
    assert_eq!(PairingRole::Dialer.as_str(), "dialer");
}

#[test]
fn an_abort_rejects_whatever_the_ceremony_was_doing() {
    for mut fsm in [opener(), opener_at_comparing()] {
        let outputs = fsm.handle(PairingInput::Abort);
        assert!(
            matches!(outputs.as_slice(), [PairingOutput::Rejected { reason }] if *reason == RejectionReason::Aborted)
        );
        assert_eq!(fsm.state(), PairingState::Rejected);
        assert_eq!(
            fsm.outcome(),
            PairingOutcome::Rejected(RejectionReason::Aborted)
        );
        // Nothing was committed by an abort.
        assert!(!outputs
            .iter()
            .any(|o| matches!(o, PairingOutput::Commit { .. })));
    }
}

#[test]
fn the_window_is_the_deadline_of_every_live_state() {
    let mut fsm = opener_at_comparing();

    // Just inside the window: nothing happens.
    assert!(fsm
        .handle(PairingInput::Tick {
            now_ms: WINDOW_MS - 1
        })
        .is_empty());
    assert_eq!(fsm.state(), PairingState::Comparing);

    // At the deadline the ceremony expires, from `Comparing`…
    let outputs = fsm.handle(PairingInput::Tick { now_ms: WINDOW_MS });
    assert_eq!(outputs, vec![PairingOutput::Expired]);
    assert_eq!(fsm.state(), PairingState::Expired);
    assert_eq!(fsm.outcome(), PairingOutcome::Expired);
    assert_eq!(fsm.outcome().as_str(), "EXPIRED");

    // …and from every other live state too.
    for mut live in [opener(), opener()] {
        live.handle(PairingInput::WindowOpened { now_ms: 5_000 });
        live.handle(PairingInput::PeerAdmitted);
        live.handle(PairingInput::HandshakeEstablished {
            peer_node_id: remote(),
        });
        let deadline = 5_000 + WINDOW_MS;
        assert!(live
            .handle(PairingInput::Tick {
                now_ms: deadline - 1
            })
            .is_empty());
        assert_eq!(
            live.handle(PairingInput::Tick { now_ms: deadline }),
            vec![PairingOutput::Expired]
        );
    }

    // An expired ceremony accepts nothing more, not even a new window.
    assert!(fsm
        .handle(PairingInput::WindowOpened { now_ms: 0 })
        .is_empty());
    assert!(fsm.handle(PairingInput::PeerAdmitted).is_empty());
    assert_eq!(fsm.state(), PairingState::Expired);
}

#[test]
fn inputs_from_the_other_role_or_the_wrong_state_are_ignored() {
    let mut dialer_fsm = dialer();
    assert!(dialer_fsm
        .handle(PairingInput::WindowOpened { now_ms: 0 })
        .is_empty());
    assert_eq!(
        dialer_fsm.state(),
        PairingState::Off,
        "only the opener opens a window"
    );

    let mut opener_fsm = opener();
    assert!(opener_fsm
        .handle(PairingInput::DialStarted { now_ms: 0 })
        .is_empty());
    assert_eq!(
        opener_fsm.state(),
        PairingState::Off,
        "only the dialer dials"
    );

    // A connection cannot be admitted before the window exists.
    assert!(opener_fsm.handle(PairingInput::PeerAdmitted).is_empty());
    assert_eq!(opener_fsm.state(), PairingState::Off);

    // A second admission does not restart the handshake.
    opener_fsm.handle(PairingInput::WindowOpened { now_ms: 0 });
    opener_fsm.handle(PairingInput::PeerAdmitted);
    opener_fsm.handle(PairingInput::PeerAdmitted);
    assert_eq!(opener_fsm.state(), PairingState::Handshaking);

    // The user cannot answer before both hellos were compared.
    let mut early = opener();
    early.handle(PairingInput::WindowOpened { now_ms: 0 });
    early.handle(PairingInput::PeerAdmitted);
    assert!(early
        .handle(PairingInput::LocalDecision { approved: true })
        .is_empty());
    assert_eq!(early.state(), PairingState::Handshaking);

    // A handshake for a session that never started is ignored.
    let mut off = opener();
    assert!(off
        .handle(PairingInput::HandshakeEstablished {
            peer_node_id: remote()
        })
        .is_empty());
    assert_eq!(off.state(), PairingState::Off);
}

#[test]
fn the_commit_carries_the_address_the_ceremony_used() {
    let mut fsm = opener_at_comparing();
    fsm.set_address("192.168.1.20:47901");
    fsm.handle(PairingInput::LocalDecision { approved: true });
    let outputs = fsm.handle(PairingInput::RemoteDecision {
        approved: true,
        peer_node_id: local(),
    });
    assert_eq!(
        outputs,
        vec![PairingOutput::Commit {
            peer_node_id: remote(),
            display_name: "node-2".to_owned(),
            address: Some("192.168.1.20:47901".to_owned()),
        }]
    );
}

#[test]
fn the_window_admits_three_pending_peers_and_then_refuses() {
    let mut window = PairingWindow::open(1_000, ATTEMPT);
    assert_eq!(window.attempt_id(), &ATTEMPT);
    assert_eq!(window.opened_ms(), 1_000);
    assert_eq!(window.deadline_ms(), 1_000 + WINDOW_MS);
    assert!(window.is_open(1_000));
    assert_eq!(window.pending(), 0);
    assert_eq!(window.attempts(), 0);

    for _ in 0..MAX_PENDING_PEERS {
        window.admit(1_001).expect("within the pending limit");
    }
    assert_eq!(window.pending(), MAX_PENDING_PEERS);
    assert_eq!(
        window.admit(1_002),
        Err(RejectionReason::TooManyAttempts),
        "«не более 3 ожидающих peers»"
    );
}

#[test]
fn the_window_allows_three_attempts_then_closes() {
    let mut window = PairingWindow::open(0, ATTEMPT);
    for _ in 0..MAX_ATTEMPTS {
        window.admit(0).expect("within the attempt limit");
        window.finish();
    }
    assert_eq!(window.attempts(), MAX_ATTEMPTS);
    assert!(!window.is_open(0), "the attempts are used up");
    assert_eq!(window.admit(0), Err(RejectionReason::Expired));
}

#[test]
fn the_window_never_admits_more_than_three_handshakes() {
    // `pending + attempts` is the handshake budget: a concluded attempt and a live pending
    // peer draw on the same three («rate limit handshakes»).
    let mut window = PairingWindow::open(0, ATTEMPT);
    window.admit(0).expect("the first handshake");
    window.finish();
    window.admit(0).expect("the second handshake");
    window.finish();
    window.admit(0).expect("the third handshake");
    assert_eq!(
        window.admit(0),
        Err(RejectionReason::TooManyAttempts),
        "three handshakes — concluded or pending — exhaust the window"
    );
}

#[test]
fn releasing_a_pending_handshake_does_not_spend_an_attempt() {
    let mut window = PairingWindow::open(0, ATTEMPT);
    window.admit(0).expect("the first handshake");
    window.release_pending();
    assert_eq!(window.pending(), 0);
    assert_eq!(
        window.attempts(),
        0,
        "an abandoned handshake is not an attempt"
    );
    assert!(window.is_open(0));
}

#[test]
fn the_window_closes_at_its_deadline() {
    let mut window = PairingWindow::open(1_000, ATTEMPT);
    assert!(window.is_open(1_000 + WINDOW_MS - 1));
    assert!(!window.is_open(1_000 + WINDOW_MS));
    assert_eq!(
        window.admit(1_000 + WINDOW_MS),
        Err(RejectionReason::Expired)
    );
}

#[test]
fn finishing_without_admitting_does_not_underflow() {
    let mut window = PairingWindow::open(0, ATTEMPT);
    window.finish();
    assert_eq!(window.pending(), 0, "pending saturates at zero");
    assert_eq!(window.attempts(), 1);
}
