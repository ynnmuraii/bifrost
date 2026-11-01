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
        self.prepared_offer(offer)
    }
    fn prepared_offer(&mut self, offer: Offer) -> (SessionView, SessionView, Agreement) {
        let requested = run(
            &mut self.source,
            2,
            Input::Receive {
                parent: self.sp,
                message: Control::Offer(offer),
            },
        );
        if let Some(request_id) = offer.request_id {
            assert!(requested.contains(&Effect::StartRequestResult {
                parent: self.sp,
                request_id,
                result: Ok(offer.session),
            }));
        }
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
fn offers_require_the_current_remote_consent_revision_before_nonce_or_key_work() {
    for revision in [0, 1, 2] {
        let mut pair = Pair::new(true, true);
        let mut offer = pair.offer();
        offer.consent_revision = revision;
        let mut effects = Vec::new();
        let result = pair.source.handle(
            2,
            Input::Receive {
                parent: pair.sp,
                message: Control::Offer(offer),
            },
            &mut effects,
        );
        assert_eq!(derives(&effects), 0);
        assert_eq!(starts(&effects), 0);
        if revision == 1 {
            assert_eq!(result, Ok(()));
            assert!(effects.contains(&Effect::GenerateSourceNonce {
                parent: pair.sp,
                session: offer.session,
            }));
            let effects = run(
                &mut pair.source,
                3,
                Input::SourceNonce {
                    parent: pair.sp,
                    session: offer.session,
                    nonce: [14; 32],
                },
            );
            assert_eq!(derives(&effects), 1);
            assert_eq!(agreement(&effects).offer.consent_revision, revision);
        } else {
            assert_eq!(result, Err(Error::Refused(Reason::ConsentRequired)));
            assert_eq!(
                sent(&effects),
                Control::Answer(Answer {
                    session: offer.session,
                    accepted: false,
                    reason: Some(Reason::ConsentRequired),
                    format: None,
                    source_nonce: None,
                    source_port: 0,
                    consent_revision: 2,
                })
            );
            assert!(!effects
                .iter()
                .any(|effect| matches!(effect, Effect::GenerateSourceNonce { .. })));
            assert!(pair.source.session(LISTENER).is_none());
        }
    }
}

#[test]
fn accepted_answers_require_the_current_remote_consent_revision_before_deriving() {
    for revision in [1, 2, 3] {
        let mut pair = Pair::new(true, true);
        let offer = pair.offer();
        let before = pair.listener.session(SOURCE);
        let mut effects = Vec::new();
        let result = pair.listener.handle(
            2,
            Input::Receive {
                parent: pair.lp,
                message: Control::Answer(Answer {
                    session: offer.session,
                    accepted: true,
                    reason: None,
                    format: Some(offer.format),
                    source_nonce: Some([14; 32]),
                    source_port: 47903,
                    consent_revision: revision,
                }),
            },
            &mut effects,
        );
        assert_eq!(starts(&effects), 0);
        if revision == 2 {
            assert_eq!(result, Ok(()));
            assert_eq!(derives(&effects), 1);
            assert_eq!(agreement(&effects).source_consent_revision, revision);
            assert_eq!(
                pair.listener.session(SOURCE).unwrap().phase,
                Phase::Answered
            );
        } else {
            assert_eq!(result, Err(Error::Refused(Reason::ConsentRequired)));
            assert_eq!(derives(&effects), 0);
            assert!(effects.is_empty());
            assert_eq!(pair.listener.session(SOURCE), before);
        }
    }
}

#[test]
fn refused_answers_do_not_require_the_current_remote_consent_revision() {
    let mut pair = Pair::new(true, true);
    let offer = pair.offer();
    let effects = run(
        &mut pair.listener,
        2,
        Input::Receive {
            parent: pair.lp,
            message: Control::Answer(Answer {
                session: offer.session,
                accepted: false,
                reason: Some(Reason::ConsentRequired),
                format: None,
                source_nonce: None,
                source_port: 0,
                consent_revision: 1,
            }),
        },
    );
    assert_eq!(derives(&effects), 0);
    assert_eq!(starts(&effects), 0);
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().reason,
        Some(Reason::ConsentRequired)
    );
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
fn automatic_source_follows_fresh_selected_listener_but_override_stays_pinned() {
    for pinned in [false, true] {
        let mut pair = Pair::new(true, true);
        if pinned {
            let settings = Settings {
                group_override: true,
                ..pair.source.configuration()
            };
            run(&mut pair.source, 0, Input::Configure(settings));
        }
        let hello = |group, revision| {
            Control::Hello(Hello {
                role: AudioRole::Listener,
                group: Some(group),
                listener: LISTENER,
                available: true,
                allowed: true,
                consent_revision: revision,
            })
        };
        let next = [44; 16];
        let effects = run(
            &mut pair.source,
            1,
            Input::Receive {
                parent: pair.sp,
                message: hello(next, 2),
            },
        );
        let expected = if pinned { GROUP } else { next };
        assert_eq!(pair.source.configuration().group, Some(expected));
        assert_eq!(
            effects.iter().any(|effect| matches!(
                effect, Effect::PublishConfiguration(settings) if settings.group == Some(next)
            )),
            !pinned
        );
        for (parent, revision) in [(pair.sp, 1), (parent(LISTENER, 99), 3)] {
            run(
                &mut pair.source,
                2,
                Input::Receive {
                    parent,
                    message: hello(GROUP, revision),
                },
            );
            assert_eq!(pair.source.configuration().group, Some(expected));
        }
    }
}

#[test]
fn pinning_or_unpinning_current_group_preserves_active_transport() {
    let mut pair = Pair::new(true, true);
    pair.active();
    let before = pair.source.session(LISTENER).unwrap();
    let listener_before = pair.listener.session(SOURCE).unwrap();
    for (now, pinned) in [(20, true), (21, false), (22, true)] {
        let settings = Settings {
            group_override: pinned,
            ..pair.source.configuration()
        };
        let effects = run(&mut pair.source, now, Input::Configure(settings));
        assert_eq!(pair.source.configuration().group_override, pinned);
        assert_eq!(pair.source.session(LISTENER), Some(before));
        assert_eq!(pair.listener.session(SOURCE), Some(listener_before));
        assert!(effects.iter().all(|effect| !matches!(
            effect,
            Effect::InvalidateSession { .. }
                | Effect::StopChild { .. }
                | Effect::DeriveKeys { .. }
                | Effect::Send {
                    message: Control::Stop { .. },
                    ..
                }
        )));
    }
    // Pinning still governs future discovery; it does not follow a new group.
    run(
        &mut pair.source,
        23,
        Input::Receive {
            parent: pair.sp,
            message: Control::Hello(Hello {
                role: AudioRole::Listener,
                group: Some([44; 16]),
                listener: LISTENER,
                available: true,
                allowed: true,
                consent_revision: 2,
            }),
        },
    );
    assert_eq!(pair.source.configuration().group, Some(GROUP));
    let settings = Settings {
        group_override: false,
        ..pair.source.configuration()
    };
    run(&mut pair.source, 24, Input::Configure(settings));
    assert_eq!(pair.source.configuration().group, Some([44; 16]));
}

#[test]
fn source_enable_waits_for_group_without_losing_intent_and_rejects_empty_override() {
    let mut source = ControlPlane::new(SOURCE);
    let settings = Settings {
        enabled: true,
        role: Some(AudioRole::Source),
        listener: Some(LISTENER),
        ..Default::default()
    };
    assert_eq!(
        source.handle(
            0,
            Input::Configure(Settings {
                group_override: true,
                ..settings
            }),
            &mut Vec::new()
        ),
        Err(Error::InvalidSettings)
    );
    run(&mut source, 0, Input::Configure(settings));
    assert!(!source.enabled());
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
            local_allowed: true,
            consent_revision: 1,
        },
    );
    run(
        &mut source,
        1,
        Input::Receive {
            parent: sp,
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
    assert!(source.enabled());
    assert!(source.configuration().enabled);
}

#[test]
fn fresh_group_change_stops_the_previous_group_session() {
    let mut pair = Pair::new(true, true);
    pair.active();
    let effects = run(
        &mut pair.source,
        20,
        Input::Receive {
            parent: pair.sp,
            message: Control::Hello(Hello {
                role: AudioRole::Listener,
                group: Some([44; 16]),
                listener: LISTENER,
                available: true,
                allowed: true,
                consent_revision: 2,
            }),
        },
    );
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::StopChild { .. })));
    assert_eq!(pair.source.configuration().group, Some([44; 16]));
    assert!(effects.iter().any(|effect| matches!(
        effect, Effect::PublishConfiguration(settings) if settings.group == Some([44; 16])
    )));
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
        request_id: None,
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
fn prepared_sessions_stop_logically_without_inventing_native_completion() {
    let mut pair = Pair::new(true, true);
    pair.prepared();
    for (node, peer) in [(&mut pair.listener, SOURCE), (&mut pair.source, LISTENER)] {
        let effects = run(
            node,
            20,
            Input::Stop {
                peer,
                reason: Reason::UserStop,
            },
        );
        let stopped = node.session(peer).unwrap();
        assert_eq!(stopped.phase, Phase::Stopped);
        assert_eq!(stopped.reason, Some(Reason::UserStop));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::StopChild { .. })));
        assert_eq!(starts(&effects), 0);
    }
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
        Input::Receive {
            parent: pair.sp,
            message: Control::Commit {
                session: sv.session,
                transcript_sha256: DIGEST,
            },
        },
    );
    run(&mut pair.source, 5000, Input::ChildStartIssued(sv));
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
fn source_capability_withdrawal_completes_matching_remote_user_stop() {
    let mut pair = Pair::new(true, true);
    let (view, _) = pair.active();
    run(
        &mut pair.listener,
        20,
        Input::Receive {
            parent: pair.lp,
            message: Control::Stop {
                session: view.session,
                reason: Reason::UserStop,
            },
        },
    );
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::Stopping
    );
    let effects = run(
        &mut pair.listener,
        21,
        Input::PeerSupport {
            parent: pair.lp,
            support: Support {
                control: true,
                system_source: false,
                listener: false,
            },
        },
    );
    let stopping = pair.listener.session(SOURCE).unwrap();
    assert_eq!(stopping.phase, Phase::Stopping);
    assert_eq!(stopping.reason, Some(Reason::UserStop));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::InvalidateSession { .. } | Effect::StopChild { .. }
    )));
    run(&mut pair.listener, 22, Input::ChildStopped(view));
    let stopped = pair.listener.session(SOURCE).unwrap();
    assert_eq!(stopped.phase, Phase::Stopped);
    assert_eq!(stopped.reason, Some(Reason::UserStop));
}

