//! Real paired cores, isolated roots, authenticated setup and encrypted loopback UDP.
//! Every launch explicitly selects the debug-only Dry backend; no native audio device
//! or input injection is used. A command ACK is never accepted as native startup.

#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window, TIMEOUT};
use common::{Cluster, Harness, Unpaired};
use protocol::ipc::{self as wire, gui_frame};

#[test]
fn real_pairing_resolves_groupless_source_requires_consent_and_streams_until_stop() {
    let _guard = common::serial();
    let two = Unpaired::start_audio("audio-pairing");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let ports = audio::ports(2);
    assert_ne!(ports[0], ports[1]);

    // Neither pairing nor choosing a trusted Listener grants directional consent.
    let mut listener_config = audio::config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        std::slice::from_ref(&source_id),
        false,
    );
    listener_config.enabled = false;
    listener.configure(listener_config.clone());
    source.configure(audio::config(
        wire::AudioRole::Source,
        ports[1],
        &listener_id,
        std::slice::from_ref(&listener_id),
        false,
    ));
    let waiting = source.status();
    assert!(
        waiting.enabled,
        "persisted explicit opt-in remains visible while waiting"
    );
    let waiting_config = waiting.config.expect("waiting Source config");
    assert!(waiting_config.enabled);
    assert_eq!(waiting_config.listener_node_id, listener_id);
    assert!(
        waiting_config.group_id.is_empty(),
        "Source never fabricates Listener group"
    );
    assert!(waiting.sessions.is_empty());
    let saved = config::schema::Config::load(two.b.root.join("config/config.toml"))
        .expect("persisted pending Source intent");
    assert!(saved.audio.enabled);
    assert!(saved.audio.group_id.is_none());
    assert_eq!(
        saved
            .audio
            .listener_node_id
            .expect("selected Listener")
            .0
            .as_slice(),
        listener_id
    );

    // Enabling the actual Listener mints its authoritative group. Only authenticated
    // Hello can carry it to the Source; the harness never copies it into Source config.
    listener_config.enabled = true;
    listener.configure(listener_config);
    let minted = listener.wait("enabled Listener mints group", |status| {
        status.enabled
            && status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id.len() == 16)
    });
    let group = minted.config.expect("Listener config").group_id;
    assert!(group.iter().any(|byte| *byte != 0));
    let resolved = source.wait("authenticated Hello resolves selected Listener", |status| {
        status
            .config
            .as_ref()
            .is_some_and(|config| config.group_id == group)
    });
    assert!(
        resolved.enabled,
        "authenticated resolution preserves explicit opt-in"
    );
    assert!(
        resolved.sessions.is_empty(),
        "group discovery is not session activation"
    );
    let saved = config::schema::Config::load(two.b.root.join("config/config.toml"))
        .expect("persisted authenticated group");
    assert!(saved.audio.enabled);
    assert_eq!(
        saved.audio.group_id.expect("resolved group").0.as_slice(),
        group
    );
    let revision = listener.status().revision;
    match listener.request(gui_frame::Msg::StartAudio(wire::StartAudio {
        peer_node_id: source_id.clone(),
        kind: wire::AudioKind::System as i32,
        profile: wire::AudioProfile::Ethernet as i32,
        expected_revision: revision,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok, "pairing alone must not authorize audio");
            assert_eq!(result.error_code, "AUDIO_CONSENT_REQUIRED");
        }
        other => panic!("StartAudio answered {other:?}"),
    }
    listener.closed();
    source.closed();

    let mut consent = listener.status().config.expect("Listener config");
    consent.peers[0].allow_system_receive = true;
    listener.configure(consent);
    let mut consent = source.status().config.expect("Source config");
    consent.peers[0].allow_system_send = true;
    source.configure(consent);
    listener.start(&source_id);
    let session_id = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);

    // Exact identity stop, not a broadcast or a stale GUI card targeting a new stream.
    listener.op(gui_frame::Msg::StopAudio(wire::StopAudio {
        peer_node_id: source_id,
        session_id,
        stop_all: false,
    }));
    listener.closed();
    source.closed();
}

