//! One explicit user intent through real core IPC, authenticated links and Dry workers.
//! Only media ports/device preferences are fixture setup: no consent lists or Start RPC.
#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window, TIMEOUT};
use common::Unpaired;
use protocol::ipc::{self as wire, gui_frame::Msg};

struct Pair {
    nodes: Unpaired,
    listener: Window,
    source: Window,
    listener_id: Vec<u8>,
    source_id: Vec<u8>,
    ports: Vec<u16>,
}

impl Pair {
    fn new(label: &str) -> Self {
        let nodes = Unpaired::start_audio(label);
        nodes.wait_ready();
        let mut listener = Window::open(&nodes.a);
        let mut source = Window::open(&nodes.b);
        audio::pair(&nodes, &mut listener, &mut source);
        let ports = audio::ports(2);
        // Two isolated local cores cannot bind the same production media port.
        // This modifies no user consent or sharing intent.
        for (window, port) in [(&mut listener, ports[0]), (&mut source, ports[1])] {
            let mut config = window.status().config.expect("default configuration");
            assert!(!config.enabled);
            assert_eq!(config.system_sharing, None);
            assert!(config.peers.is_empty());
            config.udp_port = u32::from(port);
            window.configure(config);
        }
        let listener_id = audio::node_id(&nodes.a_id);
        let source_id = audio::node_id(&nodes.b_id);
        Self {
            nodes,
            listener,
            source,
            listener_id,
            source_id,
            ports,
        }
    }

    fn receive(&mut self) {
        sharing(&mut self.listener, true, wire::AudioRole::Listener, &[]);
    }

    fn share(&mut self) {
        sharing(
            &mut self.source,
            true,
            wire::AudioRole::Source,
            &self.listener_id,
        );
    }
}

fn sharing(window: &mut Window, enabled: bool, role: wire::AudioRole, listener: &[u8]) {
    let revision = window.status().revision;
    window.op(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
        enabled,
        role: role as i32,
        listener_node_id: listener.to_vec(),
        expected_revision: revision,
    }));
}

fn saved(node: &common::Node) -> config::AudioConfig {
    config::Config::load(node.root.join("config/config.toml"))
        .expect("saved isolated config")
        .audio
}

#[test]
fn one_toggle_from_defaults_starts_real_media_and_off_preserves_user_preferences() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-one-toggle");
    // Existing, independent user preferences are not implicit system consent.
    let mut preferences = pair.source.status().config.expect("Source preferences");
    preferences.listener_node_id = pair.listener_id.clone();
    preferences.profile = wire::AudioProfile::Wifi as i32;
    preferences
        .microphone
        .as_mut()
        .expect("microphone defaults")
        .muted = true;
    preferences.peers.push(wire::AudioPeerConfig {
        node_id: pair.listener_id.clone(),
        allow_media_control: true,
        volume_percent: 100,
        ..Default::default()
    });
    pair.source.configure(preferences);
    let before = saved(&pair.nodes.b);
    assert!(!before.resume_system_audio);
    assert!(before.allow_send_to.is_empty());
    pair.receive();
    let receiving = pair.listener.status();
    assert_eq!(receiving.system_sharing_requested, Some(true));
    assert!(
        receiving.sessions.is_empty(),
        "receive policy alone is not native media"
    );
    pair.share();
    let session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    let observed = pair.source.running(&pair.listener_id);
    assert_eq!(observed.session_id, session);
    assert_eq!(observed.capture_active, Some(true));
    assert_eq!(observed.sending, Some(true));
    let after = saved(&pair.nodes.b);
    assert_eq!(after.system_sharing, Some(true));
    assert!(
        after.allow_send_to.is_empty(),
        "no fabricated legacy allowlist grants"
    );
    assert!(saved(&pair.nodes.a).allow_system_audio_from.is_empty());
    assert_eq!(after.resume_system_audio, before.resume_system_audio);
    assert_eq!(after.profile, before.profile);
    assert_eq!(after.output_device, before.output_device);
    assert_eq!(after.capture_device, before.capture_device);
    assert_eq!(after.microphone, before.microphone);
    assert_eq!(after.peers, before.peers);
    assert_eq!(after.group_id_override, before.group_id_override);
    assert_eq!(after.group_id, saved(&pair.nodes.a).group_id);
    // Urgent sparse OFF ignores stale role/target/revision, not a stale full draft.
    pair.source
        .op(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: false,
            role: i32::MAX,
            listener_node_id: vec![255],
            expected_revision: 0,
        }));
    pair.source.stopped(&session);
    pair.listener.stopped(&session);
    assert_eq!(pair.source.status().system_sharing_requested, Some(false));
    let off = saved(&pair.nodes.b);
    assert_eq!(off.system_sharing, Some(false));
    assert_eq!(off.microphone, before.microphone);
    assert_eq!(off.peers, before.peers);
    assert_eq!(off.profile, before.profile);
    assert!(!off.resume_system_audio);
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
}

