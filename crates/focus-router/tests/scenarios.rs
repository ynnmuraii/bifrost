mod common;
use common::*;
use focus_router::*;
use input_model::{CaptureKind, HidKey};

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
            notice(Notice::Focus(FocusState::Remote))
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
fn s12_owner_prepare_waits_for_agent_before_ack_and_challenge() {
    let mut r = router(PEER);
    assert_eq!(
        r.handle(10, net(prepare(L, 1))),
        vec![Output::Agent(AgentCommand::PrepareInject { lease_id: L })]
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
            notice(Notice::Focus(FocusState::Remote))
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
