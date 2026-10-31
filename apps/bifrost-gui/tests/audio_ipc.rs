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
    assert!(!audio::map_cards(&model, ui_text::Lang::En)[0].can_next);
    status.sessions[0].media_info = None;
    let cards = audio::map_cards(
        &bifrost_gui::audio_wire::decode_status(&status).unwrap(),
        ui_text::Lang::En,
    );
    assert!(!cards[0].has_media_session);
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
    vm.audio_action(&AudioAction::StopAll, now);
    vm.tick(now + Duration::from_secs(11));
    assert!(!vm.audio.pending && !vm.audio.available);
}

#[test]
fn explicit_group_creation_edits_only_the_listener_draft() {
    let mut vm = view();
    vm.apply(event(Msg::AudioStatus(snapshot(5))));
    vm.audio_action(&AudioAction::UseThisListener, Instant::now());
    assert_eq!(vm.audio.draft.group_id.len(), 16);
    assert!(vm.audio.dirty);
    assert_eq!(
        vm.audio
            .status
            .as_ref()
            .unwrap()
            .config
            .as_ref()
            .unwrap()
            .group_id,
        vec![9; 16]
    );
    assert!(vm.take_outgoing().is_empty());
    assert!(!format!("{:?}", vm.audio_player.cards[0]).contains("Private title"));
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