#[test]
fn off_supersedes_pending_on_before_listener_hello_and_never_reopens() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-pending-off");
    let revision = pair.source.status().revision;
    let lock = config::FileLock::acquire(&pair.nodes.b.root.join("config/config.toml"))
        .expect("owned config lock");
    let on = pair
        .source
        .send(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: true,
            role: wire::AudioRole::Source as i32,
            listener_node_id: pair.listener_id.clone(),
            expected_revision: revision,
        }));
    let off = pair
        .source
        .send(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: false,
            role: 0,
            listener_node_id: Vec::new(),
            expected_revision: 0,
        }));
    drop(lock);
    let answers = pair.source.answers_through(off);
    assert!(
        answers
            .iter()
            .any(|(id, answer)| *id == off && matches!(answer, Msg::OpResult(result) if result.ok)),
        "actual correlated replies: {answers:?}"
    );
    assert!(answers.iter().all(|(id, _)| *id == on || *id == off));
    pair.receive();
    pair.listener.event(|message| match message {
        Msg::AudioStatus(status)
            if status.backend.iter().any(|cap| {
                cap.capability == wire::AudioBackendCapability::Render as i32
                    && cap.availability == wire::AudioAvailability::Available as i32
            }) =>
        {
            Some(())
        }
        _ => None,
    });
    let source = pair.source.status();
    assert_eq!(source.system_sharing_requested, Some(false));
    assert!(source.sessions.is_empty());
    assert_eq!(saved(&pair.nodes.b).system_sharing, Some(false));
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
    // A distinct new user press may start; no retry of the superseded activation.
    pair.share();
    audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
}

#[test]
fn restart_without_resume_is_stopped_until_one_new_explicit_on() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-restart-off");
    pair.receive();
    pair.share();
    let old = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    drop(pair.source);
    pair.nodes.b.restart_without_input();
    pair.nodes.wait_ready();
    pair.nodes
        .b
        .wait_for("authenticated reconnect", TIMEOUT, |status| {
            status.members == 1
        })
        .expect("trusted input link reconnects");
    pair.source = Window::open(&pair.nodes.b);
    let stopped = pair.source.status();
    assert_eq!(stopped.system_sharing_requested, Some(false));
    assert!(stopped.sessions.is_empty());
    assert_eq!(saved(&pair.nodes.b).system_sharing, Some(true));
    assert!(!saved(&pair.nodes.b).resume_system_audio);
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
    pair.share();
    let new = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    assert_ne!(new, old, "fresh intent negotiates fresh session/keys");
    assert!(!saved(&pair.nodes.b).resume_system_audio);
}

#[test]
fn untrusted_target_and_explicit_advanced_deny_refuse_without_mutating_configuration() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-denied");
    let before = saved(&pair.nodes.b);
    let revision = pair.source.status().revision;
    let reply = pair
        .source
        .request(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: true,
            role: wire::AudioRole::Source as i32,
            listener_node_id: vec![255; 32],
            expected_revision: revision,
        }));
    assert!(
        matches!(reply, Msg::OpResult(result) if !result.ok && result.error_code == "AUDIO_CONSENT_REQUIRED")
    );
    assert_eq!(saved(&pair.nodes.b), before);
    let mut denied = pair.source.status().config.expect("Advanced preferences");
    denied.listener_node_id = pair.listener_id.clone();
    denied.peers.push(wire::AudioPeerConfig {
        node_id: pair.listener_id.clone(),
        system_send_override: Some(false),
        allow_media_control: true,
        volume_percent: 100,
        ..Default::default()
    });
    pair.source.configure(denied);
    let before = saved(&pair.nodes.b);
    let revision = pair.source.status().revision;
    let reply = pair
        .source
        .request(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: true,
            role: wire::AudioRole::Source as i32,
            listener_node_id: pair.listener_id.clone(),
            expected_revision: revision,
        }));
    assert!(
        matches!(reply, Msg::OpResult(result) if !result.ok && result.error_code == "AUDIO_CONSENT_REQUIRED")
    );
    assert_eq!(saved(&pair.nodes.b), before);
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
}

