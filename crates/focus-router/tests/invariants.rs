mod common;
use common::*;
use focus_router::*;
use input_model::{HidKey, MouseButton};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed, TestRng, TestRunner};
use std::collections::BTreeSet;

fn runner() -> TestRunner {
    // Deliberately avoid Config::default(): it reads PROPTEST_* environment variables.
    // No persistence, process forking, entropy, or wall-clock shrink limit.
    let config = Config {
        cases: 128,
        max_local_rejects: 65536,
        max_global_rejects: 1024,
        max_flat_map_regens: 1000000,
        failure_persistence: None,
        source_file: None,
        test_name: None,
        max_shrink_time: 0,
        max_shrink_iters: 4096,
        max_default_size_range: 100,
        result_cache: proptest::test_runner::noop_result_cache,
        verbose: 0,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(71),
        _non_exhaustive: (),
    };
    TestRunner::new_with_rng(config, TestRng::from_seed(RngAlgorithm::ChaCha, &[71; 32]))
}

fn current_id(r: &FocusRouter) -> LeaseId {
    match r.lease() {
        LeaseSlot::Idle => L,
        LeaseSlot::Source(l) => l.lease_id,
        LeaseSlot::Receiver(l) => l.lease_id,
    }
}

#[test]
fn invariant_1_no_overlapping_agent_leases_in_generated_traces() {
    runner()
        .run(
            &(
                any::<bool>(),
                prop::collection::vec((0u8..14, 1u64..40), 1..100),
            ),
            |(as_owner, steps)| {
                let mut r = router(if as_owner { LOCAL } else { PEER });
                let mut resources = BTreeSet::new();
                let mut now = 20;
                for (op, delta) in steps {
                    now += delta;
                    let lease_id = current_id(&r);
                    let input = match op {
                        0 => switch(SwitchTrigger::Edge),
                        1 => net(prepare(id(now), r.serial())),
                        2 => Input::Agent(AgentEvent::CaptureReady { lease_id }),
                        3 => Input::Agent(AgentEvent::Ready { lease_id }),
                        4 => net(ack(lease_id, None)),
                        5 => net(Message::EnterCommit { lease_id }),
                        6 => Input::Local(LocalInput::Emergency),
                        7 => Input::Local(LocalInput::SuspendOrLock),
                        8 => Input::LinkDown { peer: PEER },
                        9 => Input::Agent(AgentEvent::AgentLost),
                        10 => Input::Agent(AgentEvent::AgentReady),
                        11 if r.focus() == FocusState::Releasing => {
                            Input::Agent(AgentEvent::Released { lease_id })
                        }
                        12 => net(Message::OwnerState {
                            serial: now,
                            owner: Some(if as_owner { PEER } else { LOCAL }),
                            granted: false,
                        }),
                        _ => Input::Local(LocalInput::Resume),
                    };
                    for out in r.handle(now, input) {
                        match out {
                            Output::Agent(
                                AgentCommand::BeginCapture { lease_id, .. }
                                | AgentCommand::PrepareInject { lease_id },
                            ) => {
                                resources.insert(lease_id);
                            }
                            Output::Agent(
                                AgentCommand::EndCapture { lease_id, .. }
                                | AgentCommand::ReleaseSession { lease_id },
                            ) => {
                                resources.remove(&lease_id);
                            }
                            _ => {}
                        }
                        prop_assert!(
                            resources.len() <= 1,
                            "agent has overlapping leases: {:?}",
                            resources
                        );
                    }
                    if let Some(id) = resources.first() {
                        prop_assert_eq!(*id, current_id(&r));
                    }
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn invariant_2_burned_lease_never_accepts_late_traffic() {
    runner()
        .run(
            &(0u8..5, prop::collection::vec(0u8..7, 1..80)),
            |(expiry, late)| {
                let mut r = if expiry == 0 { prepared() } else { active() };
                let terminal = match expiry {
                    0 => Input::TimerExpired(Timer::PrepareTimeout),
                    1 => Input::TimerExpired(Timer::ReceiverLeaseDeadline),
                    2 => Input::Agent(AgentEvent::InjectResult {
                        lease_id: L,
                        input_batch_seq: 1,
                        applied_count: 0,
                        actual_position: None,
                        status: InjectStatus::Failed,
                    }),
                    3 => Input::LinkDown { peer: PEER },
                    _ => Input::Agent(AgentEvent::DeadlineExpired { lease_id: L }),
                };
                r.handle(600, terminal);
                prop_assert_ne!(r.focus(), FocusState::Remote);
                for (step, op) in late.into_iter().enumerate() {
                    let input = match op {
                        0 => net(Message::LeaseRenew {
                            lease_id: L,
                            nonce: id(1),
                        }),
                        1 => net(Message::EnterCommit { lease_id: L }),
                        2 => net(prepare(L, 1)),
                        3 => Input::Agent(AgentEvent::Ready { lease_id: L }),
                        4 => net(batch(10 + step as u64, 1)),
                        5 => Input::Agent(AgentEvent::Released { lease_id: L }),
                        _ => net(Message::LeaseRenew {
                            lease_id: L,
                            nonce: N,
                        }),
                    };
                    let out = r.handle(601 + step as u64, input);
                    let revived = out.iter().any(|o| {
                        matches!(
                            o,
                            Output::Agent(
                                AgentCommand::Inject { .. }
                                    | AgentCommand::PrepareInject { .. }
                                    | AgentCommand::ArmDeadline { .. }
                            ) | Output::Send {
                                message: Message::EnterAck { accepted: true, .. }
                                    | Message::LeaseStatus { accepted: true, .. },
                                ..
                            }
                        )
                    });
                    prop_assert!(!revived, "burned id accepted: {:?}", out);
                    prop_assert_ne!(r.focus(), FocusState::Remote);
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn invariant_3_prepare_requires_exact_current_owner_pair() {
    runner()
        .run(
            &prop::collection::vec((0u8..4, 1u64..30, any::<bool>()), 1..60),
            |steps| {
                let mut r = router(PEER);
                for (index, (kind, serial, other)) in steps.into_iter().enumerate() {
                    let now = 20 + index as u64 * 3;
                    r.handle(now, Input::Local(LocalInput::Emergency));
                    if kind == 0 {
                        r.handle(
                            now,
                            net(Message::OwnerState {
                                serial,
                                owner: Some(if other { OTHER } else { PEER }),
                                granted: false,
                            }),
                        );
                    }
                    let owner = *r.owner().unwrap();
                    let from = if other { OTHER } else { PEER };
                    let proposed = if kind == 1 { r.serial() } else { serial };
                    let allowed = from == owner && proposed == r.serial();
                    let lease_id = id(1000 + index as u64);
                    let out = r.handle(
                        now + 1,
                        Input::Net {
                            from,
                            message: prepare(lease_id, proposed),
                        },
                    );
                    let expected = if allowed {
                        vec![Output::Agent(AgentCommand::PrepareInject { lease_id })]
                    } else {
                        let code = if proposed < r.serial() {
                            ErrorCode::StaleOwnerSerial
                        } else {
                            ErrorCode::NotOwner
                        };
                        vec![
                            Output::Send {
                                peer: from,
                                message: ack(lease_id, Some(code)),
                            },
                            notice(Notice::Rejected { peer: from, code }),
                        ]
                    };
                    prop_assert_eq!(effects(&out), expected);
                    prop_assert_eq!(r.lease().is_idle(), !allowed);
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn invariant_4_pressed_inputs_gate_switch_but_never_emergency() {
    runner()
        .run(
            &(
                0u8..8,
                prop::collection::vec((any::<bool>(), any::<bool>(), any::<bool>()), 1..60),
            ),
            |(initial, steps)| {
                let mut r = match initial {
                    0 => router(LOCAL),
                    1 => preparing(),
                    2 => remote(),
                    3 => active(),
                    4 => {
                        let mut r = remote();
                        r.handle(15, switch(SwitchTrigger::Edge));
                        r
                    }
                    5 => prepared(),
                    6 => {
                        let mut r = active();
                        r.handle(512, Input::TimerExpired(Timer::ReceiverLeaseDeadline));
                        r
                    }
                    _ => {
                        let mut r = router(LOCAL);
                        r.handle(15, Input::Agent(AgentEvent::AgentLost));
                        r
                    }
                };
                for (index, (key, button, emergency)) in steps.into_iter().enumerate() {
                    let now = 600 + index as u64 * 2;
                    r.handle(
                        now,
                        Input::Agent(AgentEvent::PressedState {
                            keys: if key {
                                vec![HidKey::keyboard(4)]
                            } else {
                                vec![]
                            },
                            buttons: if button {
                                vec![MouseButton::Left]
                            } else {
                                vec![]
                            },
                        }),
                    );
                    let before = r.focus();
                    let input = if emergency {
                        Input::Local(LocalInput::Emergency)
                    } else {
                        switch(SwitchTrigger::Edge)
                    };
                    let out = r.handle(now + 1, input);
                    if emergency {
                        prop_assert!(r.lease().is_idle());
                        prop_assert!(matches!(r.focus(), FocusState::Local | FocusState::Blocked));
                        prop_assert!(
                            !out.iter()
                                .any(|o| matches!(o, Output::Notice(Notice::SwitchRefused { .. }))),
                            "emergency was refused"
                        );
                    } else if before == FocusState::Local && r.is_owner() && (key || button) {
                        prop_assert_eq!(
                            out,
                            vec![notice(Notice::SwitchRefused {
                                trigger: SwitchTrigger::Edge,
                                reason: TransitionRefusal::PressedInputHeld
                            })]
                        );
                        prop_assert!(r.lease().is_idle());
                    }
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn invariant_5_grants_require_idle_slot_after_release() {
    runner()
        .run(
            &(any::<bool>(), prop::collection::vec(0u8..4, 1..30)),
            |(receiver, steps)| {
                let mut r = if receiver { active() } else { remote() };
                let lease_id = current_id(&r);
                let start = if receiver { claim() } else { higher_claim() };
                let out = r.handle(20, start);
                prop_assert!(!out.contains(&grant(2, LOCAL)) && !out.contains(&grant(2, PEER)));
                let mut release_requested = out.iter().any(|o| {
                    matches!(
                        o,
                        Output::Agent(
                            AgentCommand::EndCapture { .. } | AgentCommand::ReleaseSession { .. }
                        )
                    )
                });
                for (index, op) in steps.into_iter().enumerate() {
                    let input = match op {
                        0 => Input::TimerExpired(Timer::ClaimGrant),
                        1 if release_requested => Input::Agent(AgentEvent::Released { lease_id }),
                        2 => Input::Local(LocalInput::Emergency),
                        _ => Input::Local(LocalInput::Resume),
                    };
                    let out = r.handle(620 + index as u64, input);
                    for effect in &out {
                        if matches!(
                            effect,
                            Output::Agent(
                                AgentCommand::EndCapture { .. }
                                    | AgentCommand::ReleaseSession { .. }
                            )
                        ) {
                            release_requested = true;
                        }
                        if matches!(
                            effect,
                            Output::Broadcast(Message::OwnerState { granted: true, .. })
                        ) {
                            prop_assert!(release_requested, "grant without release: {:?}", out);
                            prop_assert!(
                                r.lease().is_idle(),
                                "grant while lease slot is occupied: {:?}",
                                r.lease()
                            );
                        }
                    }
                }
                Ok(())
            },
        )
        .unwrap();
}

#[test]
fn invariant_6_startup_claim_is_one_above_every_observed_serial() {
    runner()
        .run(
            &(0u64..10000, prop::collection::vec(0u64..10000, 0..40)),
            |(persisted, gossip)| {
                let mut r = FocusRouter::new(
                    LOCAL,
                    Timing::default(),
                    SequentialIds::default(),
                    capabilities(),
                )
                .with_persisted_ownership(persisted, None)
                .with_claim_on_startup(true);
                let mut max_seen = persisted;
                for (index, serial) in gossip.iter().copied().enumerate() {
                    r.handle(index as u64, hello(PEER, PEER, serial));
                    max_seen = max_seen.max(serial);
                    prop_assert_eq!(r.max_seen_serial(), max_seen);
                    prop_assert!(!r.is_owner());
                }
                let now = gossip.len() as u64;
                let out = r.on_start(now);
                prop_assert_eq!(
                    effects(&out),
                    vec![Output::Broadcast(Message::OwnerClaim {
                        serial: max_seen + 1,
                        reason: ClaimReason::Startup
                    })]
                );
                prop_assert!(out.contains(&arm(Timer::ClaimGrant, now + 600)));
                let early = r.handle(now + 599, Input::TimerExpired(Timer::ClaimGrant));
                prop_assert!(!early.contains(&grant(max_seen + 1, LOCAL)));
                let due = r.handle(now + 600, Input::TimerExpired(Timer::ClaimGrant));
                prop_assert_eq!(effects(&due), vec![grant(max_seen + 1, LOCAL)]);
                prop_assert_eq!(r.serial(), max_seen + 1);
                Ok(())
            },
        )
        .unwrap();
}
