use audio_model::{
    jitter::Profile,
    keys::{Aead, Codec, Format},
    session::*,
    status::AudioRole,
};
const LISTENER: NodeId = [1; 32];
const SOURCE: NodeId = [2; 32];
const GROUP: [u8; 16] = [3; 16];
const CONNECTION: [u8; 16] = [4; 16];
const SESSION: SessionId = [5; 16];
const DIGEST: [u8; 32] = [6; 32];
fn format() -> Format {
    Format {
        codec: Codec::Opus,
        sample_rate: 48_000,
        channels: 2,
        frame_samples: 480,
        bitrate: 96_000,
        fec: false,
        aead: Aead::ChaCha20Poly1305,
        profile: Profile::Ethernet,
        max_datagram: 1200,
    }
}
fn parent(peer: NodeId, generation: u8) -> Parent {
    Parent {
        peer,
        connection_id: CONNECTION,
        generation: [generation; 16],
    }
}
fn run(node: &mut ControlPlane, now: u64, input: Input) -> Vec<Effect> {
    let mut effects = Vec::with_capacity(16);
    node.handle(now, input, &mut effects).unwrap();
    effects
}
fn sent(effects: &[Effect]) -> Control {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Send { message, .. } => Some(*message),
            _ => None,
        })
        .expect("a control message")
}
fn derives(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| matches!(effect, Effect::DeriveKeys { .. }))
        .count()
}
fn starts(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|effect| matches!(effect, Effect::StartChild { .. }))
        .count()
}
fn agreement(effects: &[Effect]) -> Agreement {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::DeriveKeys { agreement, .. } => Some(*agreement),
            _ => None,
        })
        .unwrap()
}
struct Pair {
    listener: ControlPlane,
    source: ControlPlane,
    lp: Parent,
    sp: Parent,
}
impl Pair {
    fn new(local_allowed: bool, remote_allowed: bool) -> Self {
        let mut listener = ControlPlane::new(LISTENER);
        let mut source = ControlPlane::new(SOURCE);
        run(
            &mut listener,
            0,
            Input::Configure(Settings {
                enabled: true,
                role: Some(AudioRole::Listener),
                group: Some(GROUP),
                ..Default::default()
            }),
        );
        run(
            &mut source,
            0,
            Input::Configure(Settings {
                enabled: true,
                role: Some(AudioRole::Source),
                group: Some(GROUP),
                listener: Some(LISTENER),
                ..Default::default()
            }),
        );
        run(
            &mut listener,
            0,
            Input::WorkerUp {
                generation: [8; 16],
                capture: false,
                render: true,
            },
        );
        run(
            &mut source,
            0,
            Input::WorkerUp {
                generation: [9; 16],
                capture: true,
                render: false,
            },
        );
        let lp = parent(SOURCE, 10);
        let sp = parent(LISTENER, 11);
        let lh = sent(&run(
            &mut listener,
            0,
            Input::PeerUp {
                parent: lp,
                support: Support {
                    control: true,
                    system_source: true,
                    listener: false,
                },
                trusted: true,
                local_allowed,
                consent_revision: 1,
            },
        ));
        let sh = sent(&run(
            &mut source,
            0,
            Input::PeerUp {
                parent: sp,
                support: Support {
                    control: true,
                    system_source: false,
                    listener: true,
                },
                trusted: true,
                local_allowed: remote_allowed,
                consent_revision: 2,
            },
        ));
        run(
            &mut source,
            0,
            Input::Receive {
                parent: sp,
                message: lh,
            },
        );
        run(
            &mut listener,
            0,
            Input::Receive {
                parent: lp,
                message: sh,
            },
        );
        Self {
            listener,
            source,
            lp,
            sp,
        }
    }
    fn offer(&mut self) -> Offer {
        let message = sent(&run(
            &mut self.listener,
            1,
            Input::Start {
                parent: self.lp,
                session: SESSION,
                listener_nonce: [12; 32],
                format: format(),
                format_sha256: [13; 32],
            },
        ));
        let Control::Offer(offer) = message else {
            panic!("offer")
        };
        offer
    }
    fn prepared(&mut self) -> (SessionView, SessionView, Agreement) {
        let offer = self.offer();
        let requested = run(
            &mut self.source,
            2,
            Input::Receive {
                parent: self.sp,
                message: Control::Offer(offer),
            },
        );
        assert_eq!(derives(&requested), 0);
        assert!(requested
            .iter()
            .any(|effect| matches!(effect, Effect::GenerateSourceNonce { .. })));
        let effects = run(
            &mut self.source,
            3,
            Input::SourceNonce {
                parent: self.sp,
                session: SESSION,
                nonce: [14; 32],
            },
        );
        let source_agreement = agreement(&effects);
        assert_eq!(derives(&effects), 1);
        assert!(effects.iter().all(|effect| !matches!(
            effect,
            Effect::Send {
                message: Control::Answer(_),
                ..
            }
        )));
        let sv = self.source.session(LISTENER).unwrap();
        assert!(run(
            &mut self.source,
            4,
            Input::KeysReady {
                view: sv,
                transcript_sha256: DIGEST
            }
        )
        .iter()
        .any(|effect| matches!(effect, Effect::PrepareChild { .. })));
        let answer = sent(&run(&mut self.source, 5, Input::ChildPrepared(sv)));
        assert_eq!(starts(&run(&mut self.source, 5, Input::Tick)), 0);
        let effects = run(
            &mut self.listener,
            6,
            Input::Receive {
                parent: self.lp,
                message: answer,
            },
        );
        let listener_agreement = agreement(&effects);
        assert_eq!(source_agreement, listener_agreement);
        assert_eq!(
            source_agreement.context().encode(),
            listener_agreement.context().encode()
        );
        assert_eq!(
            source_agreement.transcript_bytes(),
            listener_agreement.transcript_bytes()
        );
        assert_ne!(
            source_agreement.source_nonce,
            source_agreement.offer.listener_nonce
        );
        let lv = self.listener.session(SOURCE).unwrap();
        run(
            &mut self.listener,
            7,
            Input::KeysReady {
                view: lv,
                transcript_sha256: DIGEST,
            },
        );
        (lv, sv, source_agreement)
    }
    fn active(&mut self) -> (SessionView, SessionView) {
        let (lv, sv, _) = self.prepared();
        let local = run(&mut self.listener, 8, Input::ChildPrepared(lv));
        assert_eq!(starts(&local), 1, "Listener starts first");
        assert!(
            local.iter().all(|effect| !matches!(
                effect,
                Effect::Send {
                    message: Control::Commit { .. },
                    ..
                }
            )),
            "not even a Commit before Listener start acknowledgment"
        );
        let commit = sent(&run(&mut self.listener, 9, Input::ChildStarted(lv)));
        let source_start = run(
            &mut self.source,
            10,
            Input::Receive {
                parent: self.sp,
                message: commit,
            },
        );
        assert_eq!(starts(&source_start), 1);
        run(&mut self.source, 11, Input::ChildStarted(sv));
        run(&mut self.listener, 12, Input::PathReady(lv));
        run(&mut self.source, 12, Input::PathReady(sv));
        assert_eq!(self.listener.session(SOURCE).unwrap().phase, Phase::Active);
        assert_eq!(self.source.session(LISTENER).unwrap().phase, Phase::Active);
        (lv, sv)
    }
}
#[test]
fn contributory_setup_waits_for_listener_start_before_source_start() {
    let mut pair = Pair::new(true, true);
    pair.active();
}
#[test]
fn capability_or_pairing_never_grants_either_end_consent() {
    for (local_allowed, remote_allowed) in [(false, true), (true, false), (false, false)] {
        let mut pair = Pair::new(local_allowed, remote_allowed);
        let mut effects = Vec::new();
        assert_eq!(
            pair.listener.handle(
                1,
                Input::Start {
                    parent: pair.lp,
                    session: SESSION,
                    listener_nonce: [1; 32],
                    format: format(),
                    format_sha256: [2; 32]
                },
                &mut effects
            ),
            Err(Error::Refused(Reason::ConsentRequired))
        );
        assert_eq!(derives(&effects) + starts(&effects), 0);
    }
}
#[test]
fn listener_mints_and_source_resolves_without_manual_ids_or_automatic_enable() {
    let mut listener = ControlPlane::new(LISTENER);
    let effects = run(
        &mut listener,
        0,
        Input::Configure(Settings {
            enabled: true,
            role: Some(AudioRole::Listener),
            ..Default::default()
        }),
    );
    assert!(effects.contains(&Effect::MintListenerGroup));
    assert_eq!(
        listener.group_resolution(),
        GroupResolution::MintingListenerGroup
    );
    assert!(!listener.configuration().enabled);
    run(&mut listener, 0, Input::ListenerGroupMinted(GROUP));
    assert!(listener.configuration().enabled);
    assert_eq!(listener.configuration().group, Some(GROUP));
    let mut source = ControlPlane::new(SOURCE);
    run(&mut source, 0, Input::SelectListener(LISTENER));
    assert_eq!(
        source.group_resolution(),
        GroupResolution::WaitingForListener { listener: LISTENER }
    );
    let sp = parent(LISTENER, 11);
    let peer_up = |trusted| Input::PeerUp {
        parent: sp,
        support: Support {
            control: true,
            listener: true,
            system_source: false,
        },
        trusted,
        local_allowed: false,
        consent_revision: 0,
    };
    assert_eq!(
        source.handle(0, peer_up(false), &mut Vec::new()),
        Err(Error::Refused(Reason::Revoked))
    );
    run(&mut source, 0, peer_up(true));
    let other = parent([99; 32], 22);
    run(
        &mut source,
        0,
        Input::PeerUp {
            parent: other,
            support: Support {
                control: true,
                listener: true,
                system_source: false,
            },
            trusted: true,
            local_allowed: false,
            consent_revision: 0,
        },
    );
    let hello = |id, group| {
        Control::Hello(Hello {
            role: AudioRole::Listener,
            listener: id,
            group: Some(group),
            available: true,
            allowed: false,
            consent_revision: 0,
        })
    };
    run(
        &mut source,
        0,
        Input::Receive {
            parent: other,
            message: hello(other.peer, [98; 16]),
        },
    );
    assert_eq!(source.configuration().group, None);
    let effects = run(
        &mut source,
        0,
        Input::Receive {
            parent: sp,
            message: hello(LISTENER, GROUP),
        },
    );
    assert!(effects.iter().any(|effect| matches!(effect, Effect::PublishConfiguration(settings) if settings.group == Some(GROUP) && !settings.enabled)));
    assert_eq!(
        source.group_resolution(),
        GroupResolution::Resolved {
            group: GROUP,
            listener: LISTENER
        }
    );
    assert!(!source.enabled());
    run(&mut source, 0, Input::SelectListener(other.peer));
    assert_eq!(source.configuration().group, Some([98; 16]));
    assert!(!source.enabled());
}
#[test]
fn replayed_offer_never_produces_another_answer_or_derivation() {
    let mut pair = Pair::new(true, true);
    let offer = pair.offer();
    run(
        &mut pair.source,
        2,
        Input::Receive {
            parent: pair.sp,
            message: Control::Offer(offer),
        },
    );
    run(
        &mut pair.source,
        3,
        Input::SourceNonce {
            parent: pair.sp,
            session: SESSION,
            nonce: [14; 32],
        },
    );
    let mut effects = Vec::new();
    assert_eq!(
        pair.source.handle(
            4,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer)
            },
            &mut effects
        ),
        Err(Error::Refused(Reason::ProtocolViolation))
    );
    assert_eq!(derives(&effects), 0);
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::Send {
            message: Control::Answer(_),
            ..
        }
    )));
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Failed);
}
#[test]
fn parent_replacement_purges_sessions_and_rejects_stale_callbacks_and_down() {
    let mut pair = Pair::new(true, true);
    let (_, sv) = pair.active();
    let newer = Parent {
        generation: [33; 16],
        connection_id: [34; 16],
        ..pair.sp
    };
    let effects = run(
        &mut pair.source,
        20,
        Input::PeerUp {
            parent: newer,
            support: Support {
                control: true,
                listener: true,
                system_source: false,
            },
            trusted: true,
            local_allowed: true,
            consent_revision: 2,
        },
    );
    assert!(matches!(
        effects.first(),
        Some(Effect::InvalidateSession { .. })
    ));
    assert!(pair.source.session(LISTENER).is_none());
    assert_eq!(
        pair.source.handle(
            21,
            Input::KeysReady {
                view: sv,
                transcript_sha256: DIGEST
            },
            &mut Vec::new()
        ),
        Err(Error::StaleParent)
    );
    run(&mut pair.source, 22, Input::PeerDown(pair.sp));
    let hello = Control::Hello(Hello {
        role: AudioRole::Listener,
        group: Some(GROUP),
        listener: LISTENER,
        available: true,
        allowed: true,
        consent_revision: 1,
    });
    run(
        &mut pair.source,
        23,
        Input::Receive {
            parent: newer,
            message: hello,
        },
    );
    let offer = Offer {
        session: SESSION,
        listener_nonce: [12; 32],
        parent_connection_id: newer.connection_id,
        group: GROUP,
        listener: LISTENER,
        source: SOURCE,
        format: format(),
        format_sha256: [13; 32],
        listener_port: 47903,
        consent_revision: 1,
    };
    assert!(
        run(
            &mut pair.source,
            24,
            Input::Receive {
                parent: newer,
                message: Control::Offer(offer)
            }
        )
        .iter()
        .any(|effect| matches!(effect, Effect::GenerateSourceNonce { .. })),
        "fresh parent has a new replay namespace"
    );
}
#[test]
fn parent_id_mismatch_is_refused_before_nonce_or_key_work() {
    let mut pair = Pair::new(true, true);
    let mut offer = pair.offer();
    offer.parent_connection_id = [99; 16];
    let mut effects = Vec::new();
    assert_eq!(
        pair.source.handle(
            2,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer)
            },
            &mut effects
        ),
        Err(Error::Refused(Reason::ProtocolViolation))
    );
    assert!(effects.iter().all(|effect| !matches!(
        effect,
        Effect::GenerateSourceNonce { .. } | Effect::DeriveKeys { .. }
    )));
}
#[test]
fn withdrawals_and_revocation_invalidate_before_child_or_network_effects() {
    for remote in [false, true] {
        let mut pair = Pair::new(true, true);
        pair.active();
        let event = if remote {
            Input::Receive {
                parent: pair.sp,
                message: Control::Consent {
                    group: GROUP,
                    revision: 0,
                    allowed: false,
                },
            }
        } else {
            Input::LocalConsent {
                peer: LISTENER,
                allowed: false,
                revision: 0,
            }
        };
        let effects = run(&mut pair.source, 20, event);
        assert!(matches!(effects[0], Effect::InvalidateSession { .. }));
        assert!(matches!(effects[1], Effect::StopChild { .. }));
        assert_eq!(
            pair.source.session(LISTENER).unwrap().phase,
            Phase::Stopping
        );
        assert_eq!(
            pair.source.session(LISTENER).unwrap().reason,
            Some(Reason::ConsentRequired)
        );
    }
    let mut pair = Pair::new(true, true);
    pair.active();
    let effects = run(&mut pair.source, 20, Input::RevokePeer(LISTENER));
    assert!(matches!(effects[0], Effect::InvalidateSession { .. }));
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Failed);
    assert_eq!(
        pair.source.handle(
            21,
            Input::PeerUp {
                parent: pair.sp,
                support: Support {
                    control: true,
                    listener: true,
                    system_source: false
                },
                trusted: true,
                local_allowed: true,
                consent_revision: 3
            },
            &mut Vec::new()
        ),
        Err(Error::Refused(Reason::Revoked))
    );
}
#[test]
fn restarted_child_never_receives_old_keys_or_derives_again() {
    let mut pair = Pair::new(true, true);
    let (_, sv) = pair.active();
    run(
        &mut pair.source,
        20,
        Input::WorkerLost {
            generation: sv.worker_generation,
        },
    );
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Failed);
    let effects = run(
        &mut pair.source,
        21,
        Input::WorkerUp {
            generation: [44; 16],
            capture: true,
            render: false,
        },
    );
    assert_eq!(derives(&effects) + starts(&effects), 0);
    assert_eq!(
        pair.source.handle(
            22,
            Input::KeysReady {
                view: sv,
                transcript_sha256: DIGEST
            },
            &mut Vec::new()
        ),
        Err(Error::InvalidState)
    );
    assert_eq!(
        pair.source
            .handle(23, Input::ChildStarted(sv), &mut Vec::new()),
        Err(Error::InvalidState)
    );
    assert_eq!(
        pair.source.session(LISTENER).unwrap().reason,
        Some(Reason::WorkerLost)
    );
}
#[test]
fn invalid_answer_and_commit_cannot_start_source() {
    let mut pair = Pair::new(true, true);
    let offer = pair.offer();
    let bad = Answer {
        session: SESSION,
        accepted: true,
        reason: None,
        format: Some(offer.format),
        source_nonce: None,
        source_port: 47903,
        consent_revision: 2,
    };
    assert_eq!(
        pair.listener.handle(
            2,
            Input::Receive {
                parent: pair.lp,
                message: Control::Answer(bad)
            },
            &mut Vec::new()
        ),
        Err(Error::Refused(Reason::ProtocolViolation))
    );
    let mut pair = Pair::new(true, true);
    let (_, sv, _) = pair.prepared();
    let mut effects = Vec::new();
    assert_eq!(
        pair.source.handle(
            9,
            Input::Receive {
                parent: pair.sp,
                message: Control::Commit {
                    session: SESSION,
                    transcript_sha256: [99; 32]
                }
            },
            &mut effects
        ),
        Err(Error::Refused(Reason::ProtocolViolation))
    );
    assert_eq!(starts(&effects), 0);
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Failed);
    assert_eq!(
        pair.source
            .handle(10, Input::ChildStarted(sv), &mut Vec::new()),
        Err(Error::InvalidState)
    );
}
#[test]
fn health_and_keepalive_deadlines_do_not_accept_stale_renewals() {
    let mut pair = Pair::new(true, true);
    let (_, sv) = pair.active();
    for now in [250, 500, 750, 1000] {
        run(
            &mut pair.source,
            now,
            Input::WorkerHeartbeat {
                generation: sv.worker_generation,
            },
        );
    }
    run(
        &mut pair.source,
        1000,
        Input::Receive {
            parent: pair.sp,
            message: Control::Keepalive {
                session: SESSION,
                serial: 7,
                authorized_for_ms: 1000,
            },
        },
    );
    for now in [1250, 1500, 1750] {
        run(
            &mut pair.source,
            now,
            Input::WorkerHeartbeat {
                generation: sv.worker_generation,
            },
        );
    }
    run(
        &mut pair.source,
        1750,
        Input::Receive {
            parent: pair.sp,
            message: Control::Keepalive {
                session: SESSION,
                serial: 7,
                authorized_for_ms: 3000,
            },
        },
    );
    run(
        &mut pair.source,
        2000,
        Input::WorkerHeartbeat {
            generation: sv.worker_generation,
        },
    );
    assert_eq!(
        pair.source.session(LISTENER).unwrap().reason,
        Some(Reason::Timeout)
    );
    let mut pair = Pair::new(true, true);
    let (_, sv) = pair.active();
    run(&mut pair.source, 1000, Input::Tick);
    assert_eq!(
        pair.source.session(LISTENER).unwrap().reason,
        Some(Reason::WorkerLost)
    );
    assert_eq!(
        pair.source.handle(
            1001,
            Input::WorkerHeartbeat {
                generation: sv.worker_generation
            },
            &mut Vec::new()
        ),
        Err(Error::StaleWorker)
    );
}
#[test]
fn buffering_recovers_but_invalidated_devices_require_a_new_session() {
    let mut pair = Pair::new(true, true);
    let (lv, _) = pair.active();
    run(&mut pair.listener, 20, Input::Buffering(lv));
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::Rebuffering
    );
    run(&mut pair.listener, 21, Input::PathReady(lv));
    assert_eq!(pair.listener.session(SOURCE).unwrap().phase, Phase::Active);
    let effects = run(&mut pair.listener, 22, Input::DeviceInvalidated(lv));
    assert!(matches!(effects[0], Effect::InvalidateSession { .. }));
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::DevicePaused
    );
    assert_eq!(
        pair.listener
            .handle(23, Input::ChildStarted(lv), &mut Vec::new()),
        Err(Error::InvalidState)
    );
}
#[test]
fn setup_timeout_is_exact_and_clock_regression_is_rejected() {
    let mut pair = Pair::new(true, true);
    pair.offer();
    for now in (250..=9750).step_by(250) {
        run(
            &mut pair.listener,
            now,
            Input::WorkerHeartbeat {
                generation: [8; 16],
            },
        );
    }
    run(
        &mut pair.listener,
        10000,
        Input::WorkerHeartbeat {
            generation: [8; 16],
        },
    );
    assert_eq!(pair.listener.session(SOURCE).unwrap().phase, Phase::Offered);
    run(&mut pair.listener, 10001, Input::Tick);
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().reason,
        Some(Reason::Timeout)
    );
    assert_eq!(
        pair.listener.handle(10000, Input::Tick, &mut Vec::new()),
        Err(Error::TimeReversed)
    );
}
#[test]
fn replay_storage_saturates_without_evicting_old_session_ids() {
    let mut pair = Pair::new(true, true);
    let mut offer = pair.offer();
    let mut clock = 1;
    for number in 0..USED_SESSION_LIMIT {
        let at = number as u64 * 1000 + 2;
        while clock + 250 < at {
            clock += 250;
            run(
                &mut pair.source,
                clock,
                Input::WorkerHeartbeat {
                    generation: [9; 16],
                },
            );
        }
        offer.session = [0; 16];
        offer.session[..2].copy_from_slice(&(number as u16).to_be_bytes());
        run(
            &mut pair.source,
            at,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer),
            },
        );
        run(
            &mut pair.source,
            at,
            Input::Stop {
                peer: LISTENER,
                reason: Reason::UserStop,
            },
        );
    }
    let at = USED_SESSION_LIMIT as u64 * 1000 + 2;
    while clock + 250 < at {
        clock += 250;
        run(
            &mut pair.source,
            clock,
            Input::WorkerHeartbeat {
                generation: [9; 16],
            },
        );
    }
    offer.session = [99; 16];
    assert_eq!(
        pair.source.handle(
            at,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer)
            },
            &mut Vec::new()
        ),
        Err(Error::Refused(Reason::RateLimited))
    );
    offer.session = [0; 16];
    assert_eq!(
        pair.source.handle(
            at,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer)
            },
            &mut Vec::new()
        ),
        Err(Error::Refused(Reason::ProtocolViolation))
    );
}