#[test]
fn listener_advanced_deny_is_authoritative_and_async_refusal_is_visible_before_any_session() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-remote-deny");
    let mut advanced = pair.listener.status().config.expect("Listener preferences");
    advanced.peers.push(wire::AudioPeerConfig {
        node_id: pair.source_id.clone(),
        system_receive_override: Some(false),
        volume_percent: 100,
        ..Default::default()
    });
    pair.listener.configure(advanced);
    pair.receive();
    pair.share();
    let failed = pair
        .source
        .wait("authoritative asynchronous consent refusal", |status| {
            status.system_sharing_requested == Some(false)
                && status.system_sharing_error == "AUDIO_CONSENT_REQUIRED"
        });
    assert!(failed
        .sessions
        .iter()
        .all(|session| session.capture_active != Some(true) && session.sending != Some(true)));
    assert!(pair.listener.status().sessions.is_empty());
    assert_eq!(
        saved(&pair.nodes.a).peers[0].system_receive_override,
        Some(false)
    );
    assert_eq!(
        saved(&pair.nodes.b).system_sharing,
        Some(true),
        "persisted consent is not fabricated successful runtime activity"
    );
}

#[test]
fn system_off_stops_real_media_without_disabling_independent_microphone_policy() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-independent-policy");
    pair.receive();
    pair.share();
    let session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    assert!(
        !pair
            .listener
            .status()
            .microphone
            .expect("default mic status")
            .control_active
    );
    let mut advanced = pair.listener.status().config.expect("Listener preferences");
    let microphone = advanced.microphone.as_mut().expect("microphone settings");
    // An independently and deliberately enabled Dry endpoint, never a host mic.
    microphone.control_enabled = true;
    microphone.muted = true;
    pair.listener.configure(advanced);
    pair.listener
        .wait("independent Dry mute guard observed", |status| {
            status
                .microphone
                .as_ref()
                .is_some_and(|mic| mic.control_active && mic.muted)
        });
    let before = saved(&pair.nodes.a);
    pair.listener
        .op(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: false,
            role: 0,
            listener_node_id: vec![255],
            expected_revision: 0,
        }));
    pair.listener.stopped(&session);
    pair.source.stopped(&session);
    let status = pair.listener.status();
    assert_eq!(status.system_sharing_requested, Some(false));
    let microphone = status.microphone.expect("independent microphone status");
    assert!(microphone.control_active && microphone.muted);
    let after = saved(&pair.nodes.a);
    assert_eq!(after.system_sharing, Some(false));
    assert_eq!(after.microphone, before.microphone);
    assert_eq!(after.peers, before.peers);
    assert_eq!(after.resume_system_audio, before.resume_system_audio);
}

#[test]
fn source_on_waits_for_selected_listener_then_starts_once_without_another_click() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-source-first");
    pair.share();
    let pending = pair
        .source
        .wait("Source probe completes while Listener is off", |status| {
            status.backend.iter().any(|cap| {
                cap.capability == wire::AudioBackendCapability::SystemCapture as i32
                    && cap.availability == wire::AudioAvailability::Available as i32
            })
        });
    assert_eq!(pending.system_sharing_requested, Some(true));
    assert!(pending.system_sharing_error.is_empty());
    assert!(pending.config.expect("pending config").group_id.is_empty());
    assert!(pending.sessions.is_empty());
    pair.receive();
    audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
}

#[test]
fn off_persistence_failure_reports_failure_but_cannot_restore_capture_or_resume_it() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-off-write-failure");
    pair.receive();
    pair.share();
    let session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    let lock = config::FileLock::acquire(&pair.nodes.b.root.join("config/config.toml"))
        .expect("owned config lock");
    let reply = pair
        .source
        .request(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: false,
            role: 0,
            listener_node_id: Vec::new(),
            expected_revision: 0,
        }));
    assert!(matches!(reply, Msg::OpResult(result) if !result.ok
        && result.error_code == "AUDIO_CONFIG_WRITE"));
    let off = pair.source.status();
    assert_eq!(off.system_sharing_requested, Some(false));
    assert_eq!(off.system_sharing_error, "AUDIO_CONFIG_WRITE");
    pair.source.stopped(&session);
    pair.listener.stopped(&session);
    assert_eq!(
        saved(&pair.nodes.b).system_sharing,
        Some(true),
        "a failed persistence operation must not be reported as durable success"
    );
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
    drop(lock);
    drop(pair.source);
    pair.nodes.b.restart_without_input();
    pair.nodes.wait_ready();
    pair.source = Window::open(&pair.nodes.b);
    assert_eq!(pair.source.status().system_sharing_requested, Some(false));
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
    // A new, deliberate OFF can persist after the real lock is released.
    pair.source
        .op(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: false,
            role: 0,
            listener_node_id: Vec::new(),
            expected_revision: 0,
        }));
    assert_eq!(saved(&pair.nodes.b).system_sharing, Some(false));
    assert!(pair.source.status().system_sharing_error.is_empty());
}
