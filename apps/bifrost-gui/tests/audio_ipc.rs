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
                seek_min_ms: None,
                seek_max_ms: None,
                session_token: 7,
                playback: model::PlaybackState::Playing,
                commands: model::MediaCommands {
                    pause: true,
                    next: true,
                    ..Default::default()
                },
                revision: 3,
                artwork_id: None,
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

fn draw_audio_player(
    ctx: &bifrost_gui::egui::Context,
    vm: &bifrost_gui::view_model::ViewModel,
    events: Vec<bifrost_gui::egui::Event>,
) -> (bifrost_gui::egui::FullOutput, Vec<AudioPlayerAction>) {
    use bifrost_gui::{egui, ui};
    let mut actions = Vec::new();
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(
                    ui::audio_player::DEFAULT_WIDTH,
                    ui::audio_player::height(&vm.audio_player.cards),
                ),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            actions = ui::audio_player::draw(
                ui,
                &vm.audio_player.cards,
                vm.look.theme,
                vm.look.language,
                false,
                vm.audio.player_mode(vm.connection.is_connected()),
            )
        },
    );
    (output, actions)
}

fn seekable_snapshot(revision: u64) -> ipc::AudioStatus {
    let mut status = snapshot(revision);
    let media = status.sessions[0].media_info.as_mut().unwrap();
    media.position_ms = Some(2_000);
    media.duration_ms = Some(10_000);
    media.seek_min_ms = Some(3_000);
    media.seek_max_ms = Some(7_000);
    media.commands.as_mut().unwrap().seek = true;
    status
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
    assert!(
        vm.audio.pending,
        "running telemetry is not a media-command observation"
    );
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(support::answer(read_id, Msg::AudioStatus(snapshot(6))));
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
        let (id, message) = vm.take_outgoing().pop().unwrap();
        let Msg::SetAudio(request) = message else {
            panic!("expected SetAudio");
        };
        assert_eq!(request.config, Some(draft.clone()));
        status.revision += 1;
        status.config = request.config;
        vm.audio_result(
            id,
            audio::AudioRequest::Mutation,
            ipc::OpResult {
                ok: true,
                ..Default::default()
            },
        );
        let (read_id, _) = vm.take_outgoing().pop().unwrap();
        vm.apply(support::answer(read_id, Msg::AudioStatus(status)));
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
fn unsupported_and_timeout_release_pending_without_losing_unsaved_draft() {
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
    assert!(vm.audio.draft.background_mute && vm.audio.dirty);
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

fn running_source_snapshot(revision: u64, target: [u8; 32]) -> ipc::AudioStatus {
    let mut status = sharing_snapshot(revision, ipc::AudioRole::Source, true);
    status.config.as_mut().unwrap().listener_node_id = target.to_vec();
    let mut session = snapshot(revision).sessions.remove(0);
    session.peer = target.to_vec();
    session.local_role = ipc::AudioRole::Source as i32;
    session.media_info = None;
    session.capture_active = Some(true);
    session.sending = Some(true);
    session.consent = Some(ipc::AudioSessionConsent {
        local: Some(ipc::AudioConsentFlags {
            allow_system_send: true,
            ..Default::default()
        }),
        remote: Some(ipc::AudioConsentFlags {
            allow_system_receive: true,
            ..Default::default()
        }),
    });
    status.sessions.push(session);
    bifrost_gui::audio_wire::decode_status(&status).expect("valid observed Source pipeline");
    status
}

#[test]
fn source_retarget_uses_one_sparse_request_and_reconciles_only_authoritative_target() {
    let a = [2; 32];
    let b = [3; 32];
    for outcome in ["reject", "success", "wrong-target", "postpersist-failure"] {
        let mut vm = view();
        vm.apply(event(Msg::AudioStatus(running_source_snapshot(5, a))));
        let original_config = vm.audio.draft.clone();
        vm.audio_action(&AudioAction::Retarget(b), Instant::now());
        let mut commands = vm.take_outgoing();
        assert_eq!(commands.len(), 1);
        let (id, message) = commands.remove(0);
        assert!(matches!(message, Msg::SetSystemAudioSharing(request)
            if request.enabled && request.role == ipc::AudioRole::Source as i32
                && request.listener_node_id == b && request.expected_revision == 5));
        assert!(vm.audio.pending && vm.audio.retargeting);
        assert_eq!(vm.audio.draft.listener_node_id, b);
        assert_eq!(
            vm.audio.status.as_ref().unwrap().config.as_ref().unwrap(),
            &original_config
        );

        vm.apply(event(Msg::AudioStatus(running_source_snapshot(6, a))));
        vm.audio_result(
            id + 100,
            audio::AudioRequest::Mutation,
            ipc::OpResult {
                ok: true,
                error_code: String::new(),
            },
        );
        assert!(vm.audio.pending && vm.audio.retargeting);
        assert_eq!(
            vm.audio.draft.listener_node_id, b,
            "queued A status cannot replace selected B"
        );
        assert_eq!(vm.audio.status.as_ref().unwrap().sessions[0].peer, a);

        if outcome == "reject" {
            vm.apply(support::answer(
                id,
                Msg::OpResult(ipc::OpResult {
                    ok: false,
                    error_code: "AUDIO_CONFIG_WRITE".into(),
                }),
            ));
            assert!(!vm.audio.pending && !vm.audio.retargeting);
            assert_eq!(vm.audio.draft.listener_node_id, a);
            assert!(vm.audio.sharing_requested(ipc::AudioRole::Source));
            let (read_id, message) = vm
                .take_outgoing()
                .pop()
                .expect("fresh failure reconciliation");
            assert!(matches!(message, Msg::GetAudioStatus(_)));
            let mut same_revision = running_source_snapshot(6, a);
            same_revision.sessions[0].latency_estimate_ms = Some(77);
            vm.apply(support::answer(read_id, Msg::AudioStatus(same_revision)));
            assert_eq!(
                vm.audio.status.as_ref().unwrap().sessions[0].latency_estimate_ms,
                Some(77)
            );
            assert!(vm.audio.sharing_requested(ipc::AudioRole::Source));
            assert_eq!(
                vm.audio.error_message(vm.look.language),
                Some(ui_text::text(
                    vm.look.language,
                    ui_text::Key::AudioConfigWrite
                ))
            );
        } else {
            vm.apply(support::answer(
                id,
                Msg::OpResult(ipc::OpResult {
                    ok: true,
                    error_code: String::new(),
                }),
            ));
            let (read_id, message) = vm.take_outgoing().pop().expect("fresh post-ACK read");
            assert!(matches!(message, Msg::GetAudioStatus(_)));
            let mut authoritative =
                running_source_snapshot(7, if outcome == "wrong-target" { a } else { b });
            if outcome == "postpersist-failure" {
                authoritative.system_sharing_requested = Some(false);
                authoritative.system_sharing_error = "AUDIO_DEVICE_UNAVAILABLE".into();
                authoritative.sessions.clear();
            }
            vm.apply(support::answer(read_id, Msg::AudioStatus(authoritative)));
            if outcome == "wrong-target" {
                assert!(vm.audio.pending && vm.audio.retargeting);
                assert_eq!(vm.audio.draft.listener_node_id, b);
                assert_eq!(vm.audio.status.as_ref().unwrap().sessions[0].peer, a);
                vm.audio_action(&AudioAction::Share(false), Instant::now());
                let (_, off) = vm.take_outgoing().pop().expect("OFF remains actionable");
                assert!(matches!(off, Msg::SetSystemAudioSharing(request) if !request.enabled));
            } else {
                assert!(!vm.audio.pending && !vm.audio.retargeting);
                assert_eq!(vm.audio.draft.listener_node_id, b);
                if outcome == "postpersist-failure" {
                    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
                    assert!(vm.audio.status.as_ref().unwrap().sessions.is_empty());
                    assert_eq!(
                        vm.audio.error_message(vm.look.language),
                        Some(ui_text::text(
                            vm.look.language,
                            ui_text::Key::AudioDeviceUnavailable
                        ))
                    );
                } else {
                    assert!(vm.audio.sharing_requested(ipc::AudioRole::Source));
                    assert_eq!(vm.audio.status.as_ref().unwrap().sessions[0].peer, b);
                }
            }
        }
        assert!(
            vm.take_outgoing().is_empty(),
            "no implicit activation or retry of A"
        );
    }
}

#[test]
fn hiding_and_showing_player_preserve_running_audio_without_any_ipc() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    assert!(vm.audio_player.open);
    let before = vm.audio.status.clone();
    vm.local(&bifrost_gui::view_model::Action::Audio(
        AudioAction::Player(AudioPlayerAction::HidePlayer),
    ));
    assert!(!vm.audio_player.open);
    assert_eq!(vm.audio.status, before);
    assert!(
        vm.take_outgoing().is_empty(),
        "Hide must not stop or reconfigure audio"
    );
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    assert!(
        !vm.audio_player.open,
        "an existing stream must respect Hide"
    );
    vm.local(&bifrost_gui::view_model::Action::OpenAudioPlayer);
    assert!(vm.audio_player.open);
    assert!(
        vm.take_outgoing().is_empty(),
        "Show is local presentation only"
    );
    assert_eq!(
        vm.audio.status.as_ref().unwrap().sessions,
        before.unwrap().sessions
    );
}

#[test]
fn paused_media_keeps_the_real_card_and_resumes_from_its_current_play_button() {
    use bifrost_gui::{egui, ui};
    use ui_text::{text, Key};

    let mut vm = view();
    let mut playing = snapshot(5);
    let media = playing.sessions[0].media_info.as_mut().unwrap();
    media.position_ms = Some(5_000);
    media.duration_ms = Some(90_000);
    media.commands.as_mut().unwrap().play = true;
    vm.apply(event(Msg::AudioStatus(playing)));
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    let draw =
        |vm: &bifrost_gui::view_model::ViewModel, events| draw_audio_player(&ctx, vm, events);
    draw(&vm, Vec::new()).0.drop_without_applying_deltas();
    for (key, expected, revision) in [
        (Key::AudioPause, ipc::AudioMediaCommand::Pause, 3),
        (Key::AudioPlay, ipc::AudioMediaCommand::Play, 4),
    ] {
        let (output, _) = draw(&vm, Vec::new());
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(vm.look.language, key)))
            .expect("visible current playback control")
            .1
            .bounds()
            .unwrap();
        let at = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        output.drop_without_applying_deltas();
        let (output, actions) = draw(
            &vm,
            vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Default::default(),
                },
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                },
            ],
        );
        output.drop_without_applying_deltas();
        assert_eq!(actions, vec![AudioPlayerAction::PlayPause("01".repeat(16))]);
        vm.audio_action(
            &AudioAction::Player(actions.into_iter().next().unwrap()),
            Instant::now(),
        );
        let mut outgoing = vm.take_outgoing();
        assert_eq!(
            outgoing.len(),
            1,
            "one media command, never StopAudio or system OFF"
        );
        let (id, message) = outgoing.remove(0);
        assert!(matches!(message, Msg::SendMediaCommand(command)
            if command.command == expected as i32 && command.session_token == 7
                && command.media_revision == revision && command.session_id == vec![1; 16]
                && command.peer_node_id == vec![2; 32] && command.position_ms.is_none()));
        if key == Key::AudioPause {
            vm.apply(support::answer(
                id,
                Msg::OpResult(ipc::OpResult {
                    ok: true,
                    error_code: String::new(),
                }),
            ));
            let mut paused = snapshot(6);
            let media = paused.sessions[0].media_info.as_mut().unwrap();
            media.playback = ipc::AudioPlaybackState::Paused as i32;
            media.revision = 4;
            media.position_ms = Some(5_000);
            media.duration_ms = Some(90_000);
            media.commands.as_mut().unwrap().play = true;
            let (read_id, message) = vm.take_outgoing().pop().expect("fresh status read");
            assert!(matches!(message, Msg::GetAudioStatus(_)));
            vm.apply(support::answer(read_id, Msg::AudioStatus(paused)));
            assert_eq!(vm.audio_player.cards.len(), 1, "Pause is not a stream stop");
            assert!(vm.audio_player.open && !vm.audio_player.cards[0].playing);
            assert!(
                vm.audio_player.cards[0].has_media_session
                    && vm.audio_player.cards[0].can_play_pause
            );
            assert_eq!(
                vm.audio_player.cards[0].title.as_deref(),
                Some("Private title")
            );
        }
    }
}

