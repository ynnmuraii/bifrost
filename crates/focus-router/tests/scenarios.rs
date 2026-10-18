mod common;
use common::*;
use focus_router::*;
use input_model::{CaptureKind, HidKey};

#[test]
fn nonce_window_covers_every_legal_ttl_and_both_cadence_endpoints() {
    for ttl_ms in Timing::LEASE_TTL_MIN_MS..=Timing::LEASE_TTL_MAX_MS {
        let timing = Timing {
            lease_ttl_ms: ttl_ms,
            ..Timing::default()
        };
        let mut receiver = ReceiverLease::new(L, PEER, 0, 0);
        receiver.state = ReceiverState::Active;
        let mut source = SourceLease::new(L, PEER, 0, position(), 0);
        let mut ids = SequentialIds::default();
        let mut first = None;
        for issued_ms in (0..=ttl_ms).step_by(timing.challenge_interval_ms() as usize) {
            let (nonce, _) = receiver.issue_challenge(&mut ids, issued_ms);
            first = first.or(Some(nonce));
            assert!(source.remember_challenge(nonce));
            assert!(receiver.challenges.len() <= MAX_CHALLENGES);
            assert!(source.unanswered_challenges.len() <= MAX_CHALLENGES);
        }
        let nonce = first.unwrap();
        assert_eq!(receiver.apply_renew(&nonce, ttl_ms), Some(ttl_ms));
        assert!(source.take_challenge(&nonce), "TTL {ttl_ms}");
        assert!(!source.take_challenge(&nonce));
        assert_eq!(receiver.apply_renew(&nonce, ttl_ms), None);
    }
}

#[test]
fn delayed_850_to_950ms_renew_survives_challenges_but_not_original_issue_ttl() {
    for delay_ms in 850..=950 {
        let timing = Timing::default();
        let mut r = FocusRouter::new(LOCAL, timing, SequentialIds::default(), capabilities())
            .with_persisted_ownership(1, Some((1, PEER)));
        r.handle(0, hello(PEER, PEER, 1));
        r.handle(10, net(prepare(L, 1)));
        r.handle(11, Input::Agent(AgentEvent::Ready { lease_id: L }));
        let (nonce, issue_ms) = r.lease().receiver().unwrap().challenges[0];
        r.handle(12, net(Message::EnterCommit { lease_id: L }));
        let arrived_ms = issue_ms + delay_ms;
        for issued_ms in (112..=arrived_ms).step_by(100) {
            r.handle(issued_ms, Input::TimerExpired(Timer::ReceiverChallenge));
        }
        let out = r.handle(arrived_ms, net(Message::LeaseRenew { lease_id: L, nonce }));
        let deadline_ms = issue_ms + timing.lease_ttl_ms;
        assert!(out.contains(&status(nonce, true, 0)), "delay {delay_ms}");
        assert!(out.contains(&arm(Timer::ReceiverLeaseDeadline, deadline_ms)));
        assert_eq!(r.lease().receiver().unwrap().deadline_ms, deadline_ms);

        // No further source replies: challenges continue, but cannot buy grace.
        let next_issue_ms = 112 + ((arrived_ms - 112) / 100 + 1) * 100;
        for issued_ms in (next_issue_ms..deadline_ms).step_by(100) {
            r.handle(issued_ms, Input::TimerExpired(Timer::ReceiverChallenge));
            assert_eq!(r.lease().receiver().unwrap().deadline_ms, deadline_ms);
        }
        let out = r.handle(
            deadline_ms,
            Input::TimerExpired(Timer::ReceiverLeaseDeadline),
        );
        assert!(out.contains(&release(L)));
        assert_eq!(r.focus(), FocusState::Local);
        let out = r.handle(
            deadline_ms + 1,
            net(Message::LeaseRenew { lease_id: L, nonce }),
        );
        assert!(out.contains(&status(nonce, false, 0)));
        assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Expired);
        assert_eq!(r.lease().receiver().unwrap().deadline_ms, deadline_ms);
    }
}

#[test]
fn delayed_850_to_950ms_status_survives_challenges_but_source_still_dies() {
    for delay_ms in 850..=950 {
        let mut r = remote();
        // Advertise the legal default receiver TTL and acknowledge the fixture nonce.
        r.handle(
            15,
            net(Message::LeaseChallenge {
                lease_id: id(1),
                nonce: id(100),
                ttl_ms: 1000,
            }),
        );
        r.handle(
            16,
            net(Message::LeaseStatus {
                lease_id: id(1),
                nonce: N,
                accepted: true,
                last_input_batch_seq: 0,
            }),
        );
        let arrived_ms = 15 + delay_ms;
        for issued_ms in (115..=arrived_ms).step_by(100) {
            let out = r.handle(
                issued_ms,
                net(Message::LeaseChallenge {
                    lease_id: id(1),
                    nonce: id(issued_ms),
                    ttl_ms: 1000,
                }),
            );
            assert!(out.contains(&send(Message::LeaseRenew {
                lease_id: id(1),
                nonce: id(issued_ms),
            })));
        }
        let delayed_status = Message::LeaseStatus {
            lease_id: id(1),
            nonce: id(100),
            accepted: true,
            last_input_batch_seq: 0,
        };
        let out = r.handle(arrived_ms, net(delayed_status.clone()));
        let deadline_ms = arrived_ms + 1000;
        assert!(
            out.contains(&arm(Timer::SourceLeaseWatchdog, deadline_ms)),
            "delay {delay_ms}"
        );
        // A replay cannot postpone the watchdog. No further accepted status arrives.
        let out = r.handle(arrived_ms + 1, net(delayed_status.clone()));
        assert!(!out.iter().any(|output| matches!(
            output,
            Output::ArmTimer {
                timer: Timer::SourceLeaseWatchdog,
                ..
            }
        )));
        let out = r.handle(deadline_ms, Input::TimerExpired(Timer::SourceLeaseWatchdog));
        assert!(out.contains(&end(id(1), false)));
        assert!(out.contains(&Output::CloseLink {
            peer: PEER,
            code: ErrorCode::LeaseExpired
        }));
        assert_eq!(r.focus(), FocusState::Local);
        r.handle(deadline_ms + 1, net(delayed_status));
        assert!(r.lease().is_idle());
    }
}

