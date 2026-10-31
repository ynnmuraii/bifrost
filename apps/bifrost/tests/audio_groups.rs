//! Cross-machine group regressions through real GUI IPC and authenticated peers.
//! Every core owns an isolated root and explicitly uses Dry media, never host devices.

#![cfg(feature = "audio")]

mod common;

use common::audio::{self, Window, TIMEOUT};
use common::{Node, Unpaired};
use protocol::ipc::{self as wire, gui_frame};
use std::time::{Duration, Instant};

struct Pair {
    two: Unpaired,
    listener: Window,
    source: Window,
    listener_id: Vec<u8>,
    source_id: Vec<u8>,
    group: Vec<u8>,
}

impl Pair {
    fn start(label: &str) -> Self {
        let two = Unpaired::start_audio(label);
        two.wait_ready();
        let mut listener = Window::open(&two.a);
        let mut source = Window::open(&two.b);
        audio::pair(&two, &mut listener, &mut source);
        let listener_id = audio::node_id(&two.a_id);
        let source_id = audio::node_id(&two.b_id);
        let ports = audio::ports(2);
        let mut listener_config = audio::config(
            wire::AudioRole::Listener,
            ports[0],
            &[],
            std::slice::from_ref(&source_id),
            true,
        );
        listener_config.group_id.clear();
        listener.configure(listener_config);
        let group = listener
            .wait("Listener mints its group", |status| {
                status.enabled
                    && status
                        .config
                        .as_ref()
                        .is_some_and(|config| config.group_id.len() == 16)
            })
            .config
            .expect("Listener configuration")
            .group_id;
        assert!(group.iter().any(|byte| *byte != 0));
        let mut source_config = audio::config(
            wire::AudioRole::Source,
            ports[1],
            &listener_id,
            std::slice::from_ref(&listener_id),
            true,
        );
        source_config.group_id.clear();
        source.configure(source_config);
        source.wait("Source resolves the authenticated Listener", |status| {
            status.enabled
                && status
                    .config
                    .as_ref()
                    .is_some_and(|config| config.group_id == group)
        });
        assert_persisted_group(&two.a, &group, Some(false));
        assert_persisted_group(&two.b, &group, Some(false));
        Self {
            two,
            listener,
            source,
            listener_id,
            source_id,
            group,
        }
    }
}

fn assert_persisted_group(node: &Node, group: &[u8], explicit_override: Option<bool>) {
    let saved = config::schema::Config::load(node.root.join("config/config.toml"))
        .expect("persisted audio configuration");
    assert_eq!(
        saved.audio.group_id.expect("persisted group").0.as_slice(),
        group
    );
    assert_eq!(saved.audio.group_id_override, explicit_override);
}

fn restart_source(mut pair: Pair, policy: Option<bool>) -> Pair {
    drop(pair.source);
    pair.two.b.restart_without_input();
    pair.two.wait_ready();
    for node in [&pair.two.a, &pair.two.b] {
        node.wait_for(
            "authenticated parent after Source restart",
            TIMEOUT,
            |status| status.members == 1,
        )
        .expect("owned Source reconnects");
    }
    pair.source = Window::open(&pair.two.b);
    pair.source
        .wait("restored Source preserves raw group policy", |status| {
            status.config.as_ref().is_some_and(|config| {
                config.group_id == pair.group && config.group_id_override == policy
            })
        });
    assert_persisted_group(&pair.two.b, &pair.group, policy);
    pair
}

fn start_once(window: &mut Window, peer: &[u8]) -> gui_frame::Msg {
    let revision = window.status().revision;
    window.request(gui_frame::Msg::StartAudio(wire::StartAudio {
        peer_node_id: peer.to_vec(),
        kind: wire::AudioKind::System as i32,
        profile: wire::AudioProfile::Ethernet as i32,
        expected_revision: revision,
    }))
}

fn changed_group(group: &[u8]) -> Vec<u8> {
    let mut changed = group.to_vec();
    changed[0] ^= 0x80;
    if changed.iter().all(|byte| *byte == 0) {
        changed[1] = 1;
    }
    changed
}