#[test]
fn real_track_click_dispatches_one_guarded_seek_without_optimistic_playback_or_retry() {
    use bifrost_gui::{egui, ui};
    use ui_text::{text, Key};
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(seekable_snapshot(5))));
    let before = vm.audio_player.cards.clone();
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    draw_audio_player(&ctx, &vm, Vec::new())
        .0
        .drop_without_applying_deltas();
    let (output, _) = draw_audio_player(&ctx, &vm, Vec::new());
    let tree = output.platform_output.accesskit_update.as_ref().unwrap();
    let bounds = tree
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(text(vm.look.language, Key::AudioSeek)))
        .expect("authorized track seek hit target")
        .1
        .bounds()
        .unwrap();
    let at = egui::pos2(
        bounds.x1 as f32 - 1.0,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    output.drop_without_applying_deltas();
    let (output, actions) = draw_audio_player(
        &ctx,
        &vm,
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ],
    );
    output.drop_without_applying_deltas();
    assert_eq!(actions.len(), 1);
    let action = AudioAction::Player(actions[0].clone());
    vm.audio_action(&action, Instant::now());
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1);
    assert!(matches!(&outgoing[0].1, Msg::SendMediaCommand(command)
        if command.command == ipc::AudioMediaCommand::Seek as i32
            && command.position_ms == Some(7_000)
            && command.session_token == 7 && command.media_revision == 3
            && command.session_id == vec![1; 16] && command.peer_node_id == vec![2; 32]));
    assert_eq!(
        vm.audio_player.cards, before,
        "ACK/intent must not invent playback movement"
    );
    assert!(vm.audio.pending);
    vm.audio_action(&action, Instant::now());
    assert!(
        vm.take_outgoing().is_empty(),
        "reuse global pending, no seek queue/retry"
    );
    vm.apply(support::answer(
        outgoing[0].0,
        Msg::OpResult(ipc::OpResult {
            ok: false,
            error_code: "AUDIO_STALE_MEDIA".into(),
        }),
    ));
    assert!(!vm.audio.pending);
    assert!(vm.audio.error.is_some());
    assert_eq!(vm.audio_player.cards, before);
    assert!(
        vm.take_outgoing().is_empty(),
        "failure does not rebase or retry"
    );
}

