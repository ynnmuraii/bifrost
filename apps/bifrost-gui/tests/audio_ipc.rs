//! Audio presentation uses revisioned snapshots, not optimistic requests.
mod support;
use audio_model::status as model;
use bifrost_gui::{
    audio::{self, AudioAction},
    audio_player::{AppKind, AudioPlayerAction, StreamState},
};
use protocol::ipc::{self, gui_frame::Msg};
use std::time::{Duration, Instant};
use support::{event, view};

fn snapshot(revision: u64) -> ipc::AudioStatus {
    let mut config = audio::default_config();
    config.enabled = true;
    config.role = ipc::AudioRole::Listener as i32;
    config.mode = ipc::AudioMode::FollowFocus as i32;
    config.group_id = vec![9; 16];
    let local = model::ConsentFlags {
        allow_system_receive: true,
        ..Default::default()
    };
    let remote = model::ConsentFlags {
        allow_system_send: true,
        allow_media_control: true,
        ..Default::default()
    };
    let state = model::AudioStatus {
        revision,
        enabled: true,
        system_sharing_requested: None,
        system_sharing_error: None,
        role: model::AudioRole::Listener,
        mode: model::AudioMode::FollowFocus,
        backend: Vec::new(),
        sessions: vec![model::SessionStatus {
            session_id: [1; 16],
            peer: [2; 32],
            source_name: "WORKSTATION".into(),
            local_role: model::AudioRole::Listener,
            state: model::SessionState::Running,
            profile: model::AudioProfile::Ethernet,
            health: model::AudioHealth {
                underruns: 10,
                ..Default::default()
            },
            health_state: model::StreamHealth::Ready,
            latency_estimate_ms: None,
            gain_percent: 65,
            muted: true,
            consent: model::SessionConsent { local, remote },
            media_info: Some(model::MediaInfo {
                app_id: "browser".into(),
                title: "Private title".into(),
                subtitle: String::new(),
                position_ms: Some(0),
                duration_ms: None,
                session_token: 7,
                playback: model::PlaybackState::Playing,
                commands: model::MediaCommands {
                    pause: true,
                    next: true,
                    ..Default::default()
                },
                revision: 3,
            }),
            focus_primary: false,
            path_diagnostic: model::PathDiagnostic::None,
            local_udp_port: 0,
            peer_udp_port: 0,
            remote_quality: None,
            latency_stages: None,
        }],
    };
    bifrost_gui::audio_wire::encode_status(
        &state,
        config,
        ipc::AudioMicrophoneState {
            muted: true,
            ..Default::default()
        },
    )
    .unwrap()
}

#[test]
fn native_ptt_observations_survive_status_mapping_without_synthetic_gui_intents() {
    for state in [
        None,
        Some(ipc::AudioPttArmingState::Armed as i32),
        Some(ipc::AudioPttArmingState::NeedsPhysicalRelease as i32),
        Some(ipc::AudioPttArmingState::Unavailable as i32),
    ] {
        let mut vm = view();
        let mut status = snapshot(5);
        status.microphone.as_mut().unwrap().native_ptt_arming = state;
        vm.apply(event(Msg::AudioStatus(status)));
        assert_eq!(
            vm.audio
                .status
                .as_ref()
                .unwrap()
                .microphone
                .as_ref()
                .unwrap()
                .native_ptt_arming,
            state
        );
        assert!(!vm.audio.gui_ptt_held);
        assert!(vm.take_outgoing().is_empty());
    }
}

#[test]
fn native_ptt_unavailable_reason_survives_gui_status_mapping() {
    for reason in [
        None,
        Some(ipc::AudioPttUnavailableReason::HostClockUnavailable as i32),
        Some(ipc::AudioPttUnavailableReason::PortalFreshnessUnverifiable as i32),
        Some(ipc::AudioPttUnavailableReason::RegistrationUnavailable as i32),
    ] {
        let mut vm = view();
        let mut status = snapshot(5);
        let mic = status.microphone.as_mut().unwrap();
        mic.native_ptt_arming = Some(ipc::AudioPttArmingState::Unavailable as i32);
        mic.native_ptt_unavailable_reason = reason;
        vm.apply(event(Msg::AudioStatus(status)));
        assert_eq!(
            vm.audio
                .status
                .as_ref()
                .unwrap()
                .microphone
                .as_ref()
                .unwrap()
                .native_ptt_unavailable_reason,
            reason
        );
        assert!(!vm.audio.gui_ptt_held);
        assert!(vm.take_outgoing().is_empty());
    }
}