#[test]
fn pending_user_stop_does_not_mask_real_revocation_or_worker_loss() {
    for failure in 0..8 {
        let mut pair = Pair::new(true, true);
        let (view, _) = pair.active();
        let stop = if failure < 4 {
            Input::Receive {
                parent: pair.lp,
                message: Control::Stop {
                    session: view.session,
                    reason: Reason::UserStop,
                },
            }
        } else {
            Input::Stop {
                peer: SOURCE,
                reason: Reason::UserStop,
            }
        };
        run(&mut pair.listener, 20, stop);
        let (input, reason) = match failure % 4 {
            0 => (Input::RevokePeer(SOURCE), Reason::Revoked),
            1 => (
                Input::ChildFailed {
                    view,
                    reason: Reason::ProtocolViolation,
                },
                Reason::ProtocolViolation,
            ),
            2 => (
                Input::PeerSupport {
                    parent: pair.lp,
                    support: Support {
                        control: false,
                        system_source: false,
                        listener: false,
                    },
                },
                Reason::Revoked,
            ),
            _ => (
                Input::WorkerLost {
                    generation: view.worker_generation,
                },
                Reason::WorkerLost,
            ),
        };
        run(&mut pair.listener, 21, input);
        let failed = pair.listener.session(SOURCE).unwrap();
        assert_eq!(failed.phase, Phase::Failed, "failure case {failure}");
        assert_eq!(failed.reason, Some(reason), "failure case {failure}");
        assert_eq!(
            pair.listener
                .handle(22, Input::ChildStopped(view), &mut Vec::new()),
            Err(Error::InvalidState),
            "a late positive stop must not erase genuine failure"
        );
    }
}