#[test]
fn default_lease_survives_a_500ms_renew_stall_without_extending_the_death_budget() {
    let timing = Timing::default();
    let mut r = FocusRouter::new(LOCAL, timing, SequentialIds::default(), capabilities())
        .with_persisted_ownership(1, Some((1, PEER)));
    r.handle(0, hello(PEER, PEER, 1));
    r.handle(10, net(prepare(L, 1)));
    r.handle(11, Input::Agent(AgentEvent::Ready { lease_id: L }));
    r.handle(12, net(Message::EnterCommit { lease_id: L }));
    let mut delayed_nonce = None;
    let mut latest_nonce = None;

    // A live source answers, but ordered TCP holds the answers for 500 ms.
    // Service the real FSM deadline before delivering each network event, as core does.
    for issued in (112..=612).step_by(100) {
        if r.lease().receiver().unwrap().deadline_ms <= issued {
            r.handle(issued, Input::TimerExpired(Timer::ReceiverLeaseDeadline));
        }
        let out = r.handle(issued, Input::TimerExpired(Timer::ReceiverChallenge));
        let nonce = out.iter().find_map(|output| match output {
            Output::Send {
                message: Message::LeaseChallenge { nonce, .. },
                ..
            } => Some(*nonce),
            _ => None,
        });
        assert_eq!(
            r.focus(),
            FocusState::Remote,
            "a transient 500 ms transport stall must not expire the default lease"
        );
        assert!(out.contains(&arm(Timer::ReceiverChallenge, issued + 100)));
        delayed_nonce = delayed_nonce.or(nonce);
        latest_nonce = nonce;
    }

    let resumed_at = 613;
    let out = r.handle(
        resumed_at,
        net(Message::LeaseRenew {
            lease_id: L,
            nonce: delayed_nonce.unwrap(),
        }),
    );
    assert!(out.contains(&arm(
        Timer::ReceiverLeaseDeadline,
        112 + timing.lease_ttl_ms
    )));
    let out = r.handle(
        resumed_at,
        net(Message::LeaseRenew {
            lease_id: L,
            nonce: latest_nonce.unwrap(),
        }),
    );
    let deadline = 612 + timing.lease_ttl_ms;
    assert!(out.contains(&arm(Timer::ReceiverLeaseDeadline, deadline)));
    assert_eq!(r.focus(), FocusState::Remote);

    // The source now really dies. Unanswered challenges cannot buy grace, and an
    // old queued renew cannot move the deadline past its original issue time + TTL.
    let out = r.handle(deadline, Input::TimerExpired(Timer::ReceiverLeaseDeadline));
    assert!(out.contains(&release(L)));
    assert_eq!(r.focus(), FocusState::Local);
    assert!(deadline <= resumed_at + timing.lease_ttl_ms);
    let out = r.handle(
        deadline + 1,
        net(Message::LeaseRenew {
            lease_id: L,
            nonce: delayed_nonce.unwrap(),
        }),
    );
    assert!(!out.iter().any(|output| matches!(
        output,
        Output::ArmTimer {
            timer: Timer::ReceiverLeaseDeadline,
            ..
        }
    )));
}

#[test]
fn adapter_capture_loss_keeps_receiver_capability() {
    let mut r = router(LOCAL);
    assert!(r.can_be_source());
    r.set_capabilities(Capabilities {
        capture: false,
        inject: true,
        absolute_pointer: true,
        emergency_hotkey: false,
    });
    assert!(!r.can_be_source());
    assert!(r.can_be_receiver());
    let out = r.handle(10, switch(SwitchTrigger::Edge));
    assert!(!out
        .iter()
        .any(|output| matches!(output, Output::Agent(AgentCommand::BeginCapture { .. }))));
    assert_eq!(r.focus(), FocusState::Local);
}

#[test]
fn s01_local_switch_starts_temporary_capture() {
    let mut r = router(LOCAL);
    assert_eq!(
        r.handle(10, switch(SwitchTrigger::Edge)),
        vec![
            begin(CaptureKind::Temporary, id(1), 200),
            arm(Timer::PrepareTimeout, 210),
            notice(Notice::Focus(FocusState::Preparing))
        ]
    );
    assert_eq!(r.focus(), FocusState::Preparing);
}

#[test]
fn preparing_ignores_repeated_edge_samples_without_rearming_capture() {
    let mut r = router(LOCAL);
    let mut outputs = r.handle(10, switch(SwitchTrigger::Edge));
    for now in 11..50 {
        let repeated = r.handle(now, switch(SwitchTrigger::Edge));
        assert!(repeated.is_empty(), "duplicate edge produced {repeated:?}");
        outputs.extend(repeated);
        assert_eq!(r.focus(), FocusState::Preparing);
        assert_eq!(r.lease().source().unwrap().lease_id, id(1));
    }
    assert_eq!(
        effects(&outputs),
        vec![begin(CaptureKind::Temporary, id(1), 200)]
    );
    // An explicit timeout still aborts the one pending transition.
    assert_eq!(
        r.handle(210, Input::TimerExpired(Timer::PrepareTimeout)),
        vec![end(id(1), true), notice(Notice::Focus(FocusState::Local))]
    );
}

#[test]
fn startup_reclaims_even_when_the_restored_owner_is_self() {
    let mut r = router(LOCAL).with_claim_on_startup(true);
    let out = r.on_start(10);
    assert!(out.contains(&Output::Broadcast(Message::OwnerClaim {
        serial: 2,
        reason: ClaimReason::Startup,
    })));
    assert_eq!(r.ownership().pending_claim().unwrap().serial, 2);
    r.handle(610, Input::TimerExpired(Timer::ClaimGrant));
    assert!(r.is_owner());
    assert_eq!(r.serial(), 2);
}

#[test]
fn startup_claim_moves_past_a_matching_serial_remembered_by_a_receiver() {
    let mut r = router(LOCAL).with_claim_on_startup(true);
    r.on_start(0);
    r.handle(1, hello(PEER, LOCAL, 2));
    assert_eq!(r.ownership().pending_claim().unwrap().serial, 3);
    r.handle(601, Input::TimerExpired(Timer::ClaimGrant));
    assert!(r.is_owner());
    assert_eq!(r.serial(), 3);
}

#[test]
fn higher_self_owner_gossip_reclaims_without_an_old_echo_cancelling_it() {
    for input in [
        hello(PEER, LOCAL, 7),
        net(Message::OwnerState {
            serial: 7,
            owner: Some(LOCAL),
            granted: false,
        }),
    ] {
        let mut r = router(LOCAL);
        r.handle(10, input);
        assert_eq!(r.max_seen_serial(), 7);
        assert_eq!(r.ownership().pending_claim().unwrap().serial, 8);
        r.handle(
            11,
            net(Message::OwnerState {
                serial: 7,
                owner: Some(LOCAL),
                granted: false,
            }),
        );
        assert_eq!(r.ownership().pending_claim().unwrap().serial, 8);
        r.handle(610, Input::TimerExpired(Timer::ClaimGrant));
        assert!(r.is_owner());
        assert_eq!(r.serial(), 8);
    }
}