#[test]
fn queued_track_seek_rechecks_current_identity_range_and_permission_before_dispatch() {
    use bifrost_gui::audio_player::AudioSeekTarget;
    let action = AudioPlayerAction::Seek {
        stream_id: "01".repeat(16),
        target: AudioSeekTarget {
            session_token: 7,
            media_revision: 3,
            min_ms: 3_000,
            max_ms: 7_000,
        },
        position_ms: 5_000,
    };
    for change in [
        "none",
        "disabled",
        "session",
        "token",
        "revision",
        "media-grant",
        "send-grant",
        "receive-grant",
        "capability",
        "system-only",
        "unknown-position",
        "unknown-duration",
        "minimum",
        "maximum",
        "range-unavailable",
    ] {
        let mut status = seekable_snapshot(5);
        match change {
            "disabled" => status.config.as_mut().unwrap().enabled = false,
            "session" => status.sessions[0].session_id = vec![9; 16],
            "media-grant" => {
                status.sessions[0]
                    .consent
                    .as_mut()
                    .unwrap()
                    .remote
                    .as_mut()
                    .unwrap()
                    .allow_media_control = false
            }
            "send-grant" => {
                status.sessions[0]
                    .consent
                    .as_mut()
                    .unwrap()
                    .remote
                    .as_mut()
                    .unwrap()
                    .allow_system_send = false
            }
            "receive-grant" => {
                status.sessions[0]
                    .consent
                    .as_mut()
                    .unwrap()
                    .local
                    .as_mut()
                    .unwrap()
                    .allow_system_receive = false
            }
            "system-only" => status.sessions[0].media_info = None,
            _ => {
                let media = status.sessions[0].media_info.as_mut().unwrap();
                match change {
                    "token" => media.session_token += 1,
                    "revision" => media.revision += 1,
                    "capability" => media.commands.as_mut().unwrap().seek = false,
                    "unknown-position" => media.position_ms = None,
                    "unknown-duration" => media.duration_ms = None,
                    "minimum" => media.seek_min_ms = Some(3_001),
                    "maximum" => media.seek_max_ms = Some(6_999),
                    "range-unavailable" => {
                        media.seek_min_ms = None;
                        media.seek_max_ms = None;
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            audio::player_request(&status, &action).is_some(),
            change == "none",
            "{change}"
        );
    }
    for position_ms in [0, 2_999, 7_001, u64::MAX] {
        let mut outside = action.clone();
        if let AudioPlayerAction::Seek {
            position_ms: value, ..
        } = &mut outside
        {
            *value = position_ms;
        }
        assert!(audio::player_request(&seekable_snapshot(5), &outside).is_none());
    }
}

#[test]
fn artwork_is_requested_once_per_session_token_and_hash() {
    let mut vm = view();
    let mut status = snapshot(1);
    let media = status.sessions[0].media_info.as_mut().unwrap();
    media.artwork_id = vec![7; 32];
    media.session_token = 7;
    let session_id = status.sessions[0].session_id.clone();
    vm.apply_audio_status(0, status.clone());
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    match &outgoing[0].1 {
        Msg::GetAudioArtwork(request) => {
            assert_eq!(request.session_id, session_id);
            assert_eq!(request.session_token, 7);
            assert_eq!(request.artwork_id, vec![7; 32]);
        }
        other => panic!("{other:?}"),
    }

    // A newer revision or position never re-requests the same current-scope tuple.
    status.revision += 1;
    status.sessions[0].media_info.as_mut().unwrap().position_ms = Some(1_000);
    vm.apply_audio_status(0, status.clone());
    assert!(vm.take_outgoing().is_empty());

    // A new token scopes a new image: exactly one request, no retry loop.
    status.revision += 1;
    status.sessions[0]
        .media_info
        .as_mut()
        .unwrap()
        .session_token = 8;
    vm.apply_audio_status(0, status.clone());
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    assert!(matches!(&outgoing[0].1, Msg::GetAudioArtwork(request) if request.session_token == 8));

    // An empty id is honest "no image": no request, and the fallback stays.
    status.revision += 1;
    status.sessions[0]
        .media_info
        .as_mut()
        .unwrap()
        .artwork_id
        .clear();
    vm.apply_audio_status(0, status);
    assert!(vm.take_outgoing().is_empty());
    assert!(vm.audio.artwork.is_empty());
}

#[test]
fn player_mode_changes_only_authoritative_config_and_preserves_unsaved_advanced_draft() {
    let mut vm = view();
    vm.connection = bifrost_gui::view_model::Connection::Connected;
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let authoritative = vm.audio.status.as_ref().unwrap().config.clone().unwrap();
    let mut draft = authoritative.clone();
    draft.background_gain_db = -37;
    draft.output_device = "unsaved-output".into();
    draft.microphone.as_mut().unwrap().muted = false;
    vm.audio_action(
        &AudioAction::EditConfig(Box::new(draft.clone())),
        Instant::now(),
    );
    let ctx = bifrost_gui::egui::Context::default();
    bifrost_gui::ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    draw_audio_player(&ctx, &vm, Vec::new())
        .0
        .drop_without_applying_deltas();
    let (output, _) = draw_audio_player(&ctx, &vm, Vec::new());
    let bounds = output
        .platform_output
        .accesskit_update
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .find(|(_, node)| {
            node.label()
                == Some(ui_text::text(
                    vm.look.language,
                    ui_text::Key::AudioFollowFocus,
                ))
        })
        .unwrap()
        .1
        .bounds()
        .unwrap();
    let at = bifrost_gui::egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    output.drop_without_applying_deltas();
    let (_, actions) = draw_audio_player(
        &ctx,
        &vm,
        vec![
            bifrost_gui::egui::Event::PointerMoved(at),
            bifrost_gui::egui::Event::PointerButton {
                pos: at,
                button: bifrost_gui::egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            bifrost_gui::egui::Event::PointerButton {
                pos: at,
                button: bifrost_gui::egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ],
    );
    assert_eq!(
        actions,
        [AudioPlayerAction::SetMode(ipc::AudioMode::MixAll)]
    );
    vm.audio_action(&AudioAction::Player(actions[0].clone()), Instant::now());
    let outgoing = vm.take_outgoing();
    let mut expected = authoritative.clone();
    expected.mode = ipc::AudioMode::MixAll as i32;
    assert_eq!(outgoing.len(), 1);
    assert!(matches!(&outgoing[0].1, Msg::SetAudio(request)
        if request.expected_revision == 5 && request.config.as_ref() == Some(&expected)));
    assert!(vm.audio.pending);
    assert_eq!(
        vm.audio.player_mode(true).current,
        Some(ipc::AudioMode::FollowFocus)
    );
    assert!(!vm.audio.player_mode(true).enabled);
    assert_eq!(
        vm.audio.status.as_ref().unwrap().config.as_ref(),
        Some(&authoritative)
    );
    assert_eq!(vm.audio.draft, draft);
    vm.audio_result(
        outgoing[0].0,
        audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: true,
            ..Default::default()
        },
    );
    assert_eq!(
        vm.audio.status.as_ref().unwrap().config.as_ref(),
        Some(&authoritative)
    );
    assert!(vm.audio.pending, "persistence ACK is not a published mode");
    let mut published = snapshot(6);
    published.config = Some(expected.clone());
    published.mode = expected.mode;
    vm.apply(event(Msg::AudioStatus(published)));
    assert!(!vm.audio.pending);
    assert_eq!(
        vm.audio.status.as_ref().unwrap().config.as_ref(),
        Some(&expected)
    );
    assert_eq!(vm.audio.draft, draft);
    assert!(vm.audio.dirty);
}

#[test]
fn rejected_mode_cas_preserves_current_policy_and_draft_and_ignores_unrelated_results() {
    let mut vm = view();
    vm.connection = bifrost_gui::view_model::Connection::Connected;
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let mut draft = vm.audio.draft.clone();
    draft.output_device = "unsaved".into();
    vm.audio_action(
        &AudioAction::EditConfig(Box::new(draft.clone())),
        Instant::now(),
    );
    vm.audio_action(
        &AudioAction::Player(AudioPlayerAction::SetMode(ipc::AudioMode::MixAll)),
        Instant::now(),
    );
    let id = vm.take_outgoing()[0].0;
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    assert!(
        vm.audio.pending,
        "unrelated telemetry is not the requested mode"
    );
    vm.audio_result(
        id + 1,
        audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: false,
            error_code: "AUDIO_REVISION_MISMATCH".into(),
        },
    );
    assert!(vm.audio.pending);
    vm.audio_result(
        id,
        audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: false,
            error_code: "AUDIO_REVISION_MISMATCH".into(),
        },
    );
    assert!(!vm.audio.pending);
    assert_eq!(
        vm.audio.player_mode(true).current,
        Some(ipc::AudioMode::FollowFocus)
    );
    assert_eq!(vm.audio.draft, draft);
    assert!(vm.audio.dirty && vm.audio.error.is_some());
    assert!(
        vm.take_outgoing().is_empty(),
        "no retry/rebase or stream stop"
    );
}

#[test]
fn mode_control_is_global_and_read_only_when_unknown_disconnected_source_or_pending() {
    for state in [
        "unknown",
        "legacy",
        "disconnected",
        "source",
        "pending",
        "available",
    ] {
        let mut vm = view();
        vm.connection = bifrost_gui::view_model::Connection::Connected;
        vm.apply(event(Msg::AudioStatus(snapshot(5))));
        match state {
            "unknown" => {
                vm.audio
                    .status
                    .as_mut()
                    .unwrap()
                    .config
                    .as_mut()
                    .unwrap()
                    .mode = 99
            }
            "legacy" => vm.audio.status.as_mut().unwrap().config = None,
            "disconnected" => {
                vm.connection =
                    bifrost_gui::view_model::Connection::Disconnected { why: String::new() }
            }
            "source" => vm.audio.status.as_mut().unwrap().role = ipc::AudioRole::Source as i32,
            "pending" => vm.audio.pending = true,
            _ => {}
        }
        let mut second = vm.audio_player.cards[0].clone();
        second.stream_id = "second".into();
        vm.audio_player.cards.push(second);
        let mode = vm.audio.player_mode(vm.connection.is_connected());
        assert_eq!(mode.enabled, state == "available");
        assert_eq!(
            mode.current.is_none(),
            matches!(state, "unknown" | "legacy")
        );
        let ctx = bifrost_gui::egui::Context::default();
        bifrost_gui::ui::install_fonts(&ctx);
        ctx.enable_accesskit();
        draw_audio_player(&ctx, &vm, Vec::new())
            .0
            .drop_without_applying_deltas();
        let (output, actions) = draw_audio_player(&ctx, &vm, Vec::new());
        assert!(actions.is_empty());
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        let names = [
            ui_text::Key::AudioMode,
            ui_text::Key::AudioMixAll,
            ui_text::Key::AudioFollowFocus,
        ]
        .map(|key| ui_text::text(vm.look.language, key));
        let controls: Vec<_> = tree
            .nodes
            .iter()
            .filter(|(_, node)| names.contains(&node.label().unwrap_or("")))
            .collect();
        assert_eq!(
            controls.len(),
            1,
            "one Listener policy control, not one command per card"
        );
        assert_eq!(controls[0].1.is_disabled(), state != "available");
        output.drop_without_applying_deltas();
        vm.audio_action(
            &AudioAction::Player(AudioPlayerAction::SetMode(ipc::AudioMode::MixAll)),
            Instant::now(),
        );
        assert_eq!(
            vm.take_outgoing()
                .iter()
                .filter(|(_, message)| matches!(message, Msg::SetAudio(_)))
                .count(),
            usize::from(state == "available")
        );
    }
}

#[test]
fn mode_meaning_tooltip_wraps_inside_the_actual_player_client() {
    use bifrost_gui::{egui, ui};
    for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
        for mode in [ipc::AudioMode::MixAll, ipc::AudioMode::FollowFocus] {
            let (mode_key, hint_key) = if mode == ipc::AudioMode::MixAll {
                (
                    ui_text::Key::AudioMixAll,
                    ui_text::Key::AudioPlayerMixAllHint,
                )
            } else {
                (
                    ui_text::Key::AudioFollowFocus,
                    ui_text::Key::AudioPlayerFollowFocusHint,
                )
            };
            let mut vm = view();
            vm.look.language = lang;
            vm.connection = bifrost_gui::view_model::Connection::Connected;
            let mut status = snapshot(5);
            status.mode = mode as i32;
            status.config.as_mut().unwrap().mode = mode as i32;
            vm.apply(event(Msg::AudioStatus(status)));
            let ctx = egui::Context::default();
            ui::install_fonts(&ctx);
            ctx.enable_accesskit();
            let size = egui::vec2(428.0, 339.0);
            let mut at = None;
            let mut last = None;
            for step in 0..8 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        time: Some(f64::from(step) * 0.25),
                        events: if step == 2 {
                            at.map(egui::Event::PointerMoved).into_iter().collect()
                        } else {
                            Vec::new()
                        },
                        focused: true,
                        ..Default::default()
                    },
                    |ui| {
                        ui::audio_player::draw(
                            ui,
                            &vm.audio_player.cards,
                            vm.look.theme,
                            lang,
                            false,
                            vm.audio.player_mode(true),
                        );
                    },
                );
                output.textures_delta.clear();
                if at.is_none() {
                    at = output
                        .platform_output
                        .accesskit_update
                        .as_ref()
                        .and_then(|tree| {
                            tree.nodes
                                .iter()
                                .find(|(_, node)| {
                                    node.label() == Some(ui_text::text(lang, mode_key))
                                })
                                .and_then(|(_, node)| node.bounds())
                                .map(|bounds| {
                                    egui::pos2(
                                        ((bounds.x0 + bounds.x1) / 2.0) as f32,
                                        ((bounds.y0 + bounds.y1) / 2.0) as f32,
                                    )
                                })
                        });
                }
                last = Some(output);
            }
            fn text_bounds(shape: &egui::Shape, expected: &str) -> Option<egui::Rect> {
                match shape {
                    egui::Shape::Text(text) if text.galley.text() == expected => {
                        Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
                    }
                    egui::Shape::Vec(children) => children
                        .iter()
                        .find_map(|shape| text_bounds(shape, expected)),
                    _ => None,
                }
            }
            let output = last.unwrap();
            let tooltip_roots: Vec<_> = ctx.memory(|memory| {
                memory
                    .areas()
                    .visible_layer_ids()
                    .into_iter()
                    .filter(|layer| layer.order == egui::Order::Tooltip)
                    .collect()
            });
            assert_eq!(
                tooltip_roots.len(),
                1,
                "one mode control presents one tooltip container"
            );
            let client = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for root in tooltip_roots {
                let frame = egui::AreaState::load(&ctx, root.id).unwrap().rect();
                assert!(
                    client.contains_rect(frame),
                    "{lang:?}/{mode:?}: full tooltip frame clipped: {frame:?}"
                );
            }
            let heading = output
                .shapes
                .iter()
                .find_map(|shape| text_bounds(&shape.shape, ui_text::text(lang, mode_key)))
                .expect("the same tooltip identifies the current authoritative mode");
            assert!(heading.left() >= 0.0 && heading.right() <= size.x);
            let bounds = output
                .shapes
                .iter()
                .find_map(|shape| text_bounds(&shape.shape, ui_text::text(lang, hint_key)))
                .expect("hover shows the current meaning and the next action");
            assert!(
                bounds.left() >= 0.0 && bounds.right() <= size.x && bounds.bottom() <= size.y,
                "{lang:?}: tooltip clipped outside the real client: {bounds:?}"
            );
            output.drop_without_applying_deltas();
        }
    }
}