#[test]
fn status_mapping_preserves_presence_permissions_identity_and_current_health() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let card = &vm.audio_player.cards[0];
    assert_eq!(card.stream_id, "01".repeat(16));
    assert_eq!(card.app, AppKind::System);
    assert_eq!(card.position_ms, Some(0));
    assert_eq!(card.duration_ms, None);
    assert!(card.has_media_session && card.playing && card.can_play_pause && card.can_next);
    assert!(!card.can_prev);
    assert!(card.background && card.muted);
    assert_eq!(card.state, StreamState::Ready); // Historical counters do not invent current degradation.
    assert!(vm.audio_player.open);
    let mut stale = snapshot(4);
    stale.sessions.clear();
    vm.apply(event(Msg::AudioStatus(stale)));
    assert_eq!(vm.audio_player.cards.len(), 1);
    vm.on_disconnected("test".into());
    assert!(vm.audio_player.cards.is_empty());
    vm.apply(event(Msg::AudioStatus(snapshot(1)))); // Revision namespace resets on connection loss.
    assert_eq!(vm.audio.status.as_ref().unwrap().revision, 1);
}

#[test]
fn unavailable_native_media_identity_is_display_only_never_an_implicit_current_command() {
    for (token, revision) in [(0, 3), (7, 0), (0, 0)] {
        let mut value = snapshot(5);
        let media = value.sessions[0].media_info.as_mut().unwrap();
        media.session_token = token;
        media.revision = revision;
        let mut vm = view();
        vm.apply(event(Msg::AudioStatus(value)));
        let id = vm.audio_player.cards[0].stream_id.clone();
        assert!(audio::player_request(
            vm.audio.status.as_ref().unwrap(),
            &AudioPlayerAction::PlayPause(id)
        )
        .is_none());
        assert!(vm.audio.status.as_ref().unwrap().sessions[0]
            .media_info
            .is_some());
    }
}

#[test]
fn player_actions_target_the_exact_session_and_never_change_local_playback() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let id = vm.audio_player.cards[0].stream_id.clone();
    let now = Instant::now();
    vm.audio_action(
        &AudioAction::Player(AudioPlayerAction::PlayPause(id.clone())),
        now,
    );
    let commands = vm.take_outgoing();
    assert!(
        matches!(&commands[0].1,Msg::SendMediaCommand(c) if c.peer_node_id==vec![2;32]&&c.session_id==vec![1;16]&&c.session_token==7&&c.media_revision==3&&c.command==ipc::AudioMediaCommand::Pause as i32)
    );
    assert!(vm.audio_player.cards[0].playing);
    vm.apply(ipc::GuiFrame {
        request_id: commands[0].0,
        msg: Some(Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        })),
        ..Default::default()
    });
    assert!(vm.audio.pending);
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    assert!(!vm.audio.pending);
    let gain = audio::player_request(
        vm.audio.status.as_ref().unwrap(),
        &AudioPlayerAction::SetGain {
            stream_id: id.clone(),
            percent: 25,
        },
    )
    .unwrap();
    assert!(
        matches!(gain,Msg::SetAudioPeerGain(c) if c.gain_percent==25&&c.muted&&c.expected_revision==6)
    );
    let mute = audio::player_request(
        vm.audio.status.as_ref().unwrap(),
        &AudioPlayerAction::SetMuted {
            stream_id: id.clone(),
            muted: false,
        },
    )
    .unwrap();
    assert!(matches!(mute,Msg::SetAudioPeerGain(c) if c.gain_percent==65&&!c.muted));
    let stop = audio::player_request(
        vm.audio.status.as_ref().unwrap(),
        &AudioPlayerAction::Stop(id.clone()),
    )
    .unwrap();
    assert!(matches!(stop, Msg::StopAudio(request)
        if !request.stop_all && request.session_id == vec![1; 16] && request.peer_node_id == vec![2; 32]));
    let mut replacement = snapshot(7);
    replacement.sessions[0].session_id = vec![4; 16];
    assert!(audio::player_request(&replacement, &AudioPlayerAction::Stop(id)).is_none());
}

