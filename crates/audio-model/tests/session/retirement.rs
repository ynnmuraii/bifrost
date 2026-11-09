use super::*;

fn replacement_offer() -> Offer {
    let mut offer = Pair::new(true, true).offer();
    offer.session = [81; 16];
    offer.listener_nonce = [82; 32];
    offer
}

#[test]
fn native_retirement_new_on_cannot_leave_while_native_retirement_is_outstanding() {
    let (mut pair, old) = source_stop_after_logical_deadline();
    let mut effects = Vec::new();
    let result = pair.source.handle(
        1022,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [80; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
        &mut effects,
    );
    assert!(!effects.iter().any(|effect| matches!(effect,
        Effect::Send { message: Control::StartRequest(_), .. }
        | Effect::StartChild { .. })),
        "logical Stopped is not native retirement; ON crossed the boundary: result={result:?}, effects={effects:?}");
    assert_eq!(pair.source.session(LISTENER).unwrap().session, old.session);
    assert_eq!(
        pair.source
            .handle(1023, Input::ChildStopped(old), &mut Vec::new()),
        Ok(()),
        "old native retirement must remain addressable after the new ON intent"
    );
}

#[test]
fn native_retirement_remote_offer_cannot_replace_unretired_native_owner() {
    let (mut pair, old) = source_stop_after_logical_deadline();
    let mut effects = Vec::new();
    let result = pair.source.handle(
        1022,
        Input::Receive {
            parent: pair.sp,
            message: Control::Offer(replacement_offer()),
        },
        &mut effects,
    );
    assert!(!effects.iter().any(|effect| matches!(effect,
        Effect::GenerateSourceNonce { .. } | Effect::DeriveKeys { .. }
        | Effect::PrepareChild { .. } | Effect::StartChild { .. })),
        "replacement negotiation reached native setup before old cleanup: result={result:?}, effects={effects:?}");
    assert_eq!(
        pair.source.session(LISTENER).unwrap().session,
        old.session,
        "the only public session must still identify the outstanding native owner"
    );
    assert_eq!(
        pair.source
            .handle(1023, Input::ChildStopped(old), &mut Vec::new()),
        Ok(())
    );
}

#[test]
fn native_retirement_matching_retirement_allows_exactly_one_fresh_native_start() {
    let (mut pair, old) = source_stop_after_logical_deadline();
    let mut stale = old;
    stale.worker_generation = [99; 16];
    assert_eq!(
        pair.source
            .handle(1022, Input::ChildStopped(stale), &mut Vec::new()),
        Err(Error::StaleWorker)
    );
    run(&mut pair.source, 1022, Input::ChildStopped(old));
    let offer = replacement_offer();
    let effects = run(
        &mut pair.source,
        1023,
        Input::Receive {
            parent: pair.sp,
            message: Control::Offer(offer),
        },
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect,
        Effect::GenerateSourceNonce { session, .. } if *session == offer.session))
            .count(),
        1
    );
    run(
        &mut pair.source,
        1024,
        Input::SourceNonce {
            parent: pair.sp,
            session: offer.session,
            nonce: [83; 32],
        },
    );
    let view = pair.source.session(LISTENER).unwrap();
    assert_eq!(view.session, offer.session);
    run(
        &mut pair.source,
        1025,
        Input::KeysReady {
            view,
            transcript_sha256: [84; 32],
        },
    );
    run(&mut pair.source, 1026, Input::ChildPrepared(view));
    let commit = Input::Receive {
        parent: pair.sp,
        message: Control::Commit {
            session: view.session,
            transcript_sha256: [84; 32],
        },
    };
    let starts_once = run(&mut pair.source, 1027, commit);
    assert_eq!(starts(&starts_once), 1);
    assert!(starts_once.iter().any(|effect| matches!(effect,
        Effect::StartChild { view } if view.session == offer.session)));
    let mut duplicate = Vec::new();
    let _ = pair.source.handle(1028, commit, &mut duplicate);
    assert_eq!(
        starts(&duplicate),
        0,
        "duplicate commit cannot duplicate native ownership"
    );
    assert_eq!(
        pair.source
            .handle(1029, Input::ChildStopped(old), &mut Vec::new()),
        Err(Error::StaleWorker),
        "late old stop cannot retire the replacement"
    );
}