#[test]
fn stale_enter_ack_reclaims_and_allows_the_next_edge() {
    let mut r = preparing();
    let out = r.handle(12, net(ack(id(1), Some(ErrorCode::StaleOwnerSerial))));
    assert!(out.contains(&end(id(1), true)));
    assert_eq!(r.focus(), FocusState::Local);
    assert!(r.lease().is_idle());
    assert_eq!(r.ownership().pending_claim().unwrap().serial, 2);
    r.handle(612, Input::TimerExpired(Timer::ClaimGrant));
    r.handle(613, switch(SwitchTrigger::Edge));
    let out = r.handle(
        614,
        Input::Agent(AgentEvent::CaptureReady { lease_id: id(2) }),
    );
    assert!(out.contains(&send(prepare(id(2), 2))));
}

#[test]
fn s02_capture_ack_capture_commits_and_renews() {
    let mut r = preparing();
    assert_eq!(
        r.handle(
            11,
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) })
        ),
        vec![send(prepare(id(1), 1))]
    );
    assert_eq!(
        r.handle(12, net(ack(id(1), None))),
        vec![begin(CaptureKind::Remote, id(1), 500)]
    );
    assert!(r
        .handle(
            13,
            net(Message::LeaseChallenge {
                lease_id: id(1),
                nonce: N,
                ttl_ms: 500
            })
        )
        .is_empty());
    assert_eq!(
        r.handle(
            14,
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) })
        ),
        vec![
            Output::CancelTimer(Timer::PrepareTimeout),
            send(Message::EnterCommit { lease_id: id(1) }),
            send(Message::LeaseRenew {
                lease_id: id(1),
                nonce: N
            }),
            arm(Timer::SourceLeaseWatchdog, 514),
            notice(Notice::Focus(FocusState::Remote)),
            arm(Timer::LeaseDiagnostic, 1014)
        ]
    );
    assert_eq!(r.focus(), FocusState::Remote);
}
#[test]
fn s03_prepare_timeout_replays_and_returns_local() {
    let mut r = preparing();
    assert_eq!(
        r.handle(210, Input::TimerExpired(Timer::PrepareTimeout)),
        vec![end(id(1), true), notice(Notice::Focus(FocusState::Local))]
    );
    assert!(r.lease().is_idle());
}
#[test]
fn s04_rejected_enter_ack_replays_and_clears_lease() {
    let mut r = preparing();
    assert_eq!(
        effects(&r.handle(12, net(ack(id(1), Some(ErrorCode::UnsupportedSession))))),
        vec![
            end(id(1), true),
            notice(Notice::Rejected {
                peer: PEER,
                code: ErrorCode::UnsupportedSession
            })
        ]
    );
    assert_eq!(r.focus(), FocusState::Local);
    assert!(r.lease().is_idle());
}
#[test]
fn s05_agent_capture_failed_replays_and_returns_local() {
    let mut r = preparing();
    let out = r.handle(
        12,
        Input::Agent(AgentEvent::Error {
            lease_id: Some(id(1)),
            code: ErrorCode::CaptureFailed,
        }),
    );
    assert!(
        out.contains(&end(id(1), true)),
        "S5 requires EndCapture with replay: {out:?}"
    );
    assert_eq!(r.focus(), FocusState::Local);
    assert!(r.lease().is_idle());
}
#[test]
fn s06_held_key_refuses_normal_switch() {
    let mut r = router(LOCAL);
    r.handle(
        1,
        Input::Agent(AgentEvent::PressedState {
            keys: vec![HidKey::keyboard(4)],
            buttons: vec![],
        }),
    );
    assert_eq!(
        r.handle(10, switch(SwitchTrigger::Edge)),
        vec![notice(Notice::SwitchRefused {
            trigger: SwitchTrigger::Edge,
            reason: TransitionRefusal::PressedInputHeld
        })]
    );
    assert_eq!(r.focus(), FocusState::Local);
}
#[test]
fn s07_remote_switch_releases_with_trigger_reason() {
    for (trigger, reason) in [
        (SwitchTrigger::Edge, LeaveReason::ReturnEdge),
        (SwitchTrigger::Hotkey, LeaveReason::Hotkey),
    ] {
        let mut r = remote();
        assert_eq!(
            effects(&r.handle(20, switch(trigger))),
            vec![end(id(1), false), leave(id(1), reason)]
        );
        assert_eq!(r.focus(), FocusState::Releasing);
    }
}
/// A return edge releases the *receiver* through `LeaveAck`, not through our own
/// capture coming back: the peer needs its own release round trip, and a second
/// `EnterPrepare` before that ack is what S14 answers with `LEASE_MISMATCH`.
#[test]
fn s35_return_edge_waits_for_the_leave_ack_not_for_our_own_release() {
    let mut r = remote();
    assert_eq!(
        effects(&r.handle(20, switch(SwitchTrigger::Edge))),
        vec![end(id(1), false), leave(id(1), LeaveReason::ReturnEdge)]
    );
    // The agent restored the local capture at once — that is not the receiver's
    // release, so the lease stays in the slot and nothing may open a new one.
    assert_eq!(
        effects(&r.handle(21, Input::Agent(AgentEvent::Released { lease_id: id(1) }))),
        vec![]
    );
    assert_eq!(
        effects(&r.handle(
            21,
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) })
        )),
        vec![]
    );
    assert_eq!(r.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&r.handle(22, switch(SwitchTrigger::Edge))),
        vec![notice(Notice::SwitchRefused {
            trigger: SwitchTrigger::Edge,
            reason: TransitionRefusal::NotOwner
        })]
    );
    assert_eq!(r.lease().source().unwrap().lease_id, id(1));
    // The ack is what frees the slot, and the next transition opens a fresh lease.
    assert_eq!(
        effects(&r.handle(23, net(Message::LeaveAck { lease_id: id(1) }))),
        vec![]
    );
    assert!(r.lease().is_idle());
    assert_eq!(
        effects(&r.handle(24, switch(SwitchTrigger::Edge))),
        vec![begin(CaptureKind::Temporary, id(2), 200)]
    );
    assert_eq!(r.focus(), FocusState::Preparing);

    // The other half of the same rule: a channel that can no longer deliver the
    // `LeaveAck` falls back to the conservative `claim_grant_timeout_ms` wait.
    let mut r = remote();
    r.handle(20, switch(SwitchTrigger::Edge));
    r.handle(21, Input::Agent(AgentEvent::Released { lease_id: id(1) }));
    let out = r.handle(22, Input::LinkDown { peer: PEER });
    assert_eq!(effects(&out), vec![]);
    assert!(out.contains(&arm(Timer::ReleaseGuard, 622)));
    assert_eq!(r.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&r.handle(622, Input::TimerExpired(Timer::ReleaseGuard))),
        vec![]
    );
    assert!(r.lease().is_idle());
}