#[test]
fn denied_media_permission_and_missing_session_do_not_dispatch() {
    let mut status = snapshot(5);
    let id = "01".repeat(16);
    status.sessions[0]
        .consent
        .as_mut()
        .unwrap()
        .remote
        .as_mut()
        .unwrap()
        .allow_media_control = false;
    assert!(audio::player_request(&status, &AudioPlayerAction::Next(id.clone())).is_none());
    let model = bifrost_gui::audio_wire::decode_status(&status).unwrap();
    assert!(!audio::map_cards(&model, &[])[0].can_next);
    status.sessions[0].media_info = None;
    let cards = audio::map_cards(
        &bifrost_gui::audio_wire::decode_status(&status).unwrap(),
        &[],
    );
    assert!(!cards[0].has_media_session);
}

#[test]
fn popup_source_names_follow_trusted_membership_in_either_ipc_order() {
    for media in [true, false] {
        for members_first in [true, false] {
            let mut vm = view();
            let mut audio = snapshot(5);
            audio.sessions[0].source_name = "source".into();
            if !media {
                audio.sessions[0].media_info = None;
            }
            let mut membership = support::status_with("LOCAL");
            membership.members[1].node_id = "02".repeat(32).to_uppercase();
            membership.members[1].display_name = "LAPTOP".into();
            if members_first {
                vm.apply(event(Msg::Status(membership.clone())));
            }
            vm.apply(event(Msg::AudioStatus(audio)));
            if !members_first {
                assert_eq!(vm.audio_player.cards[0].source_name, "02020202");
                vm.apply(event(Msg::Status(membership.clone())));
            }
            assert_eq!(vm.audio_player.cards[0].source_name, "LAPTOP");
            assert_eq!(vm.audio_player.cards[0].stream_id, "01".repeat(16));
            membership.members[1].display_name = "HTPC".into();
            vm.apply(event(Msg::Status(membership.clone())));
            assert_eq!(vm.audio_player.cards[0].source_name, "HTPC");
            membership.members[1].display_name = "   ".into();
            vm.apply(event(Msg::Status(membership.clone())));
            assert_eq!(vm.audio_player.cards[0].source_name, "02020202");
            membership.members[1].node_id = "03".repeat(32);
            membership.members[1].display_name = "Wrong peer".into();
            vm.apply(event(Msg::Status(membership)));
            assert_eq!(vm.audio_player.cards[0].source_name, "02020202");
            let request = audio::player_request(
                vm.audio.status.as_ref().unwrap(),
                &AudioPlayerAction::Stop("01".repeat(16)),
            );
            assert!(
                matches!(request, Some(Msg::StopAudio(stop)) if stop.session_id == vec![1; 16] && stop.peer_node_id == vec![2; 32])
            );
        }
    }
}

#[test]
fn normal_permission_and_gain_edits_preserve_group_policy_through_apply_and_status() {
    for group_id_override in [None, Some(false), Some(true)] {
        let mut vm = view();
        let mut status = snapshot(5);
        status.role = ipc::AudioRole::Source as i32;
        status.sessions.clear();
        let config = status.config.as_mut().unwrap();
        config.role = ipc::AudioRole::Source as i32;
        config.listener_node_id = vec![2; 32];
        config.group_id_override = group_id_override;
        vm.apply(event(Msg::AudioStatus(status.clone())));
        let now = Instant::now();
        let mut draft = vm.audio.draft.clone();
        draft.background_gain_db = -24;
        draft.peers.push(ipc::AudioPeerConfig {
            node_id: vec![2; 32],
            allow_system_send: true,
            volume_percent: 70,
            ..Default::default()
        });
        vm.audio_action(&AudioAction::EditConfig(Box::new(draft.clone())), now);
        assert_eq!(vm.audio.draft.group_id_override, group_id_override);
        vm.audio_action(&AudioAction::ApplyConfig, now);
        let (_, message) = vm.take_outgoing().pop().unwrap();
        let Msg::SetAudio(request) = message else {
            panic!("expected SetAudio");
        };
        assert_eq!(request.config, Some(draft.clone()));
        status.revision += 1;
        status.config = request.config;
        vm.apply(event(Msg::AudioStatus(status)));
        assert_eq!(vm.audio.draft, draft);
        assert!(!vm.audio.dirty);
        assert!(!vm.audio.pending);
    }
}