#[test]
fn native_retirement_real_worker_loss_is_not_positive_native_retirement() {
    let (mut pair, old) = source_stop_after_logical_deadline();
    run(
        &mut pair.source,
        1022,
        Input::WorkerLost {
            generation: old.worker_generation,
        },
    );
    let view = pair.source.session(LISTENER).unwrap();
    assert_eq!(view.phase, Phase::Failed);
    assert_eq!(view.reason, Some(Reason::WorkerLost));
    assert_eq!(
        pair.source
            .handle(1023, Input::ChildStopped(old), &mut Vec::new()),
        Err(Error::InvalidState),
        "late positive stop must not erase a genuine worker loss"
    );
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Failed);
}

#[test]
fn native_retirement_rejected_start_is_distinct_from_started_or_lost_worker() {
    for outcome in 0..3 {
        let mut pair = Pair::new(true, true);
        let (_, view, _) = pair.prepared();
        let commit = run(
            &mut pair.source,
            13,
            Input::Receive {
                parent: pair.sp,
                message: Control::Commit {
                    session: view.session,
                    transcript_sha256: DIGEST,
                },
            },
        );
        assert_eq!(starts(&commit), 1);
        run(&mut pair.source, 14, Input::ChildStartIssued(view));
        match outcome {
            1 => {
                run(&mut pair.source, 15, Input::ChildStarted(view));
            }
            2 => {
                run(
                    &mut pair.source,
                    15,
                    Input::WorkerLost {
                        generation: view.worker_generation,
                    },
                );
            }
            _ => {}
        }
        let rejected = Input::ChildStartRejected {
            view,
            reason: Reason::DeviceUnavailable,
        };
        let mut effects = Vec::new();
        let result = pair.source.handle(16, rejected, &mut effects);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::StopChild { .. })));
        match outcome {
            0 => {
                assert_eq!(result, Ok(()));
                let failed = pair.source.session(LISTENER).unwrap();
                assert_eq!(failed.phase, Phase::Failed);
                assert_eq!(failed.reason, Some(Reason::DeviceUnavailable));
                assert_eq!(
                    pair.source.handle(17, rejected, &mut Vec::new()),
                    Err(Error::InvalidState)
                );
                let restart = run(
                    &mut pair.source,
                    18,
                    Input::RequestStart {
                        parent: pair.sp,
                        request_id: [90; 16],
                        kind: audio_model::keys::StreamKind::System,
                        profile: Profile::Ethernet,
                    },
                );
                assert_eq!(
                    restart
                        .iter()
                        .filter(|effect| matches!(
                            effect,
                            Effect::Send {
                                message: Control::StartRequest(_),
                                ..
                            }
                        ))
                        .count(),
                    1
                );
            }
            1 => {
                assert_eq!(result, Err(Error::InvalidState));
                assert_eq!(
                    pair.source.session(LISTENER).unwrap().phase,
                    Phase::Committed
                );
                assert_eq!(
                    pair.source.handle(
                        17,
                        Input::RequestStart {
                            parent: pair.sp,
                            request_id: [90; 16],
                            kind: audio_model::keys::StreamKind::System,
                            profile: Profile::Ethernet,
                        },
                        &mut Vec::new()
                    ),
                    Err(Error::Refused(Reason::RateLimited))
                );
            }
            _ => {
                assert_eq!(result, Err(Error::InvalidState));
                let failed = pair.source.session(LISTENER).unwrap();
                assert_eq!(failed.phase, Phase::Failed);
                assert_eq!(failed.reason, Some(Reason::WorkerLost));
            }
        }
    }
}