#[test]
fn cached_listener_hello_resolves_selection_but_revoked_hello_cannot() {
    for revoke in [false, true] {
        let mut source = ControlPlane::new(SOURCE);
        let sp = parent(LISTENER, 11);
        run(
            &mut source,
            0,
            Input::PeerUp {
                parent: sp,
                support: Support {
                    control: true,
                    listener: true,
                    system_source: false,
                },
                trusted: true,
                local_allowed: false,
                consent_revision: 0,
            },
        );
        let hello = Control::Hello(Hello {
            role: AudioRole::Listener,
            group: Some(GROUP),
            listener: LISTENER,
            available: true,
            allowed: false,
            consent_revision: 1,
        });
        run(
            &mut source,
            1,
            Input::Receive {
                parent: sp,
                message: hello,
            },
        );
        if revoke {
            run(&mut source, 2, Input::RevokePeer(LISTENER));
        }
        run(&mut source, 3, Input::SelectListener(LISTENER));
        if revoke {
            assert_eq!(
                source.group_resolution(),
                GroupResolution::WaitingForListener { listener: LISTENER }
            );
            let mut effects = Vec::new();
            assert_eq!(
                source.handle(
                    4,
                    Input::Receive {
                        parent: sp,
                        message: hello
                    },
                    &mut effects
                ),
                Err(Error::Refused(Reason::Revoked))
            );
            assert!(effects.is_empty());
        } else {
            assert_eq!(
                source.group_resolution(),
                GroupResolution::Resolved {
                    group: GROUP,
                    listener: LISTENER
                }
            );
        }
        assert!(!source.enabled());
    }
}