#[test]
fn manual_source_group_draft_survives_a_different_automatic_listener_group_status() {
    let mut vm = view();
    let mut status = snapshot(5);
    status.role = ipc::AudioRole::Source as i32;
    status.sessions.clear();
    let config = status.config.as_mut().unwrap();
    config.role = ipc::AudioRole::Source as i32;
    config.listener_node_id = vec![2; 32];
    vm.apply(event(Msg::AudioStatus(status.clone())));
    let now = Instant::now();
    let mut draft = vm.audio.draft.clone();
    draft.group_id = vec![7; 16];
    draft.group_id_override = Some(true);
    draft.background_gain_db = -24;
    vm.audio_action(&AudioAction::EditConfig(Box::new(draft.clone())), now);
    status.revision += 1;
    status.config.as_mut().unwrap().group_id = vec![8; 16];
    vm.apply(event(Msg::AudioStatus(status)));
    assert_eq!(vm.audio.draft, draft);
    assert!(vm.audio.dirty);
    vm.audio_action(&AudioAction::ApplyConfig, now);
    let (_, message) = vm.take_outgoing().pop().unwrap();
    let Msg::SetAudio(request) = message else {
        panic!("expected SetAudio");
    };
    assert_eq!(request.config, Some(draft));
    assert_eq!(request.expected_revision, 6);
}

#[test]
fn unsupported_and_timeout_rollback_pending_without_claiming_success() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let now = Instant::now();
    let mut draft = vm.audio.draft.clone();
    draft.background_mute = true;
    vm.audio_action(&AudioAction::EditConfig(Box::new(draft)), now);
    vm.audio_action(&AudioAction::ApplyConfig, now);
    let (id, _) = vm.take_outgoing().pop().unwrap();
    assert!(vm.audio.pending);
    assert!(
        !vm.audio
            .status
            .as_ref()
            .unwrap()
            .config
            .as_ref()
            .unwrap()
            .background_mute
    );
    vm.apply(ipc::GuiFrame {
        request_id: id,
        msg: Some(Msg::OpResult(ipc::OpResult {
            ok: false,
            error_code: "UNKNOWN_REQUEST".into(),
        })),
        ..Default::default()
    });
    assert!(!vm.audio.available && !vm.audio.pending);
    assert!(!vm.audio.draft.background_mute);
    assert!(vm.audio.error.is_some());
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    vm.audio_action(&AudioAction::StopSystemAudio, now);
    vm.tick(now + Duration::from_secs(11));
    assert!(!vm.audio.pending && !vm.audio.available);
}

#[test]
fn system_audio_switch_uses_sparse_current_config_intent_even_when_policy_is_already_on() {
    for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
        let mut vm = view();
        let mut status = sharing_snapshot(5, role, false);
        let config = status.config.as_mut().unwrap();
        config.peers.push(ipc::AudioPeerConfig {
            node_id: vec![2; 32],
            allow_media_control: role == ipc::AudioRole::Source,
            allow_mic_control: true,
            volume_percent: 73,
            ..Default::default()
        });
        let before = status.clone();
        bifrost_gui::audio_wire::decode_status(&status).expect("valid role-scoped fixture");
        vm.apply(event(Msg::AudioStatus(status)));
        let action = if role == ipc::AudioRole::Source {
            AudioAction::Share(true)
        } else {
            AudioAction::Receive(true)
        };
        vm.audio_action(&action, Instant::now());
        let (id, message) = vm.take_outgoing().pop().expect("one sparse intent");
        let Msg::SetSystemAudioSharing(request) = message else {
            panic!("not sparse system intent")
        };
        assert!(request.enabled);
        assert_eq!(request.role, role as i32);
        assert_eq!(request.expected_revision, 5);
        assert_eq!(
            request.listener_node_id,
            if role == ipc::AudioRole::Source {
                vec![2; 32]
            } else {
                Vec::new()
            }
        );
        assert_eq!(vm.audio.status.as_ref().unwrap(), &before);
        assert_eq!(vm.audio.draft, *before.config.as_ref().unwrap());
        assert!(vm.audio.sharing_requested(role));
        assert!(!vm.audio_player.open);
        vm.apply(ipc::GuiFrame {
            request_id: id,
            msg: Some(Msg::OpResult(ipc::OpResult {
                ok: true,
                error_code: String::new(),
            })),
            ..Default::default()
        });
        assert!(vm.audio.pending, "ACK is not worker activity");
        assert!(!vm.audio_player.open);
    }
}