#[test]
fn source_capability_loss_without_matching_user_stop_remains_failure() {
    for stopping in 0..2 {
        let mut pair = Pair::new(true, true);
        let (view, _) = pair.active();
        if stopping == 1 {
            run(
                &mut pair.listener,
                20,
                Input::Receive {
                    parent: pair.lp,
                    message: Control::Stop {
                        session: [99; 16],
                        reason: Reason::UserStop,
                    },
                },
            );
        }
        run(
            &mut pair.listener,
            21,
            Input::PeerSupport {
                parent: pair.lp,
                support: Support {
                    control: true,
                    system_source: false,
                    listener: false,
                },
            },
        );
        let failed = pair.listener.session(SOURCE).unwrap();
        assert_eq!(failed.session, view.session);
        assert_eq!(failed.phase, Phase::Failed, "stop case {stopping}");
        assert_eq!(failed.reason, Some(Reason::Revoked));
    }
}

#[test]
fn expected_media_capability_withdrawal_preserves_local_and_remote_user_stop() {
    for source in [false, true] {
        for remote in [false, true] {
            let mut pair = Pair::new(true, true);
            let (lv, sv) = pair.active();
            let (node, peer, parent, view) = if source {
                (&mut pair.source, LISTENER, pair.sp, sv)
            } else {
                (&mut pair.listener, SOURCE, pair.lp, lv)
            };
            let stop = if remote {
                Input::Receive {
                    parent,
                    message: Control::Stop {
                        session: view.session,
                        reason: Reason::UserStop,
                    },
                }
            } else {
                Input::Stop {
                    peer,
                    reason: Reason::UserStop,
                }
            };
            run(node, 20, stop);
            run(
                node,
                21,
                Input::PeerSupport {
                    parent,
                    support: Support {
                        control: true,
                        system_source: false,
                        listener: false,
                    },
                },
            );
            let stopping = node.session(peer).unwrap();
            assert_eq!(
                stopping.phase,
                Phase::Stopping,
                "source={source}, remote_stop={remote}"
            );
            assert_eq!(stopping.reason, Some(Reason::UserStop));
            run(node, 22, Input::ChildStopped(view));
            assert_eq!(node.session(peer).unwrap().phase, Phase::Stopped);
        }
    }
}