/// A `LeaveAck` can be lost — a link replaced between the `Leave` and the ack is how CI
/// hit it — and the owner must still leave `Releasing`: the bound is the documented
/// conservative wait (`claim_grant_timeout_ms`), not a message that may never arrive.
#[test]
fn s36_a_lost_leave_ack_is_bounded_by_the_release_guard() {
    let mut r = remote();
    let out = r.handle(20, switch(SwitchTrigger::Edge));
    assert!(
        out.contains(&arm(Timer::ReleaseGuard, 620)),
        "the return edge arms the bound: {out:?}"
    );
    // Our own capture is back and the ack never arrives: nothing else happens, and the
    // release is still waiting (the session delivers the guard only when it is due).
    r.handle(21, Input::Agent(AgentEvent::Released { lease_id: id(1) }));
    assert_eq!(r.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&r.handle(620, Input::TimerExpired(Timer::ReleaseGuard))),
        vec![]
    );
    assert!(r.lease().is_idle());
    assert_eq!(r.focus(), FocusState::Local);
    // The walk continues instead of hanging: the next transition opens a fresh lease.
    assert_eq!(
        effects(&r.handle(621, switch(SwitchTrigger::Edge))),
        vec![begin(CaptureKind::Temporary, id(2), 200)]
    );
}

/// A channel replaced while releasing: the ack cannot be re-delivered on the lost one, so
/// the conservative wait runs from the close — the link coming back is not by itself the
/// receiver's release (`docs/05-protocol.md`: «…либо после закрытия прежнего канала и
/// консервативного ожидания 600 ms»).
#[test]
fn s37_a_link_replaced_while_releasing_ends_the_release() {
    let mut r = remote();
    r.handle(20, switch(SwitchTrigger::Edge));
    r.handle(21, Input::Agent(AgentEvent::Released { lease_id: id(1) }));
    let out = r.handle(30, Input::LinkDown { peer: PEER });
    assert!(
        out.contains(&arm(Timer::ReleaseGuard, 630)),
        "the close restarts the bound: {out:?}"
    );
    assert_eq!(r.focus(), FocusState::Releasing);
    // The replacement (a fresh `Hello`) does not release on its own: the receiver's own
    // lease may still be alive, which is what the conservative wait covers.
    assert_eq!(effects(&r.handle(40, hello(PEER, LOCAL, 1))), vec![]);
    assert_eq!(r.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&r.handle(630, Input::TimerExpired(Timer::ReleaseGuard))),
        vec![]
    );
    assert!(r.lease().is_idle());
    assert_eq!(r.focus(), FocusState::Local);
}

#[test]
fn s08_leave_ack_returns_local_and_burns_old_id() {
    let mut r = remote();
    r.handle(20, switch(SwitchTrigger::Edge));
    assert_eq!(
        effects(&r.handle(21, net(Message::LeaveAck { lease_id: id(1) }))),
        vec![]
    );
    assert_eq!(r.focus(), FocusState::Local);
    r.handle(
        22,
        net(Message::OwnerState {
            serial: 2,
            owner: Some(PEER),
            granted: true,
        }),
    );
    assert_eq!(
        effects(&r.handle(23, net(prepare(id(1), 2)))),
        vec![
            send(ack(id(1), Some(ErrorCode::LeaseMismatch))),
            notice(Notice::Rejected {
                peer: PEER,
                code: ErrorCode::LeaseMismatch
            })
        ]
    );
    assert!(r.lease().is_idle());
}
#[test]
fn s09_source_watchdog_closes_link_and_restores_local() {
    let mut r = remote();
    assert_eq!(
        effects(&r.handle(514, Input::TimerExpired(Timer::SourceLeaseWatchdog))),
        vec![
            end(id(1), false),
            Output::CloseLink {
                peer: PEER,
                code: ErrorCode::LeaseExpired
            },
            notice(Notice::LeaseExpired { lease_id: id(1) })
        ]
    );
    assert_eq!(r.focus(), FocusState::Local);
}
#[test]
fn s10_failed_inject_ack_aborts_immediately() {
    let mut r = remote();
    let out = r.handle(
        20,
        net(Message::InjectAck {
            lease_id: id(1),
            input_batch_seq: 3,
            applied_count: 0,
            actual_position: None,
            status: InjectStatus::Failed,
        }),
    );
    assert_eq!(
        effects(&out),
        vec![
            end(id(1), false),
            notice(Notice::InjectionAborted {
                lease_id: id(1),
                code: ErrorCode::InjectionFailed
            })
        ]
    );
    assert_eq!(r.focus(), FocusState::Local);
}
#[test]
fn s11_emergency_releases_immediately() {
    let mut r = remote();
    assert_eq!(
        effects(&r.handle(20, Input::Local(LocalInput::Emergency))),
        vec![end(id(1), false), leave(id(1), LeaveReason::Emergency)]
    );
    assert_eq!(r.focus(), FocusState::Local);
}

#[test]
fn emergency_cancels_preparing_without_waiting_for_capture_or_peer() {
    for capture_ready in [false, true] {
        let mut r = preparing();
        if capture_ready {
            r.handle(
                11,
                Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
            );
        }
        let out = r.handle(12, Input::Local(LocalInput::Emergency));
        assert_eq!(
            effects(&out),
            vec![end(id(1), false), leave(id(1), LeaveReason::Emergency)]
        );
        assert!(out.contains(&Output::CancelTimer(Timer::PrepareTimeout)));
        assert_eq!(r.focus(), FocusState::Local);
        assert!(r.lease().is_idle());

        for input in [
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
            net(ack(id(1), None)),
            Input::TimerExpired(Timer::PrepareTimeout),
        ] {
            let out = r.handle(13, input);
            assert!(!out.iter().any(|output| matches!(
                output,
                Output::Agent(AgentCommand::BeginCapture { .. })
                    | Output::Send {
                        message: Message::EnterCommit { .. },
                        ..
                    }
            )));
            assert_eq!(r.focus(), FocusState::Local);
            assert!(r.lease().is_idle());
        }
    }
}

#[test]
fn emergency_clears_idle_releasing_and_receiver_states_immediately() {
    let mut source_releasing = remote();
    source_releasing.handle(20, switch(SwitchTrigger::Edge));
    let mut receiver_preparing = router(PEER);
    receiver_preparing.handle(10, net(prepare(L, 1)));
    let mut receiver_releasing = active();
    receiver_releasing.handle(
        20,
        Input::Agent(AgentEvent::LocalInputDetected { lease_id: L }),
    );
    let mut receiver_expired = active();
    receiver_expired.handle(
        20,
        Input::Agent(AgentEvent::DeadlineExpired { lease_id: L }),
    );
    for mut r in [
        router(LOCAL),
        source_releasing,
        receiver_preparing,
        prepared(),
        active(),
        receiver_releasing,
        receiver_expired,
    ] {
        let expected = match r.lease() {
            LeaseSlot::Idle => vec![],
            LeaseSlot::Source(lease) => vec![
                end(lease.lease_id, false),
                leave(lease.lease_id, LeaveReason::Emergency),
            ],
            LeaseSlot::Receiver(lease) => vec![
                release(lease.lease_id),
                leave(lease.lease_id, LeaveReason::Emergency),
            ],
        };
        assert_eq!(
            effects(&r.handle(30, Input::Local(LocalInput::Emergency))),
            expected
        );
        assert!(r.lease().is_idle());
        assert_eq!(r.focus(), FocusState::Local);
    }
}