#[test]
fn listener_normal_gui_publishes_preserve_group_across_owned_core_restart() {
    let _guard = common::serial();
    let mut pair = Pair::start("audio-group-listener-stable");
    for publish in 0..3 {
        let before = pair.listener.status();
        let mut config = before.config.expect("Listener configuration");
        config.group_id.clear(); // Normal GUI input, not the observed status group.
        config.background_gain_db = -19 - publish;
        pair.listener.configure(config);
        pair.listener
            .wait("fresh normal GUI publish preserves group", |status| {
                status.revision > before.revision
                    && status
                        .config
                        .as_ref()
                        .is_some_and(|config| config.group_id == pair.group)
            });
        assert_persisted_group(&pair.two.a, &pair.group, Some(false));
        assert_persisted_group(&pair.two.b, &pair.group, Some(false));
    }

    drop(pair.listener);
    pair.two.a.restart_without_input();
    pair.two.wait_ready();
    for node in [&pair.two.a, &pair.two.b] {
        node.wait_for(
            "authenticated link after owned Listener restart",
            TIMEOUT,
            |status| status.members == 1,
        )
        .expect("restarted cores reconnect");
    }
    pair.listener = Window::open(&pair.two.a);
    let restored = pair.listener.status();
    assert_eq!(
        restored.config.expect("restored Listener config").group_id,
        pair.group
    );
    for publish in 0..2 {
        let before = pair.listener.status();
        let mut config = before.config.expect("restarted Listener configuration");
        config.group_id.clear();
        config.background_gain_db = -24 - publish;
        pair.listener.configure(config);
        pair.listener
            .wait("fresh post-restart GUI publish preserves group", |status| {
                status.revision > before.revision
                    && status
                        .config
                        .as_ref()
                        .is_some_and(|config| config.group_id == pair.group)
            });
        assert_persisted_group(&pair.two.a, &pair.group, Some(false));
    }
    pair.source
        .wait("Source retains the same resolved group", |status| {
            status
                .config
                .as_ref()
                .is_some_and(|config| config.group_id == pair.group)
        });
    assert_persisted_group(&pair.two.b, &pair.group, Some(false));
}