#[test]
fn keepalives_cannot_extend_path_validation_past_original_setup_deadline() {
    let mut pair = Pair::new(true, true);
    let (lv, _, _) = pair.prepared();
    run(&mut pair.listener, 8, Input::ChildPrepared(lv));
    run(&mut pair.listener, 9, Input::ChildStarted(lv));
    for now in (250..=10000).step_by(250) {
        run(
            &mut pair.listener,
            now,
            Input::WorkerHeartbeat {
                generation: lv.worker_generation,
            },
        );
        if now % 1000 == 0 {
            run(
                &mut pair.listener,
                now,
                Input::Receive {
                    parent: pair.lp,
                    message: Control::Keepalive {
                        session: SESSION,
                        serial: now / 1000,
                        authorized_for_ms: 3000,
                    },
                },
            );
        }
    }
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::Committed
    );
    run(&mut pair.listener, 10001, Input::Tick);
    assert_eq!(pair.listener.session(SOURCE).unwrap().phase, Phase::Failed);
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().reason,
        Some(Reason::Timeout)
    );
}

#[test]
fn stop_guard_starts_at_stop_and_preserves_the_original_reason() {
    let mut pair = Pair::new(true, true);
    let (_, sv, _) = pair.prepared();
    for now in (250..=5000).step_by(250) {
        run(
            &mut pair.source,
            now,
            Input::WorkerHeartbeat {
                generation: sv.worker_generation,
            },
        );
    }
    run(
        &mut pair.source,
        5000,
        Input::Stop {
            peer: LISTENER,
            reason: Reason::UserStop,
        },
    );
    run(&mut pair.source, 5001, Input::Tick);
    assert_eq!(
        pair.source.session(LISTENER).unwrap().phase,
        Phase::Stopping
    );
    assert_eq!(
        pair.source.session(LISTENER).unwrap().reason,
        Some(Reason::UserStop)
    );
    for now in [5250, 5500, 5750, 6000] {
        run(
            &mut pair.source,
            now,
            Input::WorkerHeartbeat {
                generation: sv.worker_generation,
            },
        );
    }
    assert_eq!(pair.source.session(LISTENER).unwrap().phase, Phase::Stopped);
    assert_eq!(
        pair.source.session(LISTENER).unwrap().reason,
        Some(Reason::UserStop)
    );
    run(&mut pair.source, 6000, Input::ChildStopped(sv));
}