#[test]
fn player_ack_and_observation_are_separate_and_never_erase_advanced_drafts() {
    for operation in ["gain", "mute", "seek", "play"] {
        for ok in [false, true] {
            let mut vm = view();
            vm.apply(event(Msg::AudioStatus(seekable_snapshot(5))));
            let mut draft = vm.audio.draft.clone();
            draft.output_device = "unsaved-output".into();
            vm.audio_action(
                &AudioAction::EditConfig(Box::new(draft.clone())),
                Instant::now(),
            );
            let stream_id = "01".repeat(16);
            let action = match operation {
                "gain" => AudioPlayerAction::SetGain {
                    stream_id,
                    percent: 20,
                },
                "mute" => AudioPlayerAction::SetMuted {
                    stream_id,
                    muted: false,
                },
                "seek" => AudioPlayerAction::Seek {
                    stream_id,
                    target: vm.audio_player.cards[0].seek.unwrap(),
                    position_ms: 4_000,
                },
                _ => AudioPlayerAction::PlayPause(stream_id),
            };
            vm.audio_action(&AudioAction::Player(action), Instant::now());
            let (id, _) = vm.take_outgoing().pop().unwrap();
            vm.apply(event(Msg::AudioStatus(seekable_snapshot(6))));
            assert!(vm.audio.pending, "telemetry must not ACK {operation}");
            assert_eq!(vm.audio.draft, draft);
            vm.apply(support::answer(
                id,
                Msg::OpResult(ipc::OpResult {
                    ok,
                    error_code: if ok {
                        String::new()
                    } else {
                        "AUDIO_STALE_MEDIA".into()
                    },
                }),
            ));
            if ok {
                let (read_id, _) = vm.take_outgoing().pop().unwrap();
                let mut observed = seekable_snapshot(6);
                if operation == "gain" {
                    observed.sessions[0].gain_percent = 20;
                }
                vm.apply(support::answer(read_id, Msg::AudioStatus(observed)));
            } else {
                assert!(
                    vm.audio.error.is_some(),
                    "correlated rejection remains visible"
                );
            }
            assert!(!vm.audio.pending);
            assert!(vm.audio.dirty);
            assert_eq!(vm.audio.draft, draft, "{operation}, ok={ok}");
        }
    }
}

