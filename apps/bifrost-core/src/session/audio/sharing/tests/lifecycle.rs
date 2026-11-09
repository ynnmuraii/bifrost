use super::*;

pub(in crate::session::audio) struct OwnedRoot(std::path::PathBuf);
impl Drop for OwnedRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(in crate::session::audio) fn listener_fixture(
) -> (OwnedRoot, Coordinator, Actor, NodeId, fsm::Parent) {
    listener_fixture_at(fsm::Phase::Active)
}

fn listener_fixture_at(phase: fsm::Phase) -> (OwnedRoot, Coordinator, Actor, NodeId, fsm::Parent) {
    let root = OwnedRoot(std::env::temp_dir().join(format!(
        "bf-media-recovery-{}",
        u128::from_be_bytes(identity::random16().unwrap())
    )));
    let identity = Arc::new(
        identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(&root.0))
            .unwrap()
            .0,
    );
    let local = identity.node_id();
    let peer = NodeId::new([7; 32]);
    let (manager, _events, _trust) = manager_for(identity, peer, None, Vec::new());
    let config = config::AudioConfig {
        enabled: true,
        system_sharing: Some(true),
        role: config::AudioRole::Listener,
        group_id: Some(config::audio::HexId([3; 16])),
        allow_system_audio_from: vec![config::audio::HexId(*peer.as_bytes())],
        ..Default::default()
    };
    let paths = config::Paths::new(&root.0, &root.0, &root.0, Some(root.0.clone()));
    let (events, _consumer) = broadcast::channel(32);
    let (coordinator, mut actor) = Coordinator::prepare(
        config,
        local,
        manager,
        paths,
        "127.0.0.1:0".parse().unwrap(),
        true,
        events,
    );
    actor.system_requested = true;
    actor.configure().unwrap();
    let parent = fsm::Parent {
        peer: *peer.as_bytes(),
        connection_id: [4; 16],
        generation: [5; 16],
    };
    let support = fsm::Support {
        control: true,
        system_source: true,
        listener: false,
    };
    let hello = fsm::Hello {
        role: model::AudioRole::Source,
        group: Some([3; 16]),
        listener: *local.as_bytes(),
        available: true,
        allowed: true,
        consent_revision: 1,
    };
    actor.peers.insert(
        peer,
        Peer {
            parent,
            name: "owned-source".into(),
            remote: "127.0.0.1:40002".parse().unwrap(),
            support,
            trusted: true,
            hello: Some(hello),
            focus_support: false,
            mic_support: false,
            media_support: true,
            media_seek_support: false,
            rekey_support: false,
            artwork_support: false,
            artwork_rejected: 0,
            focus_sent_revision: 0,
            focus_request_at: None,
            focus_reply_at: None,
            focus_reply: false,
        },
    );
    coordinator.links.send_modify(|links| {
        links.peers = actor.peers.clone();
        links.trusted.insert(peer);
    });
    actor.admitted.insert(peer);
    for input in [
        fsm::Input::WorkerUp {
            generation: generation(1, 0),
            capture: false,
            render: true,
        },
        fsm::Input::PeerUp {
            parent,
            support,
            trusted: true,
            local_allowed: true,
            consent_revision: actor.consent_revision,
        },
        fsm::Input::Receive {
            parent,
            message: fsm::Control::Hello(hello),
        },
        fsm::Input::Start {
            parent,
            session: [6; 16],
            listener_nonce: [8; 32],
            format: format(Profile::Ethernet),
            format_sha256: wire::format_sha256(format(Profile::Ethernet)),
        },
        fsm::Input::Receive {
            parent,
            message: fsm::Control::Answer(fsm::Answer {
                session: [6; 16],
                accepted: true,
                reason: None,
                format: Some(format(Profile::Ethernet)),
                source_nonce: Some([9; 32]),
                source_port: 40002,
                consent_revision: 1,
            }),
        },
    ] {
        actor.feed(input).unwrap();
    }
    let agreement = actor
        .effects
        .iter()
        .find_map(|effect| match effect {
            fsm::Effect::DeriveKeys { agreement, .. } => Some(*agreement),
            _ => None,
        })
        .expect("real listener model agreement");
    let view = actor.model.session(*peer.as_bytes()).unwrap();
    for input in [
        fsm::Input::KeysReady {
            view,
            transcript_sha256: [10; 32],
        },
        fsm::Input::ChildPrepared(view),
        fsm::Input::ChildStartIssued(view),
    ] {
        actor.feed(input).unwrap();
    }
    if phase != fsm::Phase::Committed {
        actor.feed(fsm::Input::ChildStarted(view)).unwrap();
        actor.feed(fsm::Input::PathReady(view)).unwrap();
    }
    if phase == fsm::Phase::Rebuffering {
        actor.feed(fsm::Input::Buffering(view)).unwrap();
    }
    let view = actor.model.session(*peer.as_bytes()).unwrap();
    assert_eq!(view.phase, phase);
    actor.records.insert(
        peer,
        Record {
            view,
            agreement,
            role: model::AudioRole::Listener,
            material: None,
            start_request_id: Some(2),
            health: Default::default(),
            health_state: model::StreamHealth::Ready,
            latency_estimate_ms: None,
            latency_stages: None,
            path_diagnostic: model::PathDiagnostic::None,
            remote_quality: None,
            capture_active: None,
            sending: None,
            applied_gain: None,
            rotation: rotation::State::default(),
        },
    );
    let (handle, _stops, _shutdown, _native) = AudioHandle::controlled_stop(1);
    actor.worker = Some(Worker {
        handle,
        generation: 1,
        alive: true,
        model_ready: true,
        pid: 0,
        probe_ack: 0,
    });
    actor.effects.clear();
    (root, coordinator, actor, peer, parent)
}