#[test]
fn source_gui_start_negotiates_real_media_and_retry_uses_fresh_session_id() {
    let _guard = common::serial();
    let two = Unpaired::start_audio("audio-source-start");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    audio::pair(&two, &mut listener, &mut source);
    let listener_id = audio::node_id(&two.a_id);
    let source_id = audio::node_id(&two.b_id);
    let ports = audio::ports(2);
    listener.configure(audio::config(
        wire::AudioRole::Listener,
        ports[0],
        &[],
        std::slice::from_ref(&source_id),
        true,
    ));
    let group = listener
        .wait("Listener mints authoritative group", |status| {
            status.enabled
                && status
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
    let resolved = source.wait("Source resolves authenticated Listener group", |status| {
        status.enabled
            && status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id == group)
    });
    assert!(
        resolved.sessions.is_empty(),
        "consent is not a start request"
    );
    assert!(listener.status().sessions.is_empty());

    // Only the Source GUI asks to start: the Listener must answer through the
    // authenticated StartRequest -> Offer/Answer path, not a harness shortcut.
    source.start(&listener_id);
    let first = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    source.stop(&listener_id, &first);
    source.stopped(&first);
    listener.stopped(&first);
    source.closed();
    listener.closed();

    source.start(&listener_id);
    let second = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    assert_ne!(
        first, second,
        "Source retry must negotiate a fresh identity"
    );
    source.stop(&listener_id, &second);
    source.stopped(&second);
    listener.stopped(&second);
    audio::disable(&two.b, &mut source);
    audio::disable(&two.a, &mut listener);
}

#[test]
fn three_node_audio_shares_listener_worker_and_exact_stop_preserves_other_source() {
    let _guard = common::serial();
    let cluster = Cluster::start_audio("audio-three-node");
    cluster.wait_mesh(TIMEOUT);
    let mut listener = Window::open(cluster.node(0));
    let mut first = Window::open(cluster.node(1));
    let mut second = Window::open(cluster.node(2));
    let listener_id = audio::node_id(&cluster.node(0).id);
    let first_id = audio::node_id(&cluster.node(1).id);
    let second_id = audio::node_id(&cluster.node(2).id);
    let first_session = audio::healthy(&mut listener, &mut first, &listener_id, &first_id);
    let second_session = audio::healthy(&mut listener, &mut second, &listener_id, &second_id);
    assert_ne!(first_session, second_session);
    let status = listener.status();
    assert_eq!(
        status
            .sessions
            .iter()
            .filter(|session| { session.state == wire::AudioSessionState::Running as i32 })
            .count(),
        2,
        "both Sources run concurrently on the same Listener"
    );
    let media_port = u16::try_from(status.config.expect("Listener config").udp_port)
        .expect("Listener media port");
    #[cfg(target_os = "linux")]
    audio::assert_single_audio_socket(cluster.node(0), media_port);

    listener.stop(&first_id, &first_session);
    listener.stopped(&first_session);
    first.stopped(&first_session);
    first.closed();
    assert_eq!(
        audio::healthy(&mut listener, &mut second, &listener_id, &second_id),
        second_session,
        "exact stop preserves the other session and advancing media"
    );
    assert_eq!(
        listener.status().config.expect("Listener config").udp_port,
        u32::from(media_port),
        "both sessions use the same Listener media port"
    );
    #[cfg(target_os = "linux")]
    audio::assert_single_audio_socket(cluster.node(0), media_port);

    // Disable the shared Listener while the second Source is still streaming.
    audio::disable(cluster.node(0), &mut listener);
    second.stopped(&second_session);
    second.closed();
    audio::disable(cluster.node(1), &mut first);
    audio::disable(cluster.node(2), &mut second);
}

#[test]
fn authenticated_link_drop_disarms_both_live_media_workers() {
    let _guard = common::serial();
    let harness = Harness::start_audio(true);
    let mut listener = Window::open(&harness.a);
    let mut source = Window::open(&harness.b);
    let listener_id = audio::node_id(&harness.a.id);
    let source_id = audio::node_id(&harness.b.id);
    audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    // Hold both authenticated directions down, rather than letting a reconnect hide
    // the teardown. Media UDP is not proxied: loss of parent authority must stop it.
    harness.proxy.as_ref().expect("two-way proxy").blackhole();
    for node in [&harness.a, &harness.b] {
        node.wait_for("authenticated parent link expires", TIMEOUT, |status| {
            status.members == 0
        })
        .expect("link failure becomes observable");
    }
    listener.closed();
    source.closed();
    harness.proxy.as_ref().expect("proxy").forward();
}

#[cfg(target_os = "linux")]
#[test]
fn killing_audio_child_fails_session_without_killing_input_and_retry_uses_fresh_id() {
    let _guard = common::serial();
    let harness = Harness::start_audio(false);
    let mut listener = Window::open(&harness.a);
    let mut source = Window::open(&harness.b);
    let listener_id = audio::node_id(&harness.a.id);
    let source_id = audio::node_id(&harness.b.id);
    let old_id = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    let children = audio::audio_children(&harness.b);
    assert_eq!(children.len(), 1, "one supervised Source audio child");
    assert!(std::process::Command::new("kill")
        .args(["-KILL", &children[0].to_string()])
        .status()
        .expect("kill owned Dry audio child")
        .success());
    // Failed can be transient during supervisor recovery: consume the authoritative
    // push instead of hoping a periodic query catches it.
    source.event(|event| match event {
        gui_frame::Msg::AudioStatus(status)
            if status.sessions.iter().any(|session| {
                session.session_id == old_id
                    && session.state == wire::AudioSessionState::Failed as i32
            }) =>
        {
            Some(())
        }
        _ => None,
    });
    listener.closed();
    source.closed();
    for node in [&harness.a, &harness.b] {
        let status = node
            .status()
            .expect("input survives independent media failure");
        assert_eq!(status.agent, "ready");
        assert!(
            status.members >= 1,
            "authenticated KVM link survives media crash"
        );
        assert_eq!(status.pressed, 0);
    }
    // Explicit retry is required: resume_system_audio is deliberately false.
    listener.stop_all();
    source.stop_all();
    listener.start(&source_id);
    let new_id = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    assert_ne!(
        new_id, old_id,
        "replacement worker must negotiate fresh session keys/identity"
    );
    listener.stop_all();
    listener.closed();
    source.closed();
}

#[cfg(target_os = "linux")]
#[test]
fn disabled_audio_owns_neither_child_nor_media_udp_before_or_after_use() {
    let _guard = common::serial();
    let two = Unpaired::start_audio("audio-disabled");
    two.wait_ready();
    let mut listener = Window::open(&two.a);
    let mut source = Window::open(&two.b);
    let ports = audio::ports(2);
    for (node, window, port) in [
        (&two.a, &mut listener, ports[0]),
        (&two.b, &mut source, ports[1]),
    ] {
        let status = window.status();
        assert!(!status.enabled);
        assert!(status.sessions.is_empty());
        audio::assert_no_audio_resources(node, port);
    }
    audio::pair(&two, &mut listener, &mut source);
    // Use the same setup as full KVM stress mode, then prove disable releases ownership.
    audio::start_group(&[&two.a, &two.b]);
    for (node, window) in [(&two.a, &mut listener), (&two.b, &mut source)] {
        audio::disable(node, window);
    }
}

#[test]
fn gui_controls_use_authoritative_media_state_and_deferred_features_never_succeed() {
    let _guard = common::serial();
    let harness = Harness::start_audio(false);
    let mut listener = Window::open(&harness.a);
    let mut source = Window::open(&harness.b);
    let listener_id = audio::node_id(&harness.a.id);
    let source_id = audio::node_id(&harness.b.id);
    let session_id = audio::healthy(&mut listener, &mut source, &listener_id, &source_id);
    let revision = listener.status().revision;
    listener.op(gui_frame::Msg::SetAudioPeerGain(wire::SetAudioPeerGain {
        peer_node_id: source_id.clone(),
        gain_percent: 37,
        muted: false,
        expected_revision: revision,
    }));
    listener.wait("observed per-peer gain", |status| {
        status
            .sessions
            .iter()
            .any(|session| session.session_id == session_id && session.gain_percent == 37)
    });
    let request = listener.send(gui_frame::Msg::ListAudioDevices(wire::ListAudioDevices {
        kind: wire::AudioDeviceKind::Output as i32,
    }));
    match listener.answer(request) {
        gui_frame::Msg::AudioDevices(devices) => {
            assert_eq!(devices.kind, wire::AudioDeviceKind::Output as i32);
            assert!(devices.complete, "Dry inventory fits a single bounded page");
            assert!(
                !devices.devices.is_empty(),
                "Dry render endpoint is real to worker"
            );
        }
        other => panic!("ListAudioDevices answered {other:?}"),
    }
    let revision = source.status().revision;
    let result = source.request(gui_frame::Msg::SetMicrophoneMute(wire::SetMicrophoneMute {
        muted: true,
        expected_revision: revision,
    }));
    match result {
        gui_frame::Msg::OpResult(result) => assert!(
            result.ok
                || matches!(
                    result.error_code.as_str(),
                    "AUDIO_UNAVAILABLE" | "AUDIO_UNSUPPORTED_COMMAND"
                )
        ),
        other => panic!("SetMicrophoneMute answered {other:?}"),
    }
    let revision = source.status().revision;
    let result = source.request(gui_frame::Msg::SetPtt(wire::SetPtt {
        pressed: false,
        expected_revision: revision,
    }));
    match result {
        gui_frame::Msg::OpResult(result) => assert!(
            result.ok
                || matches!(
                    result.error_code.as_str(),
                    "AUDIO_UNAVAILABLE" | "AUDIO_UNSUPPORTED_COMMAND"
                )
        ),
        other => panic!("SetPtt release answered {other:?}"),
    }
    // M5/M6 are deferred, so lack of a native media session must be an explicit typed
    // rejection, never a successful ACK for work that nobody executes.
    match listener.request(gui_frame::Msg::SendMediaCommand(wire::SendMediaCommand {
        peer_node_id: source_id,
        session_id,
        session_token: 0,
        media_revision: 0,
        command: wire::AudioMediaCommand::Play as i32,
    })) {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok);
            assert!(matches!(
                result.error_code.as_str(),
                "AUDIO_UNAVAILABLE" | "AUDIO_UNSUPPORTED_COMMAND" | "AUDIO_STALE_MEDIA"
            ));
        }
        other => panic!("SendMediaCommand answered {other:?}"),
    }
    let status = source.status();
    let microphone = status.microphone.expect("microphone observation");
    assert!(!microphone.capture_active);
    assert!(!microphone.ptt_pressed);
    listener.stop_all();
    listener.closed();
    source.closed();
}

#[test]
fn fixture_input_and_clipboard_endpoints_are_distinct_and_reservations_are_released() {
    let _guard = common::serial();
    let ports: [u16; 6] = common::distinct_ports();
    let unique: std::collections::BTreeSet<_> = ports.into_iter().collect();
    assert_eq!(unique.len(), ports.len());
    let _listeners = ports.map(|port| {
        std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("fixture reservation is released for the service")
    });
}