#[test]
fn a_new_consent_grant_requires_a_new_revision_and_never_starts_implicitly() {
    let mut pair = Pair::new(false, true);
    assert_eq!(
        pair.listener.handle(
            1,
            Input::LocalConsent {
                peer: SOURCE,
                allowed: true,
                revision: 1
            },
            &mut Vec::new()
        ),
        Err(Error::InvalidState)
    );
    let effects = run(
        &mut pair.listener,
        2,
        Input::LocalConsent {
            peer: SOURCE,
            allowed: true,
            revision: 2,
        },
    );
    assert_eq!(starts(&effects) + derives(&effects), 0);
    run(
        &mut pair.source,
        3,
        Input::Receive {
            parent: pair.sp,
            message: sent(&effects),
        },
    );
    run(
        &mut pair.source,
        4,
        Input::Receive {
            parent: pair.sp,
            message: Control::Consent {
                group: GROUP,
                revision: 2,
                allowed: false,
            },
        },
    );
    run(
        &mut pair.source,
        5,
        Input::Receive {
            parent: pair.sp,
            message: Control::Hello(Hello {
                role: AudioRole::Listener,
                group: Some(GROUP),
                listener: LISTENER,
                available: true,
                allowed: true,
                consent_revision: 1,
            }),
        },
    );
    let offer = Offer {
        session: SESSION,
        listener_nonce: [12; 32],
        parent_connection_id: CONNECTION,
        group: GROUP,
        listener: LISTENER,
        source: SOURCE,
        format: format(),
        format_sha256: [13; 32],
        listener_port: 47903,
        consent_revision: 1,
    };
    let mut effects = Vec::new();
    assert_eq!(
        pair.source.handle(
            6,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer)
            },
            &mut effects
        ),
        Err(Error::Refused(Reason::ConsentRequired))
    );
    assert_eq!(derives(&effects) + starts(&effects), 0);
}

