//! Real paired Dry children: now-playing requires an Active, consented stream,
//! and successful remote control requires both a native result and changed metadata.

#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window};
use common::Unpaired;
use protocol::ipc::{self as wire, gui_frame};
use std::time::{Duration, Instant};

fn no_metadata(status: &wire::AudioStatus) -> bool {
    status
        .sessions
        .iter()
        .all(|session| session.media_info.is_none())
}

fn media(window: &mut Window, session_id: &[u8]) -> wire::MediaInfo {
    let status = window.wait(
        "current native now-playing is present for the active stream",
        |status| {
            status.sessions.iter().any(|session| {
                session.session_id == session_id
                    && session.state == wire::AudioSessionState::Running as i32
                    && session.media_info.is_some()
            })
        },
    );
    status
        .sessions
        .into_iter()
        .find(|session| session.session_id == session_id)
        .expect("current system-audio session")
        .media_info
        .expect("native current media")
}

fn opposite_command(info: &wire::MediaInfo) -> (wire::AudioMediaCommand, wire::AudioPlaybackState) {
    let supported = info.commands.as_ref().expect("native command capabilities");
    match wire::AudioPlaybackState::try_from(info.playback).expect("valid native playback state") {
        wire::AudioPlaybackState::Playing => {
            assert!(supported.pause, "Dry current session supports pause");
            (
                wire::AudioMediaCommand::Pause,
                wire::AudioPlaybackState::Paused,
            )
        }
        wire::AudioPlaybackState::Paused => {
            assert!(supported.play, "Dry current session supports play");
            (
                wire::AudioMediaCommand::Play,
                wire::AudioPlaybackState::Playing,
            )
        }
        state => panic!("Dry current session must be playing or paused, got {state:?}"),
    }
}

fn command(
    source_id: &[u8],
    session_id: &[u8],
    info: &wire::MediaInfo,
    command: wire::AudioMediaCommand,
) -> gui_frame::Msg {
    gui_frame::Msg::SendMediaCommand(wire::SendMediaCommand {
        peer_node_id: source_id.to_vec(),
        session_id: session_id.to_vec(),
        session_token: info.session_token,
        media_revision: info.revision,
        command: command as i32,
    })
}

fn denied(message: gui_frame::Msg, code: &str) {
    match message {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok, "media command must be refused");
            assert_eq!(result.error_code, code);
        }
        other => panic!("expected typed media operation result, got {other:?}"),
    }
}