async fn install_paused_snapshot(actor: &mut Actor, peer: NodeId, parent: fsm::Parent) {
    actor.media_tick().await;
    let session = actor.records[&peer].view.session;
    actor
        .media_on_network(
            parent,
            &net::AudioControl {
                control_version: 1,
                body: Some(net::audio_control::Body::MediaInfo(net::MediaInfo {
                    session_id: session.to_vec(),
                    present: true,
                    token: 11,
                    revision: 12,
                    app: "owned-player".into(),
                    title: "Owned paused track".into(),
                    playback: net::MediaPlayback::Paused as i32,
                    commands: 1,
                    allow_control: true,
                    ..Default::default()
                })),
            },
        )
        .await;
    let info = actor
        .media_info(peer)
        .expect("baseline snapshot admitted by the real actor");
    assert_eq!(info.playback, model::PlaybackState::Paused);
    assert!(info.commands.play && actor.media_allowed(peer));
}

#[tokio::test]
async fn listener_rebuffer_recovers_unchanged_paused_player_and_play_permission() {
    let (_root, _coordinator, mut actor, peer, parent) = listener_fixture();
    install_paused_snapshot(&mut actor, peer, parent).await;
    let before = actor.media_info(peer).unwrap();
    let view = actor.records[&peer].view;
    actor.feed(fsm::Input::Buffering(view)).unwrap();
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Rebuffering);
    actor.media_tick().await;
    assert!(actor.media_info(peer).is_none());
    assert!(
        !actor.media_allowed(peer),
        "controls remain disabled until playout recovers"
    );
    let rebuffering = actor.records[&peer].view;
    actor.feed(fsm::Input::PathReady(rebuffering)).unwrap();
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Active);
    // No second network snapshot or native MediaChanged is supplied.
    actor.media_tick().await;
    let recovered = actor
        .media_info(peer)
        .expect("same live paused player must recover without a metadata revision change");
    assert_eq!(recovered, before);
    assert!(
        recovered.commands.play && actor.media_allowed(peer),
        "Play permission must recover for the same consented system-audio session"
    );
}