#[test]
fn late_control_support_announces_without_granting_consent_or_starting() {
    let mut source = ControlPlane::new(SOURCE);
    run(&mut source, 0, Input::SelectListener(LISTENER));
    let sp = parent(LISTENER, 11);
    let effects = run(
        &mut source,
        0,
        Input::PeerUp {
            parent: sp,
            support: Support::default(),
            trusted: true,
            local_allowed: false,
            consent_revision: 0,
        },
    );
    assert!(effects
        .iter()
        .all(|effect| !matches!(effect, Effect::Send { .. })));
    let effects = run(
        &mut source,
        1,
        Input::PeerSupport {
            parent: sp,
            support: Support {
                control: true,
                listener: true,
                system_source: false,
            },
        },
    );
    assert!(matches!(
        sent(&effects),
        Control::Hello(Hello {
            allowed: false,
            group: None,
            ..
        })
    ));
    assert_eq!(derives(&effects) + starts(&effects), 0);
}

#[test]
fn listener_availability_tracks_worker_readiness_not_only_configuration() {
    let mut listener = ControlPlane::new(LISTENER);
    run(
        &mut listener,
        0,
        Input::Configure(Settings {
            enabled: true,
            role: Some(AudioRole::Listener),
            group: Some(GROUP),
            ..Default::default()
        }),
    );
    let effects = run(
        &mut listener,
        0,
        Input::PeerUp {
            parent: parent(SOURCE, 10),
            support: Support {
                control: true,
                system_source: true,
                listener: false,
            },
            trusted: true,
            local_allowed: true,
            consent_revision: 1,
        },
    );
    assert!(matches!(
        sent(&effects),
        Control::Hello(Hello {
            available: false,
            ..
        })
    ));
    let effects = run(
        &mut listener,
        1,
        Input::WorkerUp {
            generation: [8; 16],
            capture: false,
            render: true,
        },
    );
    assert!(matches!(
        sent(&effects),
        Control::Hello(Hello {
            available: true,
            ..
        })
    ));
    let effects = run(
        &mut listener,
        2,
        Input::WorkerLost {
            generation: [8; 16],
        },
    );
    assert!(matches!(
        sent(&effects),
        Control::Hello(Hello {
            available: false,
            ..
        })
    ));
    assert_eq!(derives(&effects) + starts(&effects), 0);
}
