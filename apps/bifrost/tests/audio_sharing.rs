//! One explicit user intent through real core IPC, authenticated links and Dry workers.
//! Clean-default flows set only media ports/device preferences, without grants or Start RPC.
//! The retarget regression deliberately retains an existing user's legacy grant.
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
        Self::from_nodes(Unpaired::start_audio(label))
    }

    fn from_nodes(nodes: Unpaired) -> Self {
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

fn stopped_cleanly(window: &mut Window, id: &[u8]) {
    window.wait(
        "matching native stop completes without a false failure",
        |status| {
            let Some(session) = status
                .sessions
                .iter()
                .find(|session| session.session_id == id)
            else {
                return false;
            };
            assert_ne!(
                session.state,
                wire::AudioSessionState::Failed as i32,
                "deliberate SYSTEM OFF must not become a worker-loss/revocation failure"
            );
            session.state == wire::AudioSessionState::Idle as i32
                && session.capture_active == Some(false)
                && session.sending == Some(false)
        },
    );
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
    assert_eq!(receiving.role, wire::AudioRole::Listener as i32);
    assert_eq!(
        receiving
            .config
            .as_ref()
            .expect("persisted receive policy")
            .system_sharing,
        Some(true)
    );
    assert!(
        receiving.sessions.is_empty(),
        "receive policy alone is not native media"
    );
    pair.share();
    // The first request-ID-correlated read after the actual persistence ACK,
    // before any polling for media readiness or accepting unsolicited snapshots.
    let requested = pair.source.status();
    assert_eq!(requested.system_sharing_requested, Some(true));
    assert_eq!(requested.role, wire::AudioRole::Source as i32);
    assert_eq!(
        requested
            .config
            .as_ref()
            .expect("persisted share policy")
            .system_sharing,
        Some(true)
    );
    assert!(requested.system_sharing_error.is_empty());
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
    let acknowledged_off = pair.source.status();
    assert_eq!(acknowledged_off.system_sharing_requested, Some(false));
    assert_eq!(acknowledged_off.role, wire::AudioRole::Source as i32);
    assert_eq!(
        acknowledged_off
            .config
            .as_ref()
            .expect("persisted OFF policy")
            .system_sharing,
        Some(false)
    );
    assert!(acknowledged_off.system_sharing_error.is_empty());
    stopped_cleanly(&mut pair.source, &session);
    stopped_cleanly(&mut pair.listener, &session);
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
    stopped_cleanly(&mut pair.listener, &session);
    stopped_cleanly(&mut pair.source, &session);
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
    let session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    let source = pair.source.status();
    let listener = pair.listener.status();
    assert_eq!(
        source.sessions.len(),
        1,
        "one deliberate intent starts one session"
    );
    assert_eq!(listener.sessions.len(), 1);
    assert_eq!(source.sessions[0].session_id, session);
    assert_eq!(listener.sessions[0].session_id, session);
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

#[test]
fn queued_on_from_pre_failure_snapshot_cannot_rearm_after_terminal_native_failure() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-failure-barrier");
    // A real native session-initialization failure, not a fabricated worker event.
    let occupied = std::net::UdpSocket::bind(("127.0.0.1", pair.ports[1]))
        .expect("occupy only the isolated Source media endpoint");
    pair.receive();
    pair.share();
    // This genuine publication precedes reconciliation of requested intent.
    // Model/worker failure is observable before a queued stale action is handled.
    let before_reconciliation = pair.source.event(|message| match message {
        Msg::AudioStatus(status)
            if status.system_sharing_requested == Some(true)
                && status
                    .sessions
                    .iter()
                    .any(|session| session.state == wire::AudioSessionState::Failed as i32) =>
        {
            Some(status.clone())
        }
        _ => None,
    });
    let reply = pair
        .source
        .request(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
            enabled: true,
            role: wire::AudioRole::Source as i32,
            listener_node_id: pair.listener_id.clone(),
            expected_revision: before_reconciliation.revision,
        }));
    assert!(
        matches!(&reply, Msg::OpResult(result)
        if !result.ok && result.error_code == "AUDIO_REVISION_CONFLICT"),
        "stale queued activation from revision {} must not rearm: {reply:?}",
        before_reconciliation.revision
    );
    let stopped = pair.source.status();
    assert_eq!(stopped.system_sharing_requested, Some(false));
    assert!(stopped
        .sessions
        .iter()
        .all(|session| session.capture_active != Some(true) && session.sending != Some(true)));
    assert_eq!(saved(&pair.nodes.b).system_sharing, Some(true));
    assert!(!saved(&pair.nodes.b).resume_system_audio);
    drop(occupied);
    audio::assert_no_audio_resources(&pair.nodes.b, pair.ports[1]);
}