#[tokio::test]
async fn real_consent_withdrawal_still_erases_paused_metadata() {
    let (_root, _coordinator, mut actor, peer, parent) = listener_fixture();
    install_paused_snapshot(&mut actor, peer, parent).await;
    actor.config.allow_system_audio_from.clear();
    actor
        .feed(fsm::Input::LocalConsent {
            peer: *peer.as_bytes(),
            allowed: false,
            revision: actor.consent_revision + 1,
        })
        .unwrap();
    actor.media_tick().await;
    assert!(actor.media_info(peer).is_none());
    assert!(!actor.media_allowed(peer));
}

#[tokio::test]
async fn session_local_async_error_reaches_actor_without_losing_worker_or_retirement() {
    let (_root, _coordinator, mut actor, peer, _parent) = listener_fixture();
    let view = actor.records[&peer].view;
    actor
        .on_worker(AudioSupervisorEvent::CommandRejected {
            generation: 1,
            request_id: 0,
            error: worker::Error {
                code: 8,
                session_id: view.session.to_vec(),
            },
        })
        .await;
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
    assert_eq!(
        actor.records[&peer].view.reason,
        Some(fsm::Reason::DeviceUnavailable)
    );
    assert!(actor.worker.as_ref().unwrap().alive);
    assert!(actor.native_stops_pending());
    actor
        .on_worker(AudioSupervisorEvent::SessionEvent {
            generation: 1,
            event: worker::SessionEvent {
                session_id: view.session.to_vec(),
                kind: worker::EventKind::Stopped as i32,
                ..Default::default()
            },
        })
        .await;
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
    assert!(!actor.native_stops_pending());
    assert!(actor.worker.as_ref().unwrap().alive);
}

#[tokio::test]
async fn late_stopped_cannot_turn_genuine_worker_loss_into_positive_native_completion() {
    let (_root, _coordinator, mut actor, peer, _parent) = listener_fixture();
    let view = actor.records[&peer].view;
    actor
        .on_worker(AudioSupervisorEvent::Failed {
            generation: 1,
            session_id: None,
            reason: AudioSupervisorError::ChildLost,
        })
        .await;
    assert_eq!(
        actor.records[&peer].view.reason,
        Some(fsm::Reason::WorkerLost)
    );
    actor
        .on_worker(AudioSupervisorEvent::SessionEvent {
            generation: 1,
            event: worker::SessionEvent {
                session_id: view.session.to_vec(),
                kind: worker::EventKind::Stopped as i32,
                ..Default::default()
            },
        })
        .await;
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
    assert_eq!(
        actor.records[&peer].view.reason,
        Some(fsm::Reason::WorkerLost)
    );
    assert_eq!(actor.records[&peer].capture_active, None);
    assert_eq!(actor.records[&peer].sending, None);
    assert_eq!(actor.records[&peer].start_request_id, None);
}

fn advance_past_start_rate_limit(actor: &mut Actor, view: fsm::SessionView) {
    let baseline = actor.now();
    for advance in [250, 500, 750, 1_000, 1_250] {
        actor.epoch = Instant::now()
            .checked_sub(Duration::from_millis(baseline + advance))
            .unwrap();
        actor
            .feed(fsm::Input::WorkerHeartbeat {
                generation: view.worker_generation,
            })
            .unwrap();
        actor.feed(fsm::Input::Tick).unwrap();
    }
}