#[test]
fn retired_session_stop_and_old_generation_cannot_settle_a_new_session() {
    let mut pair = Pair::new(true, true);
    let (old, _) = pair.active();
    run(
        &mut pair.listener,
        20,
        Input::Receive {
            parent: pair.lp,
            message: Control::Stop {
                session: old.session,
                reason: Reason::UserStop,
            },
        },
    );
    run(&mut pair.listener, 21, Input::ChildStopped(old));
    for now in [250, 500, 750, 1000] {
        run(
            &mut pair.listener,
            now,
            Input::WorkerHeartbeat {
                generation: old.worker_generation,
            },
        );
    }
    run(
        &mut pair.listener,
        1001,
        Input::Start {
            parent: pair.lp,
            session: [77; 16],
            listener_nonce: [78; 32],
            format: format(),
            format_sha256: [13; 32],
        },
    );
    let current = pair.listener.session(SOURCE).unwrap();
    assert_ne!(current.session, old.session);
    run(
        &mut pair.listener,
        1002,
        Input::Receive {
            parent: pair.lp,
            message: Control::Stop {
                session: old.session,
                reason: Reason::UserStop,
            },
        },
    );
    for view in [
        old,
        SessionView {
            worker_generation: [99; 16],
            ..current
        },
    ] {
        for input in [Input::ChildStopped(view), Input::ChildStartIssued(view)] {
            assert_eq!(
                pair.listener.handle(1003, input, &mut Vec::new()),
                Err(Error::StaleWorker)
            );
        }
    }
    run(
        &mut pair.listener,
        1004,
        Input::WorkerLost {
            generation: [99; 16],
        },
    );
    assert_eq!(pair.listener.session(SOURCE), Some(current));
    run(
        &mut pair.listener,
        1005,
        Input::PeerSupport {
            parent: pair.lp,
            support: Support {
                control: true,
                system_source: false,
                listener: false,
            },
        },
    );
    assert_eq!(pair.listener.session(SOURCE).unwrap().phase, Phase::Failed);
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().reason,
        Some(Reason::Revoked),
        "retired remote UserStop must not exempt a new session"
    );
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
        request_id: None,
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

fn request(id: u8) -> StartRequest {
    StartRequest {
        request_id: [id; 16],
        group: GROUP,
        source: SOURCE,
        consent_revision: 2,
        kind: audio_model::keys::StreamKind::System,
        profile: Profile::Ethernet,
    }
}

#[test]
fn source_request_enters_ordinary_contributory_handshake() {
    let mut pair = Pair::new(true, true);
    let request = sent(&run(
        &mut pair.source,
        0,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [42; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    ));
    let effects = run(
        &mut pair.listener,
        0,
        Input::Receive {
            parent: pair.lp,
            message: request,
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::GenerateListenerOffer { parent, request }]
        if *parent == pair.lp && request.request_id == [42; 16])
    );
    let offer = pair.offer();
    assert_eq!(offer.request_id, Some([42; 16]));
    let (lv, sv, agreement) = pair.prepared_offer(offer);
    // Correlation never changes key derivation or the semantic agreement.
    let mut ordinary = agreement;
    ordinary.offer.request_id = None;
    assert_eq!(agreement.context().encode(), ordinary.context().encode());
    assert_eq!(agreement.transcript_bytes(), ordinary.transcript_bytes());
    let local = run(&mut pair.listener, 8, Input::ChildPrepared(lv));
    assert_eq!(starts(&local), 1);
    assert!(!local.iter().any(|effect| matches!(
        effect,
        Effect::Send {
            message: Control::Commit { .. },
            ..
        }
    )));
    let commit = sent(&run(&mut pair.listener, 9, Input::ChildStarted(lv)));
    assert_eq!(
        starts(&run(
            &mut pair.source,
            10,
            Input::Receive {
                parent: pair.sp,
                message: commit,
            }
        )),
        1
    );
    run(&mut pair.source, 11, Input::ChildStarted(sv));
}