#[test]
fn protocol_refusal_aborts_only_matching_preparing_source() {
    for code in [ErrorCode::LeaseMismatch, ErrorCode::InvalidFrame] {
        let refusal = Message::ProtocolError {
            code,
            related_seq: 0,
        };
        let mut r = preparing();
        let out = r.handle(
            11,
            Input::Net {
                from: OTHER,
                message: refusal.clone(),
            },
        );
        assert_eq!(
            effects(&out),
            vec![notice(Notice::Rejected { peer: OTHER, code })]
        );
        assert_eq!(r.focus(), FocusState::Preparing);
        assert_eq!(r.lease().source().unwrap().lease_id, id(1));

        let out = r.handle(12, net(refusal.clone()));
        assert_eq!(
            effects(&out),
            vec![
                notice(Notice::Rejected { peer: PEER, code }),
                end(id(1), true)
            ]
        );
        assert!(out.contains(&Output::CancelTimer(Timer::PrepareTimeout)));
        assert_eq!(r.focus(), FocusState::Local);
        assert!(r.lease().is_idle());
        r.handle(
            13,
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
        );
        r.handle(14, net(ack(id(1), None)));
        r.handle(210, Input::TimerExpired(Timer::PrepareTimeout));
        assert_eq!(r.focus(), FocusState::Local);
        assert!(r.lease().is_idle());

        let mut r = remote();
        let out = r.handle(20, net(refusal));
        assert_eq!(
            effects(&out),
            vec![notice(Notice::Rejected { peer: PEER, code })]
        );
        assert_eq!(r.focus(), FocusState::Remote);
        assert_eq!(r.lease().source().unwrap().lease_id, id(1));
    }
}
#[test]
fn s12_owner_prepare_waits_for_agent_before_ack_and_challenge() {
    let mut r = router(PEER);
    assert_eq!(
        r.handle(10, net(prepare(L, 1))),
        vec![
            Output::Agent(AgentCommand::PrepareInject { lease_id: L }),
            arm(Timer::PrepareTimeout, 210)
        ]
    );
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Prepared);
    assert_eq!(
        r.handle(11, Input::Agent(AgentEvent::Ready { lease_id: L })),
        vec![
            send(ack(L, None)),
            send(Message::LeaseChallenge {
                lease_id: L,
                nonce: id(1),
                ttl_ms: 500
            }),
            arm(Timer::PrepareTimeout, 211)
        ]
    );
}

#[test]
fn receiver_missing_ready_or_commit_has_bounded_cleanup() {
    for ready in [false, true] {
        for released in [false, true] {
            let mut r = router(PEER);
            let out = r.handle(10, net(prepare(L, 1)));
            assert!(out.contains(&arm(Timer::PrepareTimeout, 210)));
            if ready {
                let out = r.handle(11, Input::Agent(AgentEvent::Ready { lease_id: L }));
                assert!(out.contains(&arm(Timer::PrepareTimeout, 211)));
            }
            let out = r.handle(211, Input::TimerExpired(Timer::PrepareTimeout));
            assert!(out.contains(&release(L)));
            assert!(out.contains(&arm(Timer::ReleaseGuard, 811)));
            assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Expired);
            let out = r.handle(212, Input::Agent(AgentEvent::Ready { lease_id: L }));
            assert!(effects(&out).is_empty());
            if released {
                r.handle(213, Input::Agent(AgentEvent::Released { lease_id: L }));
            } else {
                r.handle(811, Input::TimerExpired(Timer::ReleaseGuard));
            }
            assert!(r.lease().is_idle());
            let out = r.handle(812, net(prepare(id(2), 1)));
            assert!(out.contains(&Output::Agent(AgentCommand::PrepareInject {
                lease_id: id(2),
            })));
        }
    }
}