#[tokio::test]
async fn rejected_start_retires_exact_issued_command_without_waiting_for_stopped() {
    let (_root, _coordinator, mut actor, peer, parent) = listener_fixture_at(fsm::Phase::Committed);
    let view = actor.records[&peer].view;
    advance_past_start_rate_limit(&mut actor, view);
    for (gen, id) in [(0, view.session), (1, [99; 16])] {
        actor
            .on_worker(AudioSupervisorEvent::CommandRejected {
                generation: gen,
                request_id: 2,
                error: worker::Error {
                    code: 8,
                    session_id: id.to_vec(),
                },
            })
            .await;
        assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Committed);
        assert_eq!(actor.records[&peer].start_request_id, Some(2));
    }
    actor
        .on_worker(AudioSupervisorEvent::CommandRejected {
            generation: 1,
            request_id: 2,
            error: worker::Error {
                code: 8,
                session_id: view.session.to_vec(),
            },
        })
        .await;
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
    assert_eq!(
        actor.records[&peer].view.reason,
        Some(fsm::Reason::DeviceUnavailable)
    );
    assert_eq!(actor.records[&peer].start_request_id, None);
    assert!(!actor.native_stops_pending());
    assert!(actor.worker.as_ref().unwrap().alive);
    assert!(!actor
        .effects
        .iter()
        .any(|effect| matches!(effect, fsm::Effect::StopChild { .. })));
    actor
        .feed(fsm::Input::Start {
            parent,
            session: [60; 16],
            listener_nonce: [61; 32],
            format: format(Profile::Ethernet),
            format_sha256: wire::format_sha256(format(Profile::Ethernet)),
        })
        .expect("definitive start rejection must release model replacement admission");
    assert_eq!(actor.model.session(parent.peer).unwrap().session, [60; 16]);
}

#[tokio::test]
async fn issued_start_unsolicited_error_still_waits_for_native_cleanup() {
    let (_root, _coordinator, mut actor, peer, parent) = listener_fixture_at(fsm::Phase::Committed);
    let view = actor.records[&peer].view;
    actor
        .on_worker(AudioSupervisorEvent::CommandRejected {
            generation: 1,
            request_id: 0,
            error: worker::Error {
                code: 8,
                session_id: view.session.to_vec(),
            },
        })
        .await;
    assert!(actor.native_stops_pending());
    assert_eq!(actor.records[&peer].start_request_id, Some(2));
    assert!(actor
        .feed(fsm::Input::Start {
            parent,
            session: [60; 16],
            listener_nonce: [61; 32],
            format: format(Profile::Ethernet),
            format_sha256: wire::format_sha256(format(Profile::Ethernet)),
        })
        .is_err());
    assert_eq!(
        actor.model.session(parent.peer).unwrap().session,
        view.session
    );
}

#[tokio::test]
async fn correlated_rejection_after_worker_loss_cannot_replace_failure_with_native_success() {
    let (_root, _coordinator, mut actor, peer, _parent) =
        listener_fixture_at(fsm::Phase::Committed);
    let view = actor.records[&peer].view;
    actor
        .on_worker(AudioSupervisorEvent::Failed {
            generation: 1,
            session_id: None,
            reason: AudioSupervisorError::ChildLost,
        })
        .await;
    actor
        .on_worker(AudioSupervisorEvent::CommandRejected {
            generation: 1,
            request_id: 2,
            error: worker::Error {
                code: 8,
                session_id: view.session.to_vec(),
            },
        })
        .await;
    assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
    assert_eq!(
        actor.records[&peer].view.reason,
        Some(fsm::Reason::WorkerLost)
    );
    assert_eq!(actor.records[&peer].capture_active, None);
    assert_eq!(actor.records[&peer].sending, None);
}