#[test]
fn listener_request_refusals_are_typed_and_do_not_stop_sessions() {
    for reason in [
        Reason::ConsentRequired,
        Reason::RateLimited,
        Reason::GroupMismatch,
    ] {
        let mut pair = Pair::new(reason != Reason::ConsentRequired, true);
        let mut requested = request(43);
        if reason == Reason::RateLimited {
            pair.offer();
        }
        if reason == Reason::GroupMismatch {
            requested.group = [99; 16];
        }
        let before = pair.listener.session(SOURCE);
        let effects = run(
            &mut pair.listener,
            2,
            Input::Receive {
                parent: pair.lp,
                message: Control::StartRequest(requested),
            },
        );
        assert_eq!(
            sent(&effects),
            Control::StartRefusal {
                request_id: [43; 16],
                reason
            }
        );
        assert_eq!(pair.listener.session(SOURCE), before);
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::StopChild { .. })));
    }
}

#[test]
fn requests_are_rate_limited_deduplicated_and_queue_one() {
    let mut pair = Pair::new(true, true);
    let first = run(
        &mut pair.listener,
        1,
        Input::Receive {
            parent: pair.lp,
            message: Control::StartRequest(request(44)),
        },
    );
    assert!(matches!(
        first.as_slice(),
        [Effect::GenerateListenerOffer { .. }]
    ));
    for id in [44, 45] {
        assert!(run(
            &mut pair.listener,
            2,
            Input::Receive {
                parent: pair.lp,
                message: Control::StartRequest(request(id)),
            }
        )
        .is_empty());
    }
    run(
        &mut pair.listener,
        3,
        Input::Stop {
            peer: SOURCE,
            reason: Reason::UserStop,
        },
    );
    assert!(run(
        &mut pair.listener,
        4,
        Input::Receive {
            parent: pair.lp,
            message: Control::StartRequest(request(46)),
        }
    )
    .is_empty());
    for at in [250, 500, 750, 1000] {
        run(
            &mut pair.listener,
            at,
            Input::WorkerHeartbeat {
                generation: [8; 16],
            },
        );
    }
    assert!(run(
        &mut pair.listener,
        1001,
        Input::Receive {
            parent: pair.lp,
            message: Control::StartRequest(request(44)),
        }
    )
    .is_empty());
    assert!(matches!(
        run(
            &mut pair.listener,
            1002,
            Input::Receive {
                parent: pair.lp,
                message: Control::StartRequest(request(47)),
            }
        )
        .as_slice(),
        [Effect::GenerateListenerOffer { .. }]
    ));
}

#[test]
fn refusal_correlates_only_pending_request_not_a_session() {
    let mut pair = Pair::new(true, true);
    pair.active();
    let before = pair.source.session(LISTENER);
    assert!(run(
        &mut pair.source,
        13,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: SESSION,
                reason: Reason::UserStop
            },
        }
    )
    .is_empty());
    assert_eq!(pair.source.session(LISTENER), before);
    let mut pair = Pair::new(true, true);
    run(
        &mut pair.source,
        1,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [48; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    let effects = run(
        &mut pair.source,
        2,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: [48; 16],
                reason: Reason::ConsentRequired,
            },
        },
    );
    assert_eq!(
        effects,
        vec![Effect::StartRequestResult {
            parent: pair.sp,
            request_id: [48; 16],
            result: Err(Reason::ConsentRequired),
        }]
    );
}