#[test]
fn active_media_requires_separate_control_consent_executes_and_clears_on_stop() {
    let _guard = common::serial();
    let two = Unpaired::start_audio("audio-media-control");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let ports = audio::ports(2);
    assert!(
        no_metadata(&listener.status()),
        "pairing exposes no metadata"
    );

    listener.configure(audio::config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        std::slice::from_ref(&source_id),
        true,
    ));
    let group = listener
        .wait("Listener mints authoritative group", |status| {
            status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id.len() == 16)
        })
        .config
        .expect("Listener config")
        .group_id;
    source.configure(audio::config(
        wire::AudioRole::Source,
        ports[1],
        &listener_id,
        std::slice::from_ref(&listener_id),
        true,
    ));
    let resolved = source.wait("authenticated Hello resolves Source group", |status| {
        status
            .config
            .as_ref()
            .is_some_and(|config| config.group_id == group)
    });
    assert!(resolved.sessions.is_empty(), "consent alone is not Active");
    assert!(no_metadata(&listener.status()), "no metadata before Active");
    assert!(!resolved.config.expect("Source config").peers[0].allow_media_control);

    listener.start(&source_id);
    let session_id = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    let native = media(&mut source, &session_id);
    assert_eq!(native.title, "Dry media");
    let initial = media(&mut listener, &session_id);
    assert!(
        !initial.title.is_empty(),
        "actual current media has a title"
    );
    assert_ne!(initial.session_token, 0);
    assert_eq!(initial.session_token, native.session_token);
    assert_eq!(initial.revision, native.revision);
    assert_ne!(initial.revision, 0);
    let (requested, _) = opposite_command(&initial);
    denied(
        listener.request(command(&source_id, &session_id, &initial, requested)),
        ipc::audio::codes::AUDIO_CONSENT_REQUIRED,
    );
    // Permission denial takes precedence over absent native targeting facts.
    for (token, revision) in [(0, initial.revision), (initial.session_token, 0), (0, 0)] {
        let mut unavailable = initial.clone();
        unavailable.session_token = token;
        unavailable.revision = revision;
        denied(
            listener.request(command(&source_id, &session_id, &unavailable, requested)),
            ipc::audio::codes::AUDIO_CONSENT_REQUIRED,
        );
    }
    let after_denial = media(&mut listener, &session_id);
    assert_eq!(after_denial.session_token, initial.session_token);
    assert_eq!(after_denial.revision, initial.revision);
    assert_eq!(after_denial.playback, initial.playback);

    // Change only remote-control consent: keep system consent and the live session.
    let source_before = source.running(&listener_id);
    let listener_before = listener.running(&source_id);
    let mut config = source.status().config.expect("Source config");
    let peer = config
        .peers
        .iter_mut()
        .find(|peer| peer.node_id == listener_id)
        .expect("selected Listener policy");
    assert!(peer.allow_system_send);
    assert!(!peer.allow_media_control);
    peer.allow_media_control = true;
    source.configure(config);
    let same_session = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    assert_eq!(
        same_session, session_id,
        "control opt-in must not replace stream"
    );
    let source_after = source.running(&listener_id);
    let listener_after = listener.running(&source_id);
    for (before, after) in [
        (source_before, source_after),
        (listener_before, listener_after),
    ] {
        let before = before.consent.expect("original bilateral consent");
        let after = after.consent.expect("retained bilateral consent");
        for (before, after) in [(before.local, after.local), (before.remote, after.remote)] {
            let before = before.expect("original directional consent");
            let after = after.expect("retained directional consent");
            assert_eq!(after.allow_system_send, before.allow_system_send);
            assert_eq!(after.allow_system_receive, before.allow_system_receive);
        }
    }
    listener.wait(
        "Source control permission reaches the Active Listener",
        |status| {
            status.sessions.iter().any(|session| {
                session.session_id == session_id
                    && session
                        .consent
                        .as_ref()
                        .and_then(|consent| consent.remote.as_ref())
                        .is_some_and(|consent| consent.allow_media_control)
            })
        },
    );

    // Exercise both transitions against the child's current token and revision.
    let mut current = media(&mut listener, &session_id);
    for (token, revision) in [(0, current.revision), (current.session_token, 0), (0, 0)] {
        let mut unavailable = current.clone();
        unavailable.session_token = token;
        unavailable.revision = revision;
        denied(
            listener.request(command(
                &source_id,
                &session_id,
                &unavailable,
                wire::AudioMediaCommand::Toggle,
            )),
            ipc::audio::codes::AUDIO_STALE_MEDIA,
        );
        let unchanged = media(&mut source, &session_id);
        assert_eq!(unchanged.session_token, current.session_token);
        assert_eq!(unchanged.revision, current.revision);
        assert_eq!(unchanged.playback, current.playback);
    }
    let stale = current.clone();
    for _ in 0..2 {
        let (requested, expected_playback) = opposite_command(&current);
        let before_revision = listener.status().revision;
        audio::assert_ok(listener.request(command(&source_id, &session_id, &current, requested)));
        let changed = listener.wait(
            "native command changes playback and media revision",
            |status| {
                status.revision > before_revision
                    && status.sessions.iter().any(|session| {
                        session.session_id == session_id
                            && session.state == wire::AudioSessionState::Running as i32
                            && session.media_info.as_ref().is_some_and(|info| {
                                info.session_token == current.session_token
                                    && info.revision > current.revision
                                    && info.playback == expected_playback as i32
                            })
                    })
            },
        );
        current = changed
            .sessions
            .into_iter()
            .find(|session| session.session_id == session_id)
            .expect("same Active system-audio session")
            .media_info
            .expect("changed native metadata");
    }

    let (requested, _) = opposite_command(&current);
    denied(
        listener.request(command(&source_id, &session_id, &stale, requested)),
        ipc::audio::codes::AUDIO_STALE_MEDIA,
    );
    let mut wrong_token = current.clone();
    wrong_token.session_token = if current.session_token == 1 { 2 } else { 1 };
    denied(
        listener.request(command(&source_id, &session_id, &wrong_token, requested)),
        ipc::audio::codes::AUDIO_STALE_MEDIA,
    );
    let unchanged = media(&mut listener, &session_id);
    assert_eq!(unchanged.session_token, current.session_token);
    assert_eq!(unchanged.revision, current.revision);
    assert_eq!(unchanged.playback, current.playback);
    let mut config = source.status().config.expect("Source config");
    config
        .peers
        .iter_mut()
        .find(|peer| peer.node_id == listener_id)
        .expect("selected Listener policy")
        .allow_media_control = false;
    source.configure(config);
    listener.wait(
        "control withdrawal reaches Listener without stopping metadata",
        |status| {
            status.sessions.iter().any(|session| {
                session.session_id == session_id
                    && session.state == wire::AudioSessionState::Running as i32
                    && session.media_info.is_some()
                    && session
                        .consent
                        .as_ref()
                        .and_then(|consent| consent.remote.as_ref())
                        .is_some_and(|consent| !consent.allow_media_control)
            })
        },
    );
    denied(
        listener.request(command(&source_id, &session_id, &current, requested)),
        ipc::audio::codes::AUDIO_CONSENT_REQUIRED,
    );
    denied(
        listener.request(command(&source_id, &session_id, &stale, requested)),
        ipc::audio::codes::AUDIO_CONSENT_REQUIRED,
    );
    for (token, revision) in [(0, current.revision), (current.session_token, 0), (0, 0)] {
        let mut unavailable = current.clone();
        unavailable.session_token = token;
        unavailable.revision = revision;
        denied(
            listener.request(command(&source_id, &session_id, &unavailable, requested)),
            ipc::audio::codes::AUDIO_CONSENT_REQUIRED,
        );
    }
    let after_withdrawal = media(&mut listener, &session_id);
    assert_eq!(after_withdrawal.revision, current.revision);
    assert_eq!(after_withdrawal.playback, current.playback);

    listener.stop(&source_id, &session_id);
    listener.stopped(&session_id);
    source.stopped(&session_id);
    listener.wait("stop clears private now-playing", no_metadata);
    // Observe multiple control ticks so a queued child update cannot resurrect it.
    let deadline = Instant::now() + Duration::from_millis(750);
    while Instant::now() < deadline {
        assert!(
            no_metadata(&listener.status()),
            "stopped metadata resurrected"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    listener.closed();
    source.closed();
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}