#[test]
fn receiver_invalid_frame_aborts_only_matching_pending_preparation() {
    let mut r = router(PEER);
    r.handle(10, net(prepare(L, 1)));
    let error = |lease_id| {
        Input::Agent(AgentEvent::Error {
            lease_id,
            code: ErrorCode::InvalidFrame,
        })
    };
    for lease_id in [None, Some(id(2))] {
        let out = r.handle(11, error(lease_id));
        assert!(!out.contains(&release(L)));
        assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Prepared);
    }
    let out = r.handle(12, error(Some(L)));
    assert!(out.contains(&release(L)));
    assert!(out.contains(&Output::CancelTimer(Timer::PrepareTimeout)));
    assert!(out.contains(&arm(Timer::ReleaseGuard, 612)));
    assert_eq!(
        r.lease().receiver().unwrap().state,
        ReceiverState::Releasing
    );
    r.handle(612, Input::TimerExpired(Timer::ReleaseGuard));
    assert!(r.lease().is_idle());
    let out = r.handle(613, net(prepare(id(2), 1)));
    assert!(out.contains(&Output::Agent(AgentCommand::PrepareInject {
        lease_id: id(2),
    })));

    let mut r = active();
    let out = r.handle(20, error(Some(L)));
    assert!(!out.contains(&release(L)));
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Active);
}
#[test]
fn s13_prepare_rejects_nonowner_stale_serial_and_revision() {
    for (from, serial, revision, code) in [
        (OTHER, 1, 0, ErrorCode::NotOwner),
        (PEER, 0, 0, ErrorCode::StaleOwnerSerial),
        (PEER, 1, 1, ErrorCode::TopologyChanged),
    ] {
        let mut r = router(PEER);
        let out = r.handle(
            10,
            Input::Net {
                from,
                message: Message::EnterPrepare {
                    lease_id: L,
                    topology_revision: revision,
                    owner_serial: serial,
                    position: position(),
                },
            },
        );
        assert_eq!(
            effects(&out),
            vec![
                Output::Send {
                    peer: from,
                    message: ack(L, Some(code))
                },
                notice(Notice::Rejected { peer: from, code })
            ]
        );
        assert!(r.lease().is_idle());
    }
}
#[test]
fn s14_second_lease_is_refused() {
    let mut r = active();
    assert_eq!(
        effects(&r.handle(20, net(prepare([7; 16], 1)))),
        vec![
            send(ack([7; 16], Some(ErrorCode::LeaseMismatch))),
            notice(Notice::Rejected {
                peer: PEER,
                code: ErrorCode::LeaseMismatch
            })
        ]
    );
    assert_eq!(r.lease().receiver().unwrap().lease_id, L);
    assert_eq!(r.focus(), FocusState::Remote);
}
#[test]
fn s15_commit_arms_deadline_challenge_and_repeat_guard() {
    let mut r = prepared();
    assert_eq!(
        r.handle(12, net(Message::EnterCommit { lease_id: L })),
        vec![
            Output::CancelTimer(Timer::PrepareTimeout),
            Output::Agent(AgentCommand::ArmDeadline {
                lease_id: L,
                remaining_ms: 500
            }),
            arm(Timer::ReceiverLeaseDeadline, 512),
            arm(Timer::ReceiverChallenge, 112),
            arm(Timer::RepeatGuard, 162),
            notice(Notice::Focus(FocusState::Remote)),
            arm(Timer::LeaseDiagnostic, 1012)
        ]
    );
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Active);
}
#[test]
fn s16_prepared_timeout_burns_id_and_refuses_late_commit() {
    let mut r = prepared();
    assert_eq!(
        effects(&r.handle(211, Input::TimerExpired(Timer::PrepareTimeout))),
        vec![release(L), notice(Notice::LeaseExpired { lease_id: L })]
    );
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Expired);
    assert_eq!(
        effects(&r.handle(212, net(Message::EnterCommit { lease_id: L }))),
        vec![send(ack(L, Some(ErrorCode::LeaseExpired)))]
    );
    r.handle(213, Input::Agent(AgentEvent::Released { lease_id: L }));
    assert_eq!(
        effects(&r.handle(214, net(prepare(L, 1)))),
        vec![
            send(ack(L, Some(ErrorCode::LeaseMismatch))),
            notice(Notice::Rejected {
                peer: PEER,
                code: ErrorCode::LeaseMismatch
            })
        ]
    );
}
#[test]
fn s17_renew_deadline_is_anchored_to_challenge_issue_time() {
    let mut r = active();
    let out = r.handle(
        100,
        net(Message::LeaseRenew {
            lease_id: L,
            nonce: id(1),
        }),
    );
    assert_eq!(
        out,
        vec![
            status(id(1), true, 0),
            arm(Timer::ReceiverLeaseDeadline, 511),
            arm(Timer::RepeatGuard, 250)
        ]
    );
    assert_eq!(r.lease().receiver().unwrap().deadline_ms, 511);
}
#[test]
fn s18_replayed_or_stale_nonce_does_not_extend_deadline() {
    let mut r = active();
    r.handle(
        100,
        net(Message::LeaseRenew {
            lease_id: L,
            nonce: id(1),
        }),
    );
    for nonce in [id(1), N] {
        assert_eq!(
            r.handle(101, net(Message::LeaseRenew { lease_id: L, nonce })),
            vec![status(nonce, false, 0)]
        );
        assert_eq!(r.lease().receiver().unwrap().deadline_ms, 511);
    }
}
#[test]
fn s19_expired_receiver_cannot_be_revived() {
    let mut r = active();
    assert_eq!(
        effects(&r.handle(512, Input::TimerExpired(Timer::ReceiverLeaseDeadline))),
        vec![release(L), notice(Notice::LeaseExpired { lease_id: L })]
    );
    for now in [513, 514] {
        assert_eq!(
            r.handle(
                now,
                net(Message::LeaseRenew {
                    lease_id: L,
                    nonce: id(1)
                })
            ),
            vec![status(id(1), false, 0)]
        );
    }
    assert_eq!(
        effects(&r.handle(515, net(Message::EnterCommit { lease_id: L }))),
        vec![send(ack(L, Some(ErrorCode::LeaseExpired)))]
    );
    assert_eq!(r.lease().receiver().unwrap().state, ReceiverState::Expired);
}
#[test]
fn s20_invalid_batches_fail_without_injection() {
    for (seq, count, code) in [
        (4, 1, ErrorCode::InvalidSequence),
        (5, 1, ErrorCode::InvalidSequence),
        (6, 0, ErrorCode::InvalidFrame),
        (6, 129, ErrorCode::InvalidFrame),
    ] {
        let mut r = active();
        r.handle(20, net(batch(5, 1)));
        let out = r.handle(21, net(batch(seq, count)));
        assert_eq!(
            effects(&out),
            vec![
                send(Message::InjectAck {
                    lease_id: L,
                    input_batch_seq: seq,
                    applied_count: 0,
                    actual_position: None,
                    status: InjectStatus::Failed
                }),
                notice(Notice::Rejected { peer: PEER, code })
            ]
        );
        assert_eq!(r.lease().receiver().unwrap().last_input_batch_seq, 5);
    }
}
#[test]
fn s21_input_batch_and_successful_result_acknowledge_progress() {
    let mut r = active();
    let Message::InputBatch { events, .. } = batch(5, 1) else {
        unreachable!()
    };
    assert_eq!(
        r.handle(20, net(batch(5, 1))),
        vec![
            arm(Timer::RepeatGuard, 170),
            Output::Agent(AgentCommand::Inject {
                lease_id: L,
                input_batch_seq: 5,
                events
            })
        ]
    );
    assert_eq!(
        effects(&r.handle(
            21,
            Input::Agent(AgentEvent::InjectResult {
                lease_id: L,
                input_batch_seq: 5,
                applied_count: 1,
                actual_position: Some(position()),
                status: InjectStatus::Ok
            })
        )),
        vec![send(Message::InjectAck {
            lease_id: L,
            input_batch_seq: 5,
            applied_count: 1,
            actual_position: Some(position()),
            status: InjectStatus::Ok
        })]
    );
    assert_eq!(r.lease().receiver().unwrap().last_input_batch_seq, 5);
}
#[test]
fn s22_receiver_local_input_releases_before_leave() {
    let mut r = active();
    assert_eq!(
        effects(&r.handle(
            20,
            Input::Agent(AgentEvent::LocalInputDetected { lease_id: L })
        )),
        vec![
            release(L),
            leave(L, LeaveReason::ReceiverLocalInput),
            notice(Notice::Takeover { lease_id: L })
        ]
    );
    assert_eq!(r.focus(), FocusState::Releasing);
}
#[test]
fn s23_leave_ack_waits_for_agent_released() {
    let mut r = active();
    assert_eq!(
        effects(&r.handle(
            20,
            net(Message::Leave {
                lease_id: L,
                reason: LeaveReason::Hotkey
            })
        )),
        vec![release(L)]
    );
    assert_eq!(r.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&r.handle(21, Input::Agent(AgentEvent::Released { lease_id: L }))),
        vec![send(Message::LeaveAck { lease_id: L })]
    );
    assert!(r.lease().is_idle());
}