#[test]
fn replaced_parent_cannot_correlate_or_expire_new_request() {
    let mut pair = Pair::new(true, true);
    run(
        &mut pair.source,
        1,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [49; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    let newer = parent(LISTENER, 55);
    let replacement = run(
        &mut pair.source,
        2,
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
    assert!(replacement.contains(&Effect::StartRequestResult {
        parent: pair.sp,
        request_id: [49; 16],
        result: Err(Reason::ParentLost),
    }));
    run(
        &mut pair.source,
        3,
        Input::Receive {
            parent: newer,
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
    run(
        &mut pair.source,
        4,
        Input::RequestStart {
            parent: newer,
            request_id: [49; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    assert!(run(
        &mut pair.source,
        100_000,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: [49; 16],
                reason: Reason::Disabled
            },
        }
    )
    .is_empty());
    let result = run(
        &mut pair.source,
        5,
        Input::Receive {
            parent: newer,
            message: Control::StartRefusal {
                request_id: [49; 16],
                reason: Reason::ConsentRequired,
            },
        },
    );
    assert_eq!(
        result,
        vec![Effect::StartRequestResult {
            parent: newer,
            request_id: [49; 16],
            result: Err(Reason::ConsentRequired),
        }]
    );
}

#[test]
fn correlated_offer_cannot_match_a_different_pending_request() {
    let mut pair = Pair::new(true, true);
    let mut offer = pair.offer();
    offer.request_id = Some([50; 16]);
    run(
        &mut pair.source,
        1,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [51; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    assert!(run(
        &mut pair.source,
        2,
        Input::Receive {
            parent: pair.sp,
            message: Control::Offer(offer),
        }
    )
    .is_empty());
    assert!(pair.source.session(LISTENER).is_none());
    let refusal = run(
        &mut pair.source,
        3,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: [51; 16],
                reason: Reason::Disabled,
            },
        },
    );
    assert_eq!(
        refusal,
        vec![Effect::StartRequestResult {
            parent: pair.sp,
            request_id: [51; 16],
            result: Err(Reason::Disabled),
        }]
    );
}

#[test]
fn request_replay_storage_saturates_without_eviction() {
    let mut pair = Pair::new(false, true);
    for index in 0..USED_REQUEST_LIMIT {
        let mut value = request(0);
        value.request_id[..2].copy_from_slice(&(index as u16).to_be_bytes());
        let effects = run(
            &mut pair.listener,
            index as u64 * 1000,
            Input::Receive {
                parent: pair.lp,
                message: Control::StartRequest(value),
            },
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Send {
                message: Control::StartRefusal {
                    reason: Reason::ConsentRequired,
                    ..
                },
                ..
            }
        )));
    }
    let at = USED_REQUEST_LIMIT as u64 * 1000;
    for value in [request(0), request(99)] {
        assert!(run(
            &mut pair.listener,
            at,
            Input::Receive {
                parent: pair.lp,
                message: Control::StartRequest(value),
            }
        )
        .is_empty());
    }
}

#[test]
fn pending_source_request_times_out_without_stopping_any_session() {
    let mut pair = Pair::new(true, true);
    run(
        &mut pair.source,
        1,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [52; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    for at in (250..=10000).step_by(250) {
        run(
            &mut pair.source,
            at,
            Input::WorkerHeartbeat {
                generation: [9; 16],
            },
        );
    }
    let expired = run(&mut pair.source, 10001, Input::Tick);
    assert_eq!(
        expired,
        vec![Effect::StartRequestResult {
            parent: pair.sp,
            request_id: [52; 16],
            result: Err(Reason::Timeout),
        }]
    );
    assert!(pair.source.session(LISTENER).is_none());
}

#[test]
fn four_parents_receive_typed_full_refusals_for_three_listener_slots() {
    let mut pair = Pair::new(true, true);
    for (index, source) in [SOURCE, [61; 32], [62; 32], [63; 32]]
        .into_iter()
        .enumerate()
    {
        let link = if source == SOURCE {
            pair.lp
        } else {
            parent(source, 60 + index as u8)
        };
        run(
            &mut pair.listener,
            0,
            Input::PeerUp {
                parent: link,
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
        run(
            &mut pair.listener,
            0,
            Input::Receive {
                parent: link,
                message: Control::Hello(Hello {
                    role: AudioRole::Source,
                    group: Some(GROUP),
                    listener: LISTENER,
                    available: false,
                    allowed: true,
                    consent_revision: 2,
                }),
            },
        );
        let mut requested = request(60 + index as u8);
        requested.source = source;
        let effects = run(
            &mut pair.listener,
            0,
            Input::Receive {
                parent: link,
                message: Control::StartRequest(requested),
            },
        );
        if index < 3 {
            assert!(matches!(
                effects.as_slice(),
                [Effect::GenerateListenerOffer { .. }]
            ));
            assert!(
                pair.listener.session(source).is_none(),
                "reservation precedes randomness"
            );
        } else {
            assert_eq!(
                sent(&effects),
                Control::StartRefusal {
                    request_id: requested.request_id,
                    reason: Reason::RateLimited,
                }
            );
        }
    }
    assert_eq!(
        pair.listener.handle(
            0,
            Input::PeerUp {
                parent: parent([64; 32], 64),
                support: Support {
                    control: true,
                    system_source: true,
                    listener: false
                },
                trusted: true,
                local_allowed: true,
                consent_revision: 1,
            },
            &mut Vec::new()
        ),
        Err(Error::Capacity)
    );
    // Converting an existing reservation must not count the same slot twice.
    pair.offer();
    let mut effects = Vec::new();
    assert_eq!(
        pair.listener.handle(
            1,
            Input::Start {
                parent: parent([63; 32], 63),
                session: [65; 16],
                listener_nonce: [66; 32],
                format: format(),
                format_sha256: DIGEST,
            },
            &mut effects
        ),
        Err(Error::Refused(Reason::RateLimited))
    );
    assert!(effects.is_empty());
    // A live session and two pending requests still fill the same global cap.
    let mut requested = request(67);
    requested.source = [63; 32];
    for now in [250, 500, 750] {
        run(
            &mut pair.listener,
            now,
            Input::WorkerHeartbeat {
                generation: [8; 16],
            },
        );
    }
    let effects = run(
        &mut pair.listener,
        1000,
        Input::Receive {
            parent: parent([63; 32], 63),
            message: Control::StartRequest(requested),
        },
    );
    assert_eq!(
        sent(&effects),
        Control::StartRefusal {
            request_id: requested.request_id,
            reason: Reason::RateLimited,
        }
    );
}

#[test]
fn worker_readiness_changes_are_generation_fenced_and_never_resume_old_keys() {
    let mut pair = Pair::new(true, true);
    let (view, _) = pair.active();
    let mut effects = Vec::new();
    assert_eq!(
        pair.listener.handle(
            13,
            Input::WorkerCapabilities {
                generation: [99; 16],
                capture: false,
                render: false
            },
            &mut effects
        ),
        Err(Error::StaleWorker)
    );
    assert!(effects.is_empty());
    assert_eq!(pair.listener.session(SOURCE).unwrap().phase, Phase::Active);
    let effects = run(
        &mut pair.listener,
        14,
        Input::WorkerCapabilities {
            generation: view.worker_generation,
            capture: false,
            render: false,
        },
    );
    assert!(effects
        .iter()
        .any(|effect| matches!(effect, Effect::InvalidateSession { .. })));
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::DevicePaused
    );
    let effects = run(
        &mut pair.listener,
        14,
        Input::WorkerCapabilities {
            generation: view.worker_generation,
            capture: false,
            render: false,
        },
    );
    assert!(
        effects.is_empty(),
        "unchanged readiness must not invalidate or announce again"
    );
    let mut effects = Vec::new();
    assert_eq!(
        pair.listener.handle(
            14,
            Input::WorkerCapabilities {
                generation: [99; 16],
                capture: false,
                render: false
            },
            &mut effects
        ),
        Err(Error::StaleWorker)
    );
    assert!(effects.is_empty());
    let effects = run(
        &mut pair.listener,
        15,
        Input::WorkerCapabilities {
            generation: view.worker_generation,
            capture: false,
            render: true,
        },
    );
    assert_eq!(derives(&effects) + starts(&effects), 0);
    assert_eq!(
        pair.listener.session(SOURCE).unwrap().phase,
        Phase::DevicePaused
    );
    let effects = run(
        &mut pair.listener,
        15,
        Input::WorkerCapabilities {
            generation: view.worker_generation,
            capture: false,
            render: true,
        },
    );
    assert!(
        effects.is_empty(),
        "unchanged ready state must not spam Hello"
    );
}

#[test]
fn stale_gui_starts_fail_without_advancing_time_or_expiring_requests() {
    let mut pair = Pair::new(true, true);
    run(
        &mut pair.source,
        0,
        Input::RequestStart {
            parent: pair.sp,
            request_id: [70; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    let stale = parent(LISTENER, 99);
    for input in [
        Input::RequestStart {
            parent: stale,
            request_id: [71; 16],
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
        Input::Start {
            parent: stale,
            session: [72; 16],
            listener_nonce: [73; 32],
            format: format(),
            format_sha256: DIGEST,
        },
    ] {
        let mut effects = Vec::new();
        assert_eq!(
            pair.source.handle(20_000, input, &mut effects),
            Err(Error::StaleParent)
        );
        assert!(effects.is_empty());
    }
    let effects = run(
        &mut pair.source,
        1,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: [70; 16],
                reason: Reason::Disabled,
            },
        },
    );
    assert_eq!(
        effects,
        vec![Effect::StartRequestResult {
            parent: pair.sp,
            request_id: [70; 16],
            result: Err(Reason::Disabled),
        }]
    );
}

#[test]
fn outgoing_request_limits_return_immediate_typed_errors() {
    let mut pair = Pair::new(true, true);
    let sp = pair.sp;
    let start = |id| Input::RequestStart {
        parent: sp,
        request_id: [id; 16],
        kind: audio_model::keys::StreamKind::System,
        profile: Profile::Ethernet,
    };
    run(&mut pair.source, 0, start(74));
    let mut effects = Vec::new();
    assert_eq!(
        pair.source.handle(0, start(75), &mut effects),
        Err(Error::Refused(Reason::RateLimited))
    );
    assert!(
        effects.is_empty(),
        "a pending request must not hide a GUI result"
    );
    run(
        &mut pair.source,
        1,
        Input::Receive {
            parent: pair.sp,
            message: Control::StartRefusal {
                request_id: [74; 16],
                reason: Reason::Disabled,
            },
        },
    );
    assert_eq!(
        pair.source.handle(2, start(75), &mut effects),
        Err(Error::Refused(Reason::RateLimited))
    );
    assert!(effects.is_empty(), "a fast retry must fail synchronously");
    for now in [250, 500, 750] {
        run(
            &mut pair.source,
            now,
            Input::WorkerHeartbeat {
                generation: [9; 16],
            },
        );
    }
    assert_eq!(
        pair.source.handle(1000, start(74), &mut effects),
        Err(Error::Refused(Reason::RateLimited))
    );
    assert!(
        effects.is_empty(),
        "a replayed request ID must fail synchronously"
    );
    assert!(matches!(
        sent(&run(&mut pair.source, 1000, start(75))),
        Control::StartRequest(_)
    ));
}

#[test]
fn rejected_correlated_offer_finishes_request_for_both_reservation_errors() {
    for reason in [Reason::ProtocolViolation, Reason::RateLimited] {
        let mut pair = Pair::new(true, true);
        let mut offer = pair.offer();
        let port = offer.listener_port;
        offer.listener_port = 0;
        let mut effects = Vec::new();
        assert_eq!(
            pair.source.handle(
                2,
                Input::Receive {
                    parent: pair.sp,
                    message: Control::Offer(offer),
                },
                &mut effects
            ),
            Err(Error::Refused(Reason::UnsupportedFormat))
        );
        assert!(pair.source.session(LISTENER).is_none());
        run(
            &mut pair.source,
            3,
            Input::RequestStart {
                parent: pair.sp,
                request_id: [76; 16],
                kind: audio_model::keys::StreamKind::System,
                profile: Profile::Ethernet,
            },
        );
        offer.listener_port = port;
        offer.request_id = Some([76; 16]);
        if reason == Reason::RateLimited {
            offer.session = [77; 16];
        }
        effects.clear();
        assert_eq!(
            pair.source.handle(
                4,
                Input::Receive {
                    parent: pair.sp,
                    message: Control::Offer(offer),
                },
                &mut effects
            ),
            Err(Error::Refused(reason))
        );
        assert!(effects.contains(&Effect::StartRequestResult {
            parent: pair.sp,
            request_id: [76; 16],
            result: Err(reason),
        }));
        assert_eq!(derives(&effects) + starts(&effects), 0);
        assert!(
            run(
                &mut pair.source,
                5,
                Input::Receive {
                    parent: pair.sp,
                    message: Control::StartRefusal {
                        request_id: [76; 16],
                        reason: Reason::Timeout
                    },
                }
            )
            .is_empty(),
            "failed reservation must clear the pending request"
        );
    }
}

#[test]
fn incoming_requests_to_source_do_not_poison_outgoing_request_budget() {
    let mut pair = Pair::new(true, true);
    for index in 0..USED_REQUEST_LIMIT {
        let mut incoming = request(0);
        incoming.request_id[..2].copy_from_slice(&(index as u16).to_be_bytes());
        assert!(run(
            &mut pair.source,
            0,
            Input::Receive {
                parent: pair.sp,
                message: Control::StartRequest(incoming),
            }
        )
        .is_empty());
    }
    let effects = run(
        &mut pair.source,
        0,
        Input::RequestStart {
            parent: pair.sp,
            request_id: request(0).request_id,
            kind: audio_model::keys::StreamKind::System,
            profile: Profile::Ethernet,
        },
    );
    assert!(matches!(sent(&effects), Control::StartRequest(_)));
}