#[test]
fn resolved_source_follows_changed_listener_group_without_source_gui_clear_and_persists_it() {
    let _guard = common::serial();
    let mut pair = Pair::start("audio-group-source-follows");
    audio::assert_ok(start_once(&mut pair.listener, &pair.source_id));
    let previous_session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    let before = pair.source.status();
    let received = before
        .sessions
        .iter()
        .find(|session| session.session_id == previous_session)
        .and_then(|session| session.remote_quality.as_ref())
        .map_or(0, |quality| quality.received_packets);
    let progressed = pair
        .source
        .wait("Source receiver-report telemetry advances", |status| {
            status.sessions.iter().any(|session| {
                session.session_id == previous_session
                    && session.state == wire::AudioSessionState::Running as i32
                    && session
                        .remote_quality
                        .as_ref()
                        .is_some_and(|quality| quality.received_packets > received)
            })
        });
    assert_eq!(
        progressed.config, before.config,
        "telemetry is not a preference edit"
    );
    let mut normal = before
        .config
        .clone()
        .expect("automatic Source configuration");
    assert_eq!(normal.group_id_override, Some(false));
    normal.peers[0].allow_media_control = true;
    let request_id = pair.source.send(gui_frame::Msg::SetAudio(wire::SetAudio {
        config: Some(normal),
        expected_revision: before.revision,
    }));
    eprintln!(
        "system-audio permission CAS: request_id={request_id}, expected_revision={}, \
         latest_observed_revision={}, receiver_report_received_before={received}",
        before.revision, progressed.revision,
    );
    audio::assert_ok(pair.source.answer(request_id));
    let accepted = pair.source.status();
    let future = pair
        .source
        .request(gui_frame::Msg::SetAudio(wire::SetAudio {
            config: accepted.config.clone(),
            expected_revision: u64::MAX,
        }));
    assert!(
        matches!(future, gui_frame::Msg::OpResult(result)
        if !result.ok && result.error_code == "AUDIO_REVISION_CONFLICT"),
        "an unpublished future snapshot must remain refused"
    );
    let stale = pair
        .source
        .request(gui_frame::Msg::SetAudio(wire::SetAudio {
            config: before.config.clone(),
            expected_revision: before.revision,
        }));
    assert!(
        matches!(stale, gui_frame::Msg::OpResult(result)
        if !result.ok && result.error_code == "AUDIO_REVISION_CONFLICT"),
        "an actually stale full preference edit must remain refused"
    );
    assert_eq!(pair.source.status().config, accepted.config);
    // Deliberate new mutations use newly read preferences. Returning the payload
    // to A must not make a pre-B snapshot a valid CAS base again.
    let mut returned = pair
        .source
        .status()
        .config
        .expect("current Source preferences");
    returned.peers[0].allow_media_control = false;
    pair.source.configure(returned);
    let returned = pair.source.status();
    assert_eq!(
        returned.config, before.config,
        "the control payload returned to A"
    );
    let aba = pair
        .source
        .request(gui_frame::Msg::SetAudio(wire::SetAudio {
            config: before.config.clone(),
            expected_revision: before.revision,
        }));
    assert!(
        matches!(aba, gui_frame::Msg::OpResult(result)
        if !result.ok && result.error_code == "AUDIO_REVISION_CONFLICT"),
        "ABA must not revive a stale consent/config snapshot"
    );
    let mut normal = pair
        .source
        .status()
        .config
        .expect("fresh Source preferences");
    normal.peers[0].allow_media_control = true;
    pair.source.configure(normal);
    let mut listener_gain = pair
        .listener
        .status()
        .config
        .expect("Listener gain configuration");
    listener_gain.peers[0].volume_percent = 73;
    pair.listener.configure(listener_gain);
    let source_after = pair.source.running(&pair.listener_id);
    let listener_after = pair.listener.running(&pair.source_id);
    assert_eq!(source_after.session_id, previous_session);
    assert_eq!(listener_after.session_id, previous_session);
    assert_eq!(
        audio::healthy(
            &mut pair.listener,
            &mut pair.source,
            &pair.listener_id,
            &pair.source_id,
        ),
        previous_session,
        "permission and gain edits preserve the real media flow and session"
    );
    let normal = pair.source.status().config.expect("saved automatic Source");
    assert_eq!(normal.group_id_override, Some(false));
    assert!(normal.peers[0].allow_media_control);
    assert_eq!(
        pair.listener
            .status()
            .config
            .expect("saved Listener gain")
            .peers[0]
            .volume_percent,
        73
    );
    assert_persisted_group(&pair.two.b, &pair.group, Some(false));
    pair = restart_source(pair, Some(false));
    let source_revision = pair.source.status().revision;
    let next_group = changed_group(&pair.group);
    let mut advanced = pair
        .listener
        .status()
        .config
        .expect("Listener configuration");
    advanced.group_id = next_group.clone();
    pair.listener.configure(advanced);

    // No Source SetAudio occurs: an already-resolved automatic Source must follow Hello.
    pair.source.wait(
        "fresh Hello replaces previously resolved Source group",
        |status| {
            status.revision > source_revision
                && status.enabled
                && status
                    .config
                    .as_ref()
                    .is_some_and(|config| config.group_id == next_group)
        },
    );
    pair.listener.stopped(&previous_session);
    pair.source.stopped(&previous_session);
    assert_persisted_group(&pair.two.a, &next_group, Some(false));
    assert_persisted_group(&pair.two.b, &next_group, Some(false));

    audio::assert_ok(start_once(&mut pair.listener, &pair.source_id));
    let next_session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    assert_ne!(
        previous_session, next_session,
        "new group cannot reuse old session identity"
    );
    pair.listener.stop(&pair.source_id, &next_session);
    pair.listener.stopped(&next_session);
    pair.source.stopped(&next_session);
}