#[test]
fn receiver_duplicate_and_crossed_leave_wait_for_local_release() {
    for takeover in [false, true] {
        for guard_expires in [false, true] {
            let mut r = active();
            let initial = if takeover {
                Input::Agent(AgentEvent::LocalInputDetected { lease_id: L })
            } else {
                net(Message::Leave {
                    lease_id: L,
                    reason: LeaveReason::ReturnEdge,
                })
            };
            let out = r.handle(20, initial);
            assert!(out.contains(&release(L)));
            assert!(out.contains(&arm(Timer::ReleaseGuard, 620)));
            assert_eq!(r.focus(), FocusState::Releasing);

            for message in [
                Message::Leave {
                    lease_id: L,
                    reason: LeaveReason::ReturnEdge,
                },
                Message::Leave {
                    lease_id: L,
                    reason: LeaveReason::ReturnEdge,
                },
                Message::LeaveAck { lease_id: L },
            ] {
                let out = r.handle(21, net(message));
                assert!(effects(&out).is_empty(), "{out:?}");
                assert!(!out.contains(&Output::CancelTimer(Timer::ReleaseGuard)));
                assert_eq!(r.focus(), FocusState::Releasing);
                assert_eq!(r.lease().receiver().unwrap().lease_id, L);
            }

            let out = r.handle(22, net(prepare(id(2), 1)));
            assert!(!out.contains(&Output::Agent(AgentCommand::PrepareInject {
                lease_id: id(2),
            })));
            assert_eq!(r.lease().receiver().unwrap().lease_id, L);

            let out = if guard_expires {
                r.handle(620, Input::TimerExpired(Timer::ReleaseGuard))
            } else {
                r.handle(23, Input::Agent(AgentEvent::Released { lease_id: L }))
            };
            if guard_expires {
                assert!(effects(&out).is_empty());
            } else {
                assert_eq!(effects(&out), vec![send(Message::LeaveAck { lease_id: L })]);
            }
            assert!(r.lease().is_idle());
            assert_eq!(r.focus(), FocusState::Local);
            let out = r.handle(621, Input::Agent(AgentEvent::Released { lease_id: L }));
            assert!(effects(&out).is_empty());
        }
    }
}
#[test]
fn s24_silence_fires_repeat_guard_once() {
    let mut r = active();
    // A timer is delivered once when due, and is not rearmed without new activity.
    assert_eq!(
        r.handle(162, Input::TimerExpired(Timer::RepeatGuard)),
        vec![notice(Notice::RepeatGuardElapsed { lease_id: L })]
    );
    assert!(r.handle(163, Input::Local(LocalInput::Resume)).is_empty());
    assert_eq!(r.focus(), FocusState::Remote);
}
#[test]
fn s25_handoff_waits_for_released_or_capture_ready() {
    for confirmation in [
        AgentEvent::Released { lease_id: id(1) },
        AgentEvent::CaptureReady { lease_id: id(1) },
    ] {
        let mut r = remote();
        assert_eq!(
            effects(&r.handle(20, higher_claim())),
            vec![end(id(1), false)]
        );
        assert_eq!(r.focus(), FocusState::Releasing);
        let out = r.handle(21, Input::Agent(confirmation));
        assert_eq!(effects(&out), vec![grant(2, PEER)]);
        assert!(r.lease().is_idle());
        assert_eq!(r.owner(), Some(&PEER));
    }
}
#[test]
fn s26_release_precedes_ownership_grant() {
    let mut receiver_owner = active();
    receiver_owner.handle(15, claim());
    receiver_owner.handle(
        16,
        net(Message::OwnerState {
            serial: 2,
            owner: Some(LOCAL),
            granted: true,
        }),
    );
    assert!(receiver_owner.is_owner());
    for (mut r, lease_id, command, serial) in [
        (remote(), id(1), end(id(1), false), 2),
        (receiver_owner, L, release(L), 3),
    ] {
        let mut out = r.handle(
            20,
            net(Message::OwnerClaim {
                serial,
                reason: ClaimReason::User,
            }),
        );
        no_grant(&out);
        assert_eq!(effects(&out), vec![command.clone()]);
        out.extend(r.handle(21, Input::Agent(AgentEvent::Released { lease_id })));
        let expected_grant = grant(serial, PEER);
        // The receiver may also owe its previous source a LeaveAck. Check the
        // complete release/grant subsequence without constraining that extra ack.
        let ownership_effects: Vec<_> = effects(&out)
            .into_iter()
            .filter(|effect| {
                !matches!(
                    effect,
                    Output::Send {
                        message: Message::LeaveAck { .. },
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(ownership_effects, vec![command, expected_grant]);
        assert!(r.lease().is_idle());
    }
}
#[test]
fn s27_unanswered_claim_self_grants_at_timeout() {
    let mut r = router(PEER);
    let out = r.handle(10, claim());
    assert_eq!(
        effects(&out),
        vec![Output::Broadcast(Message::OwnerClaim {
            serial: 2,
            reason: ClaimReason::User
        })]
    );
    assert!(out.contains(&arm(Timer::ClaimGrant, 610)));
    no_grant(&r.handle(609, Input::TimerExpired(Timer::ClaimGrant)));
    assert_eq!(
        effects(&r.handle(610, Input::TimerExpired(Timer::ClaimGrant))),
        vec![grant(2, LOCAL)]
    );
    assert!(r.is_owner());
    assert_eq!(r.serial(), 2);
}
#[test]
fn s28_higher_owner_state_abandons_claim() {
    let mut r = router(PEER);
    r.handle(10, claim());
    assert_eq!(
        effects(&r.handle(
            20,
            net(Message::OwnerState {
                serial: 3,
                owner: Some(OTHER),
                granted: false
            })
        )),
        vec![notice(Notice::ClaimRefused { code: None })]
    );
    assert_eq!(r.owner(), Some(&OTHER));
    assert_eq!(r.ownership().state(), OwnershipState::Follower);
    no_grant(&r.handle(610, Input::TimerExpired(Timer::ClaimGrant)));
}
#[test]
fn s29_protocol_refusal_cancels_self_grant() {
    for code in [ErrorCode::CannotOwn, ErrorCode::StaleOwnerSerial] {
        let mut r = router(PEER);
        r.handle(10, claim());
        assert_eq!(
            effects(&r.handle(
                20,
                net(Message::ProtocolError {
                    code,
                    related_seq: 0
                })
            )),
            vec![notice(Notice::ClaimRefused { code: Some(code) })]
        );
        no_grant(&r.handle(610, Input::TimerExpired(Timer::ClaimGrant)));
        assert_eq!(r.ownership().state(), OwnershipState::Follower);
    }
}
#[test]
fn s30_gossip_or_owner_state_deposes_with_release_before_leave() {
    for input in [
        hello(OTHER, OTHER, 2),
        net(Message::OwnerState {
            serial: 2,
            owner: Some(OTHER),
            granted: false,
        }),
    ] {
        let mut r = remote();
        assert_eq!(
            effects(&r.handle(20, input)),
            vec![
                notice(Notice::OwnershipLost {
                    serial: 2,
                    owner: OTHER
                }),
                end(id(1), false),
                leave(id(1), LeaveReason::OwnershipLost)
            ]
        );
        assert_eq!(r.ownership().state(), OwnershipState::Deposed);
        r.handle(21, Input::Agent(AgentEvent::Released { lease_id: id(1) }));
        assert_eq!(r.owner(), Some(&OTHER));
        assert_eq!(r.focus(), FocusState::Local);
    }
}
#[test]
fn s31_link_down_releases_both_roles_and_source_waits_600ms() {
    let mut source = remote();
    let out = source.handle(20, Input::LinkDown { peer: PEER });
    assert_eq!(effects(&out), vec![end(id(1), false)]);
    assert!(out.contains(&arm(Timer::ReleaseGuard, 620)));
    assert_eq!(source.focus(), FocusState::Releasing);
    source.handle(619, Input::Local(LocalInput::Resume));
    assert_eq!(source.focus(), FocusState::Releasing);
    assert_eq!(
        effects(&source.handle(620, Input::TimerExpired(Timer::ReleaseGuard))),
        vec![]
    );
    assert_eq!(source.focus(), FocusState::Local);
    let mut receiver = active();
    assert_eq!(
        effects(&receiver.handle(20, Input::LinkDown { peer: PEER })),
        vec![release(L)]
    );
    assert_eq!(receiver.focus(), FocusState::Releasing);
    receiver.handle(21, Input::Agent(AgentEvent::Released { lease_id: L }));
    assert!(receiver.lease().is_idle());
}
#[test]
fn s32_suspend_or_lock_releases_both_roles_immediately() {
    for (mut r, lease_id, command) in [
        (remote(), id(1), end(id(1), false)),
        (active(), L, release(L)),
    ] {
        assert_eq!(
            effects(&r.handle(20, Input::Local(LocalInput::SuspendOrLock))),
            vec![command, leave(lease_id, LeaveReason::SuspendOrLock)]
        );
        assert_eq!(r.focus(), FocusState::Local);
        assert!(r.lease().is_idle());
    }
}
#[test]
fn s33_agent_lost_blocks_until_agent_ready() {
    for (mut r, lease_id, command) in [
        (remote(), id(1), end(id(1), false)),
        (active(), L, release(L)),
    ] {
        assert_eq!(
            effects(&r.handle(20, Input::Agent(AgentEvent::AgentLost))),
            vec![command, leave(lease_id, LeaveReason::AgentLost)]
        );
        assert_eq!(r.focus(), FocusState::Blocked);
        assert_eq!(
            effects(&r.handle(21, Input::Agent(AgentEvent::AgentReady))),
            vec![]
        );
        assert_eq!(r.focus(), FocusState::Local);
    }
}
#[test]
fn s34_refused_switches_send_nothing() {
    let mut blocked = router(LOCAL);
    blocked.handle(1, Input::Agent(AgentEvent::AgentLost));
    let mut unsupported = router(LOCAL);
    unsupported.handle(
        1,
        Input::Hello {
            peer: PEER,
            hello: PeerHello {
                can_own: true,
                capabilities: Capabilities {
                    inject: false,
                    ..capabilities()
                },
                owner_serial: 1,
                owner: Some(LOCAL),
            },
        },
    );
    for (mut r, reason) in [
        (router(PEER), TransitionRefusal::NotOwner),
        (blocked, TransitionRefusal::AgentBlocked),
        (unsupported, TransitionRefusal::ReceiverUnsupported),
    ] {
        let before = r.focus();
        assert_eq!(
            r.handle(10, switch(SwitchTrigger::Hotkey)),
            vec![notice(Notice::SwitchRefused {
                trigger: SwitchTrigger::Hotkey,
                reason
            })]
        );
        assert_eq!(r.focus(), before);
        assert!(r.lease().is_idle());
    }
}

/// A source-side router whose own `lease_ttl_ms` is `lease_ttl_ms` (audit F2).
fn source_router(lease_ttl_ms: u64) -> FocusRouter {
    let mut r = FocusRouter::new(
        LOCAL,
        Timing {
            lease_ttl_ms,
            ..Timing::default()
        },
        SequentialIds::default(),
        capabilities(),
    )
    .with_persisted_ownership(1, Some((1, LOCAL)));
    r.handle(0, hello(PEER, LOCAL, 1));
    r
}

/// Audit F2: the source's lease watchdog is bounded by the `lease_ttl_ms` the *receiver*
/// advertised in its `LeaseChallenge`, not by the source's own configured value.
///
/// `docs/05-protocol.md`, «Watchdog без зависимости от синхронизации часов»: the receiver
/// issues the challenges and anchors every deadline at `issue_time + lease_ttl`, and «Все
/// дедлайны lease выводятся из одного параметра `lease_ttl_ms`». Each node configures its
/// own value, so the source must adopt the number on the wire: with its own 300 ms it closed
/// a lease the 1000 ms receiver still considered alive.
#[test]
fn the_source_watchdog_uses_the_receivers_advertised_lease_ttl() {
    let mut r = source_router(300);
    r.handle(10, switch(SwitchTrigger::Edge));
    r.handle(
        11,
        Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
    );
    r.handle(12, net(ack(id(1), None)));
    // The receiver's own `lease_ttl_ms` is 1000 ms, more than three times the source's.
    r.handle(
        13,
        net(Message::LeaseChallenge {
            lease_id: id(1),
            nonce: N,
            ttl_ms: 1000,
        }),
    );
    let committed = r.handle(
        14,
        Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
    );
    assert_eq!(r.focus(), FocusState::Remote);
    assert!(
        committed.contains(&arm(Timer::SourceLeaseWatchdog, 1_014)),
        "the commit watchdog must be bounded by the receiver's ttl (14 + 1000): {committed:?}"
    );
    let status = r.handle(
        600,
        net(Message::LeaseStatus {
            lease_id: id(1),
            nonce: N,
            accepted: true,
            last_input_batch_seq: 0,
        }),
    );
    assert!(
        status.contains(&arm(Timer::SourceLeaseWatchdog, 1_600)),
        "the re-armed watchdog must use the receiver's ttl (600 + 1000): {status:?}"
    );
}

/// The advertised value is a peer's number: outside the documented 300..=1000 it is clamped,
/// so a broken peer cannot park our watchdog a millisecond or a day away (audit F2).
#[test]
fn a_lease_challenge_ttl_outside_the_documented_range_is_clamped() {
    for (advertised, effective) in [(1u32, 300u64), (600_000, 1_000)] {
        let mut r = source_router(300);
        r.handle(10, switch(SwitchTrigger::Edge));
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
                ttl_ms: advertised,
            }),
        );
        let committed = r.handle(
            14,
            Input::Agent(AgentEvent::CaptureReady { lease_id: id(1) }),
        );
        assert!(
            committed.contains(&arm(Timer::SourceLeaseWatchdog, 14 + effective)),
            "advertised {advertised} ms must clamp to {effective} ms: {committed:?}"
        );
    }
}