#[test]
fn retarget_preserves_inactive_media_permission_and_revokes_old_system_scope() {
    let _guard = common::serial();
    let (cluster, ()) = common::Cluster::start_with("sharing-retarget", |roots, _, _| {
        for root in roots {
            audio::mark_dry(root);
        }
    });
    for node in &cluster.nodes {
        node.wait_for("trusted three-core mesh", TIMEOUT, |status| {
            status.agent == "ready" && status.members == 2
        })
        .expect("trusted fixture links");
    }
    let source_id = audio::node_id(&cluster.nodes[0].id);
    let a_id = audio::node_id(&cluster.nodes[1].id);
    let b_id = audio::node_id(&cluster.nodes[2].id);
    let mut source = Window::open(&cluster.nodes[0]);
    let mut a = Window::open(&cluster.nodes[1]);
    let mut b = Window::open(&cluster.nodes[2]);
    for (window, port) in [&mut source, &mut a, &mut b]
        .into_iter()
        .zip(audio::ports(3))
    {
        let mut config = window.status().config.expect("default configuration");
        assert!(!config.enabled);
        config.udp_port = u32::from(port);
        window.configure(config);
    }
    // This is an existing user's positive legacy SYSTEM grant and independent
    // media permission, not a shortcut granting the new target consent.
    let mut existing = source.status().config.expect("Source preferences");
    existing.listener_node_id = a_id.clone();
    existing.peers = vec![
        wire::AudioPeerConfig {
            node_id: a_id.clone(),
            allow_system_send: true,
            system_send_override: Some(true),
            allow_media_control: true,
            volume_percent: 100,
            ..Default::default()
        },
        wire::AudioPeerConfig {
            node_id: b_id.clone(),
            volume_percent: 100,
            ..Default::default()
        },
    ];
    source.configure(existing);
    sharing(&mut a, true, wire::AudioRole::Listener, &[]);
    sharing(&mut b, true, wire::AudioRole::Listener, &[]);
    sharing(&mut source, true, wire::AudioRole::Source, &a_id);
    let old_session = audio::healthy(&mut a, &mut source, &a_id, &source_id);

    // Exactly one new user intent, with no hidden permission editor or Start RPC.
    sharing(&mut source, true, wire::AudioRole::Source, &b_id);
    let accepted = source.status();
    assert_eq!(accepted.system_sharing_requested, Some(true));
    assert_eq!(
        accepted
            .config
            .as_ref()
            .expect("persisted target")
            .listener_node_id,
        b_id
    );
    let new_session = audio::healthy(&mut b, &mut source, &b_id, &source_id);
    assert_ne!(new_session, old_session);
    a.stopped(&old_session);
    source.stopped(&old_session);
    let config = saved(&cluster.nodes[0]);
    assert_eq!(
        config.listener_node_id,
        Some(config::audio::HexId(b_id.try_into().unwrap()))
    );
    assert!(config.allow_send_to.is_empty(), "old SYSTEM grant revoked");
    let old = config
        .peers
        .iter()
        .find(|p| p.node_id.0.as_slice() == a_id)
        .unwrap();
    assert!(
        old.allow_media_control,
        "unrelated USER media opt-in retained"
    );
    assert_eq!(old.system_send_override, None);
    let selected = config
        .peers
        .iter()
        .find(|p| Some(p.node_id) == config.listener_node_id)
        .unwrap();
    assert!(
        !selected.allow_media_control,
        "media consent never transferred"
    );
    sharing(&mut source, false, wire::AudioRole::Source, &[]);
    source.stopped(&new_session);
    assert_eq!(saved(&cluster.nodes[0]).system_sharing, Some(false));
    assert!(saved(&cluster.nodes[0])
        .peers
        .iter()
        .any(|p| p.allow_media_control));
}

#[test]
fn source_can_receive_without_clearing_independent_media_preferences() {
    let _guard = common::serial();
    let mut pair = Pair::new("sharing-source-to-receive");
    let mut preferences = pair.source.status().config.expect("Source preferences");
    preferences.listener_node_id = pair.listener_id.clone();
    preferences.profile = wire::AudioProfile::Wifi as i32;
    preferences.microphone.as_mut().unwrap().muted = true;
    preferences.peers.push(wire::AudioPeerConfig {
        node_id: pair.listener_id.clone(),
        allow_system_send: true,
        system_send_override: Some(true),
        allow_media_control: true,
        volume_percent: 100,
        ..Default::default()
    });
    pair.source.configure(preferences);
    let before = saved(&pair.nodes.b);
    sharing(&mut pair.source, true, wire::AudioRole::Listener, &[]);
    let received = pair.source.status();
    assert_eq!(received.role, wire::AudioRole::Listener as i32);
    assert_eq!(received.system_sharing_requested, Some(true));
    assert!(received
        .sessions
        .iter()
        .all(|session| session.capture_active != Some(true)));
    let after = saved(&pair.nodes.b);
    assert_eq!(after.role, config::AudioRole::Listener);
    assert_eq!(after.system_sharing, Some(true));
    assert!(after.allow_send_to.is_empty());
    assert_eq!(after.peers[0].system_send_override, None);
    assert!(
        after.peers[0].allow_media_control,
        "dormant USER opt-in retained"
    );
    assert_eq!(after.microphone, before.microphone);
    assert_eq!(after.profile, before.profile);
    assert_eq!(after.output_device, before.output_device);
    assert_eq!(after.capture_device, before.capture_device);
    assert_eq!(after.resume_system_audio, before.resume_system_audio);
}