#[test]
fn gui_ptt_release_bypasses_pending_or_unavailable_status_without_optimism() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    vm.audio_action(&AudioAction::SetPtt(true), Instant::now());
    assert!(vm.audio.gui_ptt_held && vm.audio.pending);
    assert!(
        !vm.audio
            .status
            .as_ref()
            .unwrap()
            .microphone
            .as_ref()
            .unwrap()
            .ptt_pressed
    );
    vm.take_outgoing();
    vm.audio.available = false;
    vm.audio_action(&AudioAction::SetPtt(false), Instant::now());
    assert!(!vm.audio.gui_ptt_held);
    assert!(matches!(&vm.take_outgoing()[0].1, Msg::SetPtt(value) if !value.pressed));
}

#[test]
fn device_pages_are_correlated_revision_scoped_and_cumulatively_bounded() {
    let mut vm = view();
    vm.audio_action(
        &AudioAction::ListDevices(ipc::AudioDeviceKind::Output),
        Instant::now(),
    );
    let id = vm.take_outgoing()[0].0;
    let page = |revision, complete, names: &[&str]| ipc::AudioDevices {
        kind: ipc::AudioDeviceKind::Output as i32,
        revision,
        complete,
        devices: names
            .iter()
            .map(|name| ipc::AudioDevice {
                id: (*name).into(),
                name: (*name).into(),
                available: true,
                ..Default::default()
            })
            .collect(),
    };
    let frame = |request_id, page| ipc::GuiFrame {
        request_id,
        msg: Some(Msg::AudioDevices(page)),
        ..Default::default()
    };
    vm.apply(frame(id + 1, page(1, true, &["uncorrelated"])));
    assert!(vm.audio.devices.is_empty());
    vm.apply(frame(id, page(2, false, &["one"])));
    assert!(vm.inflight.iter().any(|pending| pending.id == id));
    vm.apply(frame(id, page(2, true, &["two"])));
    assert_eq!(vm.audio.devices[0].devices.len(), 2);
    assert!(vm.audio.devices[0].complete);
    assert!(!vm.inflight.iter().any(|pending| pending.id == id));
    vm.audio_action(
        &AudioAction::ListDevices(ipc::AudioDeviceKind::Output),
        Instant::now(),
    );
    let id = vm.take_outgoing()[0].0;
    vm.apply(frame(id, page(3, false, &["new"])));
    vm.apply(frame(id, page(2, true, &["stale"])));
    assert_eq!(vm.audio.devices[0].devices[0].id, "new");
    assert!(!vm.audio.devices[0].complete);
    vm.audio_action(
        &AudioAction::ListDevices(ipc::AudioDeviceKind::Output),
        Instant::now(),
    );
    let id = vm.take_outgoing()[0].0;
    let names: Vec<String> = (0..64).map(|index| format!("device-{index}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    vm.apply(frame(id, page(4, false, &names)));
    vm.apply(frame(id, page(4, true, &["overflow"])));
    assert_eq!(vm.audio.devices[0].devices.len(), 64);
    assert!(!vm.audio.devices[0].complete);
    assert!(vm.audio.error.is_some());
}

fn sharing_snapshot(revision: u64, role: ipc::AudioRole, requested: bool) -> ipc::AudioStatus {
    let mut status = snapshot(revision);
    status.sessions.clear();
    status.role = role as i32;
    status.system_sharing_requested = Some(requested);
    let config = status.config.as_mut().unwrap();
    config.role = role as i32;
    config.system_sharing = Some(true);
    config.listener_node_id = if role == ipc::AudioRole::Source {
        vec![2; 32]
    } else {
        Vec::new()
    };
    status
}

#[test]
fn system_audio_off_supersedes_pending_on_and_ignores_late_ack_and_on_snapshot() {
    for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
        let mut vm = view();
        vm.apply(event(Msg::AudioStatus(sharing_snapshot(5, role, false))));
        let on = if role == ipc::AudioRole::Source {
            AudioAction::Share(true)
        } else {
            AudioAction::Receive(true)
        };
        let off = if role == ipc::AudioRole::Source {
            AudioAction::Share(false)
        } else {
            AudioAction::Receive(false)
        };
        vm.audio_action(&on, Instant::now());
        let (old_id, _) = vm.take_outgoing().pop().unwrap();
        vm.audio.available = false;
        vm.audio_action(&off, Instant::now());
        let (off_id, message) = vm.take_outgoing().pop().expect("OFF is urgent");
        assert!(matches!(&message, Msg::SetSystemAudioSharing(request) if !request.enabled));
        assert!(!vm.audio.sharing_requested(role));
        for ok in [true, false] {
            vm.audio_result(
                old_id,
                audio::AudioRequest::Mutation,
                ipc::OpResult {
                    ok,
                    error_code: "AUDIO_CONSENT_REQUIRED".into(),
                },
            );
            assert!(vm.audio.pending);
            assert!(!vm.audio.sharing_requested(role));
            assert!(vm.audio.error.is_none());
        }
        vm.apply(event(Msg::AudioStatus(sharing_snapshot(6, role, true))));
        assert!(vm.audio.pending, "late ON snapshot cannot settle OFF");
        assert!(!vm.audio.sharing_requested(role));
        vm.audio_result(
            off_id,
            audio::AudioRequest::Mutation,
            ipc::OpResult {
                ok: true,
                error_code: String::new(),
            },
        );
        let (snapshot_id, _) = vm.take_outgoing().pop().expect("post-ACK read");
        vm.apply(support::answer(
            snapshot_id,
            Msg::AudioStatus(sharing_snapshot(7, role, false)),
        ));
        assert!(!vm.audio.pending);
        assert!(!vm.audio.sharing_requested(role));
        assert!(!vm.audio_player.open);
    }
}

#[test]
fn system_audio_failures_rollback_intent_and_report_async_failure_without_fake_activity() {
    for persisted in [false, true] {
        let mut vm = view();
        vm.apply(event(Msg::AudioStatus(sharing_snapshot(
            5,
            ipc::AudioRole::Source,
            false,
        ))));
        vm.audio_action(&AudioAction::Share(true), Instant::now());
        let (id, _) = vm.take_outgoing().pop().unwrap();
        if persisted {
            vm.audio_result(
                id,
                audio::AudioRequest::Mutation,
                ipc::OpResult {
                    ok: true,
                    error_code: String::new(),
                },
            );
            let mut failed = sharing_snapshot(6, ipc::AudioRole::Source, false);
            failed.system_sharing_error = "AUDIO_CONSENT_REQUIRED".into();
            let (snapshot_id, _) = vm.take_outgoing().pop().expect("post-ACK read");
            vm.apply(support::answer(snapshot_id, Msg::AudioStatus(failed)));
        } else {
            vm.audio_result(
                id,
                audio::AudioRequest::Mutation,
                ipc::OpResult {
                    ok: false,
                    error_code: "AUDIO_CONSENT_REQUIRED".into(),
                },
            );
        }
        assert!(!vm.audio.pending);
        assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
        assert_eq!(
            vm.audio.error_message(vm.look.language),
            Some(ui_text::text(
                vm.look.language,
                ui_text::Key::AudioSystemConsentRequired
            ))
        );
        assert!(!vm.audio_player.open);
    }
}

#[test]
fn pending_or_failed_listener_paths_do_not_autoopen_until_worker_running() {
    let mut vm = view();
    let mut pending = snapshot(5);
    pending.sessions[0].state = ipc::AudioSessionState::PathValidation as i32;
    pending.sessions[0].media_info = None;
    bifrost_gui::audio_wire::decode_status(&pending).expect("valid pending pipeline");
    vm.apply(event(Msg::AudioStatus(pending)));
    assert!(!vm.audio_player.open);
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    assert!(vm.audio_player.open);
    vm.audio_player.open = false;
    let mut buffering = snapshot(7);
    buffering.sessions[0].state = ipc::AudioSessionState::Rebuffering as i32;
    vm.apply(event(Msg::AudioStatus(buffering)));
    vm.apply(event(Msg::AudioStatus(snapshot(8))));
    assert!(!vm.audio_player.open, "recovery must respect hidden player");
}

#[test]
fn system_audio_global_stop_lowers_requested_intent_without_microphone_or_player_side_effects() {
    let mut vm = view();
    let mut status = sharing_snapshot(5, ipc::AudioRole::Listener, true);
    status
        .config
        .as_mut()
        .unwrap()
        .microphone
        .as_mut()
        .unwrap()
        .control_enabled = true;
    status.microphone.as_mut().unwrap().control_active = true;
    status.microphone.as_mut().unwrap().muted = false;
    status.microphone.as_mut().unwrap().ptt_pressed = true;
    vm.apply(event(Msg::AudioStatus(status.clone())));
    vm.audio.gui_ptt_held = true;
    vm.audio_action(&AudioAction::Receive(true), Instant::now());
    let (on_id, _) = vm.take_outgoing().pop().unwrap();
    vm.audio_action(&AudioAction::StopSystemAudio, Instant::now());
    let (_, stop) = vm.take_outgoing().pop().unwrap();
    assert!(matches!(stop, Msg::SetSystemAudioSharing(request) if !request.enabled));
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Listener));
    assert!(vm.audio.gui_ptt_held);
    assert_eq!(vm.audio.status.as_ref().unwrap(), &status);
    vm.audio_result(
        on_id,
        audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: true,
            error_code: String::new(),
        },
    );
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Listener));
    assert!(vm.audio.pending);
}