#[test]
fn source_advanced_override_stays_pinned_and_both_start_directions_report_mismatch_promptly() {
    let _guard = common::serial();
    pinned_source_survives_restart(Some(true));
}

#[test]
fn legacy_absent_group_policy_survives_restart_and_remains_pinned() {
    let _guard = common::serial();
    pinned_source_survives_restart(None);
}

fn pinned_source_survives_restart(policy: Option<bool>) {
    let mut pair = Pair::start("audio-group-pinned-restart");
    let mut advanced = pair.source.status().config.expect("Source configuration");
    advanced.group_id = pair.group.clone();
    advanced.group_id_override = policy;
    pair.source.configure(advanced);
    assert_persisted_group(&pair.two.b, &pair.group, policy);

    let before = pair.source.status();
    let mut normal_source = before.config.expect("pinned Source configuration");
    assert_eq!(normal_source.group_id_override, policy);
    normal_source.background_gain_db = -26;
    pair.source.configure(normal_source);
    pair.source.wait(
        "normal Source GUI publish preserves Advanced pin",
        |status| {
            status.revision > before.revision
                && status.config.as_ref().is_some_and(|config| {
                    config.group_id == pair.group && config.listener_node_id == pair.listener_id
                })
        },
    );
    assert_persisted_group(&pair.two.b, &pair.group, policy);
    pair = restart_source(pair, policy);

    let next_group = changed_group(&pair.group);
    let mut listener_config = pair
        .listener
        .status()
        .config
        .expect("Listener configuration");
    listener_config.group_id = next_group.clone();
    pair.listener.configure(listener_config);
    let before = pair.listener.status();
    let mut normal = before.config.expect("changed Listener configuration");
    normal.group_id.clear();
    normal.background_gain_db = -27;
    pair.listener.configure(normal);
    pair.listener.wait(
        "fresh Listener publication retains its changed group",
        |status| {
            status.revision > before.revision
                && status
                    .config
                    .as_ref()
                    .is_some_and(|config| config.group_id == next_group)
        },
    );

    for (window, peer) in [
        (&mut pair.source, pair.listener_id.as_slice()),
        (&mut pair.listener, pair.source_id.as_slice()),
    ] {
        let started = Instant::now();
        let result = start_once(window, peer);
        let elapsed = started.elapsed();
        match result {
            gui_frame::Msg::OpResult(result) => {
                assert!(!result.ok, "mismatched groups cannot start media");
                assert_eq!(result.error_code, "AUDIO_GROUP_MISMATCH");
            }
            other => panic!("StartAudio answered {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "typed mismatch took {elapsed:?}, not an immediate rejection"
        );
    }
    let status = pair.source.status();
    assert_eq!(
        status.config.expect("pinned Source config").group_id,
        pair.group
    );
    assert_persisted_group(&pair.two.b, &pair.group, policy);
    assert_persisted_group(&pair.two.a, &next_group, Some(false));
    pair.listener.closed();
    pair.source.closed();
}

#[test]
fn listener_single_gui_start_activates_both_workers_after_source_has_resolved_and_consented() {
    let _guard = common::serial();
    let mut pair = Pair::start("audio-group-listener-direct-start");
    assert!(pair.listener.status().sessions.is_empty());
    assert!(pair.source.status().sessions.is_empty());

    // One real Listener request: no Source start, consent retry helper, sleeps or fake ACKs.
    audio::assert_ok(start_once(&mut pair.listener, &pair.source_id));
    let session = audio::healthy(
        &mut pair.listener,
        &mut pair.source,
        &pair.listener_id,
        &pair.source_id,
    );
    assert_persisted_group(&pair.two.b, &pair.group, Some(false));
    pair.listener.stop(&pair.source_id, &session);
    pair.listener.stopped(&session);
    pair.source.stopped(&session);
}