#[cfg(windows)]
fn fixture_binary_hash() -> String {
    let path = env!("CARGO_BIN_EXE_bifrost").replace('\'', "''");
    // Use .NET directly: hosted Windows PowerShell may not autoload Get-FileHash.
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$sha = [Security.Cryptography.SHA256]::Create()
$stream = [IO.File]::OpenRead('{path}')
try {{
    [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '')
}} finally {{
    $stream.Dispose()
    $sha.Dispose()
}}
"#
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("hash the exact Cargo-selected binary before launching fixture cores");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hash = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .to_ascii_lowercase();
    assert_eq!(hash.len(), 64);
    println!(
        "WINDOWS_SOURCE_FIXTURE_BINARY path={} sha256={hash}",
        env!("CARGO_BIN_EXE_bifrost")
    );
    hash
}

#[cfg(windows)]
fn owned_roles(nodes: [&common::Node; 2], expected_hash: &str) -> Vec<(u64, String, u64)> {
    let path = env!("CARGO_BIN_EXE_bifrost").replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$expected = [IO.Path]::GetFullPath('{path}')
$hashes = @{{}}
foreach ($rootId in @({a}, {b})) {{
    $core = Get-CimInstance Win32_Process -Filter "ProcessId = $rootId"
    if ($null -eq $core) {{ throw 'owned fixture core exited' }}
    $coreCreationDate = if ($null -eq $core.CreationDate) {{ '<unavailable>' }} else {{ $core.CreationDate.ToUniversalTime().ToString('o') }}
    foreach ($process in @($core) + @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $rootId")) {{
        $creationDate = if ($null -eq $process.CreationDate) {{ '<unavailable>' }} else {{ $process.CreationDate.ToUniversalTime().ToString('o') }}
        if (![string]::Equals($process.ExecutablePath, $expected, [StringComparison]::OrdinalIgnoreCase)) {{
            throw "[DEBUG-rel050-owned-child] fixture role did not use Cargo-selected binary: pid=$($process.ProcessId) parent=$($process.ParentProcessId) created=$creationDate core=$rootId coreCreated=$coreCreationDate image=$($process.ExecutablePath)"
        }}
        $commandLinePresent = ![string]::IsNullOrEmpty([string]$process.CommandLine)
        $firstArgument = '<unavailable>'
        if ($process.CommandLine -match '^(?:"[^"]*"|\S+)\s+("[^"]*"|\S+)') {{
            $firstArgument = $Matches[1]
            if ($firstArgument -notmatch '^"?[A-Za-z-][A-Za-z0-9_-]{{0,31}}"?$') {{ $firstArgument = '<redacted>' }}
        }}
        [Console]::Error.WriteLine("[DEBUG-rel050-owned-child] pid=$($process.ProcessId) parent=$($process.ParentProcessId) created=$creationDate core=$rootId coreCreated=$coreCreationDate image=$($process.ExecutablePath) commandLinePresent=$commandLinePresent firstArgument=$firstArgument")
        if ($process.ProcessId -eq $rootId) {{ $role = 'core' }}
        elseif ($process.CommandLine -match '\s(audio|agent)(?:\s|$)') {{ $role = $Matches[1] }}
        else {{ throw 'unexpected child of owned fixture core' }}
        if (!$hashes.ContainsKey($process.ExecutablePath)) {{
            $sha = [Security.Cryptography.SHA256]::Create()
            $stream = [IO.File]::OpenRead($process.ExecutablePath)
            try {{
                $hashes[$process.ExecutablePath] = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant()
            }} finally {{
                $stream.Dispose()
                $sha.Dispose()
            }}
        }}
        $hash = $hashes[$process.ExecutablePath]
        if ($hash -ne '{expected_hash}') {{ throw 'fixture image changed after prelaunch hash' }}
        "$rootId`t$role`t$($process.ProcessId)`t$($process.ExecutablePath)`t$hash"
    }}
}}
"#,
        a = nodes[0].pid(),
        b = nodes[1].pid()
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("inspect only fixture core/agent/audio process images");
    if output.status.success() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(
        output.status.success(),
        "owned fixture inspection failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::str::from_utf8(&output.stdout).expect("owned process receipt");
    let mut roles = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        println!("WINDOWS_SOURCE_OWNED_PROCESS {line}");
        let mut fields = line.split('\t');
        let core: u64 = fields.next().expect("core PID").parse().unwrap();
        let role = fields.next().expect("role").to_owned();
        let pid: u64 = fields.next().expect("role PID").parse().unwrap();
        let _image = fields.next().expect("image path");
        assert_eq!(fields.next(), Some(expected_hash));
        assert_eq!(fields.next(), None);
        roles.push((core, role, pid));
    }
    for node in nodes {
        for role in ["core", "agent", "audio"] {
            assert_eq!(
                roles
                    .iter()
                    .filter(|(root, kind, _)| *root == u64::from(node.pid()) && kind == role)
                    .count(),
                1
            );
        }
    }
    roles.sort();
    roles
}