#[test]
fn system_audio_snapshot_before_ack_does_not_hide_current_save_failure() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(sharing_snapshot(
        5,
        ipc::AudioRole::Source,
        false,
    ))));
    vm.audio_action(&AudioAction::Share(true), Instant::now());
    let (id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(event(Msg::AudioStatus(sharing_snapshot(
        6,
        ipc::AudioRole::Source,
        false,
    ))));
    assert!(vm.audio.pending);
    assert!(vm.audio.sharing_requested(ipc::AudioRole::Source));
    vm.audio_result(
        id,
        audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: false,
            error_code: "AUDIO_CONFIG_WRITE".into(),
        },
    );
    assert!(!vm.audio.pending);
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
    assert_eq!(
        vm.audio.error_message(vm.look.language),
        Some(ui_text::text(
            vm.look.language,
            ui_text::Key::AudioConfigWrite
        ))
    );
    assert!(!vm.audio_player.open);
}

#[test]
fn legacy_unknown_receive_intent_displays_only_authorized_running_pipeline_and_explicit_off_wins() {
    for requested in [None, Some(false), Some(true)] {
        let mut vm = view();
        let mut status = snapshot(5);
        status.system_sharing_requested = requested;
        bifrost_gui::audio_wire::decode_status(&status).expect("valid observed pipeline");
        vm.apply(event(Msg::AudioStatus(status)));
        assert_eq!(
            vm.audio.sharing_requested(ipc::AudioRole::Listener),
            requested != Some(false)
        );
        assert_eq!(
            vm.audio
                .status
                .as_ref()
                .unwrap()
                .config
                .as_ref()
                .unwrap()
                .system_sharing,
            None
        );
    }
    let mut vm = view();
    let mut status = snapshot(5);
    status.sessions[0].state = ipc::AudioSessionState::PathValidation as i32;
    status.sessions[0].media_info = None;
    bifrost_gui::audio_wire::decode_status(&status).expect("valid pending pipeline");
    vm.apply(event(Msg::AudioStatus(status)));
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Listener));
}