#[test]
fn gain_keeps_one_request_and_the_latest_desired_value_until_reconciliation() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let gain = |percent| {
        AudioAction::Player(AudioPlayerAction::SetGain {
            stream_id: "01".repeat(16),
            percent,
        })
    };
    vm.audio_action(&gain(60), Instant::now());
    let (first_id, first) = vm.take_outgoing().pop().unwrap();
    assert!(matches!(first, Msg::SetAudioPeerGain(value) if value.gain_percent == 60));
    for percent in [50, 40, 20] {
        vm.audio_action(&gain(percent), Instant::now());
    }
    assert!(vm.take_outgoing().is_empty());
    assert_eq!(vm.audio_player.cards[0].gain_percent, 65);
    assert_eq!(vm.audio_player.cards[0].desired_gain_percent, Some(20));
    vm.audio_action(
        &AudioAction::Player(AudioPlayerAction::SetMuted {
            stream_id: "01".repeat(16),
            muted: true,
        }),
        Instant::now(),
    );
    let (mute_id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(support::answer(
        mute_id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    vm.take_outgoing();
    assert!(vm.audio.pending, "urgent mute must not steal gain's ACK");
    vm.apply(support::answer(
        first_id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    let mut observed = snapshot(6);
    observed.sessions[0].gain_percent = 60;
    vm.apply(support::answer(read_id, Msg::AudioStatus(observed)));
    let outgoing = vm.take_outgoing();
    assert_eq!(outgoing.len(), 1);
    assert!(matches!(&outgoing[0].1, Msg::SetAudioPeerGain(value)
        if value.gain_percent == 20 && value.expected_revision == 6 && value.muted));
    let second_id = outgoing[0].0;
    vm.apply(support::answer(
        second_id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    let mut observed = snapshot(7);
    observed.sessions[0].gain_percent = 20;
    vm.apply(support::answer(read_id, Msg::AudioStatus(observed)));
    assert!(!vm.audio.pending);
    assert_eq!(vm.audio_player.cards[0].gain_percent, 20);
    assert_eq!(vm.audio_player.cards[0].desired_gain_percent, None);
    assert!(vm.take_outgoing().is_empty());
}

#[test]
fn receiving_failure_retains_the_selected_task_and_explicit_discard_restores_config() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(sharing_snapshot(
        5,
        ipc::AudioRole::Source,
        false,
    ))));
    let original = vm.audio.draft.clone();
    let mut draft = original.clone();
    draft.role = ipc::AudioRole::Listener as i32;
    draft.udp_port = 47904;
    vm.audio_action(
        &AudioAction::EditConfig(Box::new(draft.clone())),
        Instant::now(),
    );
    vm.audio_action(&AudioAction::Receive(true), Instant::now());
    let (id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(support::answer(
        id,
        Msg::OpResult(ipc::OpResult {
            ok: false,
            error_code: "AUDIO_DEVICE_UNAVAILABLE".into(),
        }),
    ));
    assert_eq!(vm.audio.draft, draft);
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Listener));
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
    vm.audio_action(&AudioAction::DiscardConfig, Instant::now());
    assert_eq!(vm.audio.draft, original);
    assert!(!vm.audio.dirty);
}

#[test]
fn delayed_post_ack_read_reconciles_without_rolling_back_newer_push_state() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    vm.audio_action(
        &AudioAction::Player(AudioPlayerAction::PlayPause("01".repeat(16))),
        Instant::now(),
    );
    let (id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(support::answer(
        id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(event(Msg::AudioStatus(snapshot(7))));
    assert!(vm.audio.pending);
    vm.apply(support::answer(read_id, Msg::AudioStatus(snapshot(6))));
    assert!(!vm.audio.pending);
    assert_eq!(vm.audio.status.as_ref().unwrap().revision, 7);
}

#[test]
fn gain_return_to_confirmed_value_survives_telemetry_before_the_pending_mutation() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    let gain = |percent| {
        AudioAction::Player(AudioPlayerAction::SetGain {
            stream_id: "01".repeat(16),
            percent,
        })
    };
    vm.audio_action(&gain(60), Instant::now());
    let (first_id, _) = vm.take_outgoing().pop().unwrap();
    vm.audio_action(&gain(65), Instant::now());
    assert!(vm.take_outgoing().is_empty());
    vm.apply(event(Msg::AudioStatus(snapshot(6))));
    assert!(vm.audio.pending);
    assert_eq!(vm.audio_player.cards[0].gain_percent, 65);
    assert_eq!(vm.audio_player.cards[0].desired_gain_percent, Some(65));
    vm.apply(support::answer(
        first_id,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    let mut observed = snapshot(7);
    observed.sessions[0].gain_percent = 60;
    vm.apply(support::answer(read_id, Msg::AudioStatus(observed)));
    let outgoing = vm.take_outgoing();
    assert_eq!(
        outgoing.len(),
        1,
        "returning to the original value still needs a final mutation"
    );
    assert!(matches!(&outgoing[0].1, Msg::SetAudioPeerGain(value)
        if value.gain_percent == 65 && value.expected_revision == 7));
    assert_eq!(vm.audio_player.cards[0].gain_percent, 60);
    assert_eq!(vm.audio_player.cards[0].desired_gain_percent, Some(65));
    vm.apply(support::answer(
        outgoing[0].0,
        Msg::OpResult(ipc::OpResult {
            ok: true,
            ..Default::default()
        }),
    ));
    let (read_id, _) = vm.take_outgoing().pop().unwrap();
    vm.apply(support::answer(read_id, Msg::AudioStatus(snapshot(8))));
    assert!(!vm.audio.pending);
    assert_eq!(vm.audio_player.cards[0].gain_percent, 65);
    assert_eq!(vm.audio_player.cards[0].desired_gain_percent, None);
    assert!(vm.take_outgoing().is_empty());
}