#[cfg(windows)]
#[test]
fn windows_process_source_reverses_roles_and_mode_change_keeps_owned_media_running() {
    let _guard = common::serial();
    let expected_hash = fixture_binary_hash();
    let mut pair = Pair::from_nodes(Unpaired::start_audio_no_input("wr"));
    let mut preferences = pair
        .listener
        .status()
        .config
        .expect("isolated Listener preferences");
    preferences.mode = wire::AudioMode::FollowFocus as i32;
    pair.listener.configure(preferences);
    pair.receive();
    pair.share();
    let first = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    let roles_before = owned_roles([&pair.nodes.a, &pair.nodes.b], &expected_hash);
    let listener_before = saved(&pair.nodes.a);
    let source_before = saved(&pair.nodes.b);
    let ended_before = (
        pair.nodes.a.log().matches("audio session ended").count(),
        pair.nodes.b.log().matches("audio session ended").count(),
    );
    // The player button sends precisely this mode-only SetAudio on an ACTIVE
    // Listener. MixAll must remove focus ducking without replacing SYSTEM/native ownership.
    let mut mode_only = pair
        .listener
        .status()
        .config
        .expect("active Listener config");
    mode_only.mode = wire::AudioMode::MixAll as i32;
    pair.listener.configure(mode_only);
    pair.listener
        .wait("MixAll reaches running worker gain policy", |status| {
            status.mode == wire::AudioMode::MixAll as i32
                && status.sessions.iter().any(|session| {
                    session.session_id == first
                        && session.gain_percent == 100
                        && !session.muted
                        && session.state == wire::AudioSessionState::Running as i32
                })
        });
    assert_eq!(
        audio::healthy(
            &mut pair.listener,
            &mut pair.source,
            &pair.listener_id,
            &pair.source_id,
        ),
        first
    );
    assert_eq!(
        owned_roles([&pair.nodes.a, &pair.nodes.b], &expected_hash),
        roles_before
    );
    assert_eq!(
        (
            pair.nodes.a.log().matches("audio session ended").count(),
            pair.nodes.b.log().matches("audio session ended").count(),
        ),
        ended_before,
        "mode-only configuration must not stop either native SYSTEM lifecycle"
    );
    let mut expected_listener = listener_before;
    expected_listener.mode = config::AudioMode::MixAll;
    assert_eq!(
        saved(&pair.nodes.a),
        expected_listener,
        "all independent Listener preferences preserved"
    );
    assert_eq!(
        saved(&pair.nodes.b),
        source_before,
        "Source consent, local-sound authority and P4 preferences unchanged"
    );
    sharing(
        &mut pair.source,
        false,
        wire::AudioRole::Source,
        &pair.listener_id,
    );
    sharing(&mut pair.listener, false, wire::AudioRole::Listener, &[]);
    stopped_cleanly(&mut pair.source, &first);
    stopped_cleanly(&mut pair.listener, &first);

    sharing(&mut pair.source, true, wire::AudioRole::Listener, &[]);
    sharing(
        &mut pair.listener,
        true,
        wire::AudioRole::Source,
        &pair.source_id,
    );
    let reversed = audio::healthy(
        &mut pair.source,
        &mut pair.listener,
        &pair.source_id,
        &pair.listener_id,
    );
    assert_ne!(
        first, reversed,
        "role reversal negotiates a fresh media session"
    );
    assert_eq!(pair.listener.status().role, wire::AudioRole::Source as i32);
    assert_eq!(pair.source.status().role, wire::AudioRole::Listener as i32);
    sharing(
        &mut pair.listener,
        false,
        wire::AudioRole::Source,
        &pair.source_id,
    );
    sharing(&mut pair.source, false, wire::AudioRole::Listener, &[]);
    stopped_cleanly(&mut pair.listener, &reversed);
    stopped_cleanly(&mut pair.source, &reversed);
}