#[test]
fn urgent_card_mute_ack_cannot_replace_pending_system_intent_persistence_result() {
    let mut vm = view();
    let mut status = sharing_snapshot(5, ipc::AudioRole::Listener, false);
    status.sessions = snapshot(5).sessions;
    bifrost_gui::audio_wire::decode_status(&status).expect("valid retained active stream");
    vm.apply(event(Msg::AudioStatus(status)));
    vm.audio_action(&AudioAction::Receive(true), Instant::now());
    let (system_id, _) = vm.take_outgoing().pop().unwrap();
    vm.audio_action(
        &AudioAction::Player(AudioPlayerAction::SetMuted {
            stream_id: "01".repeat(16),
            muted: true,
        }),
        Instant::now(),
    );
    let (mute_id, message) = vm.take_outgoing().pop().expect("urgent card mute");
    assert!(matches!(message, Msg::SetAudioPeerGain(request) if request.muted));
    vm.apply(support::answer(
        mute_id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }),
    ));
    assert!(vm.audio.pending);
    assert!(vm.audio.sharing_requested(ipc::AudioRole::Listener));
    vm.apply(support::answer(
        system_id,
        Msg::OpResult(ipc::OpResult {
            ok: false,
            error_code: "AUDIO_CONFIG_WRITE".into(),
        }),
    ));
    assert!(!vm.audio.pending);
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Listener));
    assert_eq!(
        vm.audio.error_message(vm.look.language),
        Some(ui_text::text(
            vm.look.language,
            ui_text::Key::AudioConfigWrite
        ))
    );
}