#[tokio::test]
async fn spontaneous_stopped_retires_model_owner_in_every_started_phase() {
    for phase in [
        fsm::Phase::Committed,
        fsm::Phase::Active,
        fsm::Phase::Rebuffering,
    ] {
        let (_root, _coordinator, mut actor, peer, parent) = listener_fixture_at(phase);
        let view = actor.records[&peer].view;
        advance_past_start_rate_limit(&mut actor, view);
        actor
            .on_worker(AudioSupervisorEvent::SessionEvent {
                generation: 1,
                event: worker::SessionEvent {
                    session_id: view.session.to_vec(),
                    kind: worker::EventKind::Stopped as i32,
                    reason: worker::StopReason::PathTimeout as i32,
                    ..Default::default()
                },
            })
            .await;
        assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Failed);
        assert_eq!(actor.records[&peer].view.reason, Some(fsm::Reason::Timeout));
        assert_eq!(actor.records[&peer].start_request_id, None);
        assert!(!actor.native_stops_pending());
        // No second StopSession/Stopped exchange is supplied for the absent native stream.
        actor
            .feed(fsm::Input::Start {
                parent,
                session: [60; 16],
                listener_nonce: [61; 32],
                format: format(Profile::Ethernet),
                format_sha256: wire::format_sha256(format(Profile::Ethernet)),
            })
            .expect("actual native Stopped must also retire the model's owner");
        assert_eq!(actor.model.session(parent.peer).unwrap().session, [60; 16]);
    }
}

#[tokio::test]
async fn long_rebuffer_cannot_resurrect_withdrawn_metadata_or_play_permission() {
    for present in [false, true] {
        let (_root, _coordinator, mut actor, peer, parent) = listener_fixture();
        install_paused_snapshot(&mut actor, peer, parent).await;
        let view = actor.records[&peer].view;
        actor.feed(fsm::Input::Buffering(view)).unwrap();
        actor.media_tick().await;
        actor
            .media_on_network(
                parent,
                &net::AudioControl {
                    control_version: 1,
                    body: Some(net::audio_control::Body::MediaInfo(net::MediaInfo {
                        session_id: view.session.to_vec(),
                        present,
                        token: 11,
                        revision: 13,
                        app: "owned-player".into(),
                        title: "Owned paused track".into(),
                        playback: net::MediaPlayback::Paused as i32,
                        commands: 1,
                        allow_control: false,
                        ..Default::default()
                    })),
                },
            )
            .await;
        let baseline = actor.now();
        for advance in (250..=fsm::SETUP_TIMEOUT_MS + 1_500).step_by(250) {
            actor.epoch = Instant::now()
                .checked_sub(Duration::from_millis(baseline + advance))
                .unwrap();
            actor
                .feed(fsm::Input::WorkerHeartbeat {
                    generation: view.worker_generation,
                })
                .unwrap();
            actor
                .feed(fsm::Input::Receive {
                    parent,
                    message: fsm::Control::Keepalive {
                        session: view.session,
                        serial: advance / 250,
                        authorized_for_ms: fsm::PEER_TTL_MS,
                    },
                })
                .unwrap();
            actor.feed(fsm::Input::Tick).unwrap();
            actor.media_tick().await;
            assert_eq!(actor.records[&peer].view.phase, fsm::Phase::Rebuffering);
            assert!(actor.media_info(peer).is_none());
            assert!(!actor.media_allowed(peer));
        }
        actor
            .feed(fsm::Input::PathReady(actor.records[&peer].view))
            .unwrap();
        actor.media_tick().await;
        if present {
            assert_eq!(actor.media_info(peer).unwrap().revision, 13);
        } else {
            assert!(actor.media_info(peer).is_none());
        }
        assert!(!actor.media_allowed(peer));
        let (out, mut replies) = crate::gui::Replies::channel(4);
        actor
            .media_on_gui(Request {
                request_id: 90,
                connection_id: [91; 16],
                out,
                message: gui::gui_frame::Msg::SendMediaCommand(gui::SendMediaCommand {
                    peer_node_id: peer.as_bytes().to_vec(),
                    session_id: view.session.to_vec(),
                    session_token: 11,
                    media_revision: 12,
                    command: gui::AudioMediaCommand::Play as i32,
                    position_ms: None,
                }),
            })
            .await;
        let (id, response) = replies.recv().await.unwrap();
        assert_eq!(id, 90);
        assert!(matches!(response, gui::gui_frame::Msg::OpResult(result)
            if !result.ok && result.error_code == ipc::audio::codes::AUDIO_CONSENT_REQUIRED));
    }
}
