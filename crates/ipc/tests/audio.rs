use ipc::audio::{validate_config, validate_frame, validate_status};
use prost::Message;
use protocol::ipc as wire;

fn config() -> wire::AudioConfig {
    wire::AudioConfig {
        role: wire::AudioRole::Listener as i32,
        group_id_override: Some(false),
        mode: wire::AudioMode::MixAll as i32,
        profile: wire::AudioProfile::Ethernet as i32,
        udp_port: 47903,
        output_device: "default".into(),
        capture_device: "default_monitor".into(),
        focus_debounce_ms: 250,
        focus_ramp_ms: 150,
        background_gain_db: -18,
        microphone: Some(wire::AudioMicrophoneConfig {
            capture_device: "default".into(),
            muted: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}
fn status() -> wire::AudioStatus {
    wire::AudioStatus {
        role: wire::AudioRole::Listener as i32,
        mode: wire::AudioMode::MixAll as i32,
        config: Some(config()),
        microphone: Some(wire::AudioMicrophoneState::default()),
        ..Default::default()
    }
}
fn session_status() -> wire::AudioStatus {
    let mut value = status();
    value.sessions.push(wire::AudioSessionStatus {
        session_id: vec![1; 16],
        peer: vec![2; 32],
        source_name: "WORKSTATION".into(),
        local_role: wire::AudioRole::Listener as i32,
        state: wire::AudioSessionState::PathValidation as i32,
        profile: wire::AudioProfile::Ethernet as i32,
        kind: wire::AudioKind::System as i32,
        health: Some(wire::AudioHealth::default()),
        health_state: wire::AudioStreamHealth::Ready as i32,
        gain_percent: 100,
        consent: Some(wire::AudioSessionConsent {
            local: Some(wire::AudioConsentFlags::default()),
            remote: Some(wire::AudioConsentFlags::default()),
        }),
        ..Default::default()
    });
    value
}
fn frame(message: wire::gui_frame::Msg) -> wire::GuiFrame {
    wire::GuiFrame {
        seq: 1,
        request_id: 1,
        msg: Some(message),
    }
}

#[test]
fn modern_listener_retains_dormant_media_preferences_without_relaxing_other_roles() {
    let mut value = config();
    value.peers.push(wire::AudioPeerConfig {
        node_id: vec![1; 32],
        allow_media_control: true,
        volume_percent: 100,
        ..Default::default()
    });
    assert!(
        validate_config(&value).is_err(),
        "legacy Listener stays strict"
    );
    for policy in [false, true] {
        value.system_sharing = Some(policy);
        validate_config(&value).unwrap();
        value.peers[0].allow_mic_receive = true;
        assert!(
            validate_config(&value).is_err(),
            "mic role checks unchanged"
        );
        value.peers[0].allow_mic_receive = false;
        value.peers[0].system_send_override = Some(true);
        assert!(
            validate_config(&value).is_err(),
            "Listener cannot send SYSTEM"
        );
        value.peers[0].system_send_override = None;
    }
}

#[test]
fn native_ptt_arming_preserves_optional_observations_and_rejects_unknown_states() {
    for state in [
        None,
        Some(wire::AudioPttArmingState::Armed as i32),
        Some(wire::AudioPttArmingState::NeedsPhysicalRelease as i32),
        Some(wire::AudioPttArmingState::Unavailable as i32),
    ] {
        let mut value = status();
        let mic = value.microphone.as_mut().unwrap();
        mic.native_ptt_arming = state;
        // GUI-held PTT is independent of native hotkey readiness.
        mic.control_active = true;
        mic.ptt_pressed = true;
        assert!(validate_status(&value).is_ok());
        let encoded = frame(wire::gui_frame::Msg::AudioStatus(value.clone()));
        let decoded = wire::GuiFrame::decode(encoded.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, encoded);
        assert!(validate_frame(&decoded).is_ok());
        let wire::gui_frame::Msg::AudioStatus(restored) = decoded.msg.unwrap() else {
            panic!("expected audio status");
        };
        assert_eq!(restored.microphone.unwrap().native_ptt_arming, state);
    }
    for state in [0, -1, 4, i32::MAX] {
        let mut value = status();
        value.microphone.as_mut().unwrap().native_ptt_arming = Some(state);
        assert!(validate_status(&value).is_err());
        assert!(validate_frame(&frame(wire::gui_frame::Msg::AudioStatus(value))).is_err());
    }
}

#[test]
fn native_ptt_unavailability_details_are_optional_bounded_and_state_specific() {
    for reason in [
        None,
        Some(wire::AudioPttUnavailableReason::HostClockUnavailable as i32),
        Some(wire::AudioPttUnavailableReason::PortalFreshnessUnverifiable as i32),
        Some(wire::AudioPttUnavailableReason::RegistrationUnavailable as i32),
    ] {
        for state in [
            None,
            Some(wire::AudioPttArmingState::Armed as i32),
            Some(wire::AudioPttArmingState::NeedsPhysicalRelease as i32),
            Some(wire::AudioPttArmingState::Unavailable as i32),
        ] {
            let mut value = status();
            let mic = value.microphone.as_mut().unwrap();
            mic.native_ptt_arming = state;
            mic.native_ptt_unavailable_reason = reason;
            mic.control_active = true;
            mic.ptt_pressed = true;
            let valid =
                reason.is_none() || state == Some(wire::AudioPttArmingState::Unavailable as i32);
            assert_eq!(validate_status(&value).is_ok(), valid);
            let encoded = frame(wire::gui_frame::Msg::AudioStatus(value));
            let decoded = wire::GuiFrame::decode(encoded.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded, encoded);
            assert_eq!(validate_frame(&decoded).is_ok(), valid);
        }
    }
    for reason in [0, -1, 4, i32::MAX] {
        let mut value = status();
        let mic = value.microphone.as_mut().unwrap();
        mic.native_ptt_arming = Some(wire::AudioPttArmingState::Unavailable as i32);
        mic.native_ptt_unavailable_reason = Some(reason);
        assert!(validate_status(&value).is_err());
        assert!(validate_frame(&frame(wire::gui_frame::Msg::AudioStatus(value))).is_err());
    }
}

#[test]
fn validates_config_ranges_and_security_structure() {
    assert!(validate_config(&config()).is_ok());
    for port in [0, 47902, 65536, u32::MAX] {
        let mut c = config();
        c.udp_port = port;
        assert!(validate_config(&c).is_err());
    }
    for role in [0, 3, -1] {
        let mut c = config();
        c.role = role;
        assert!(validate_config(&c).is_err());
    }
    let mut c = config();
    c.enabled = true;
    assert!(validate_config(&c).is_ok());
    c.group_id = vec![1; 16];
    assert!(validate_config(&c).is_ok());
    c.role = wire::AudioRole::Source as i32;
    assert!(validate_config(&c).is_err());
    c.listener_node_id = vec![2; 32];
    assert!(validate_config(&c).is_ok());
    c.peers.push(wire::AudioPeerConfig {
        node_id: vec![2; 32],
        allow_system_send: true,
        volume_percent: 100,
        ..Default::default()
    });
    assert!(validate_config(&c).is_ok());
    c.peers[0].allow_system_receive = true;
    assert!(validate_config(&c).is_err());
    c.peers[0].allow_system_receive = false;
    c.peers.push(c.peers[0].clone());
    assert!(validate_config(&c).is_err());
    c.peers.pop();
    c.microphone.as_mut().unwrap().streaming_enabled = true;
    assert!(validate_config(&c).is_err());
    let mut c = config();
    c.output_device = "é".repeat(129);
    assert!(validate_config(&c).is_err());
    c.output_device = "bad\0id".into();
    assert!(validate_config(&c).is_err());
    c.output_device = "default".into();
    c.microphone.as_mut().unwrap().ptt_enabled = true;
    assert!(validate_config(&c).is_err());
}

#[test]
fn gui_only_ptt_allows_no_global_binding_but_checks_configured_chords() {
    let mut value = config();
    let mic = value.microphone.as_mut().unwrap();
    mic.control_enabled = true;
    mic.ptt_enabled = true;
    assert!(validate_config(&value).is_ok());
    value.microphone.as_mut().unwrap().ptt_hotkey = "Ctrl+Alt+P".into();
    assert!(validate_config(&value).is_ok());
    value.microphone.as_mut().unwrap().ptt_hotkey = "Ctrl++".into();
    assert!(validate_config(&value).is_err());
    value.microphone.as_mut().unwrap().ptt_hotkey = "bad\0chord".into();
    assert!(validate_config(&value).is_err());
}

#[test]
fn enabled_listener_accepts_empty_group() {
    let mut c = config();
    c.enabled = true;
    assert!(validate_config(&c).is_ok());
    let mut s = status();
    s.enabled = true;
    s.config = Some(c);
    assert!(validate_status(&s).is_ok());
}

#[test]
fn enabled_source_with_listener_accepts_empty_group() {
    let mut c = config();
    c.enabled = true;
    c.role = wire::AudioRole::Source as i32;
    c.listener_node_id = vec![2; 32];
    assert!(validate_config(&c).is_ok());
}

#[test]
fn enabled_source_without_listener_rejects_empty_group() {
    let mut c = config();
    c.enabled = true;
    c.role = wire::AudioRole::Source as i32;
    assert!(validate_config(&c).is_err());
}

#[test]
fn nonempty_group_requires_exactly_sixteen_bytes() {
    for length in [1, 15, 17, 32] {
        let mut c = config();
        c.group_id = vec![1; length];
        assert!(validate_config(&c).is_err());
    }
}

#[test]
fn audio_group_override_preserves_presence_and_requires_a_valid_source_pin() {
    for group_id_override in [None, Some(false), Some(true)] {
        let mut value = config();
        value.role = wire::AudioRole::Source as i32;
        value.listener_node_id = vec![2; 32];
        value.group_id = vec![3; 16];
        value.group_id_override = group_id_override;
        assert!(validate_config(&value).is_ok());
        let decoded = wire::AudioConfig::decode(value.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded.group_id_override, group_id_override);
        value.group_id.clear();
        assert_eq!(
            validate_config(&value).is_ok(),
            group_id_override != Some(true)
        );
        value.group_id = vec![3; 16];
        value.role = wire::AudioRole::Listener as i32;
        assert_eq!(
            validate_config(&value).is_ok(),
            group_id_override != Some(true)
        );
    }
}
#[test]
fn exact_stop_identity_and_correlation_are_required() {
    use wire::gui_frame::Msg;
    assert!(validate_frame(&frame(Msg::StopAudio(wire::StopAudio {
        stop_all: true,
        ..Default::default()
    })))
    .is_ok());
    assert!(validate_frame(&frame(Msg::StopAudio(wire::StopAudio {
        stop_all: true,
        peer_node_id: vec![1; 32],
        ..Default::default()
    })))
    .is_err());
    assert!(validate_frame(&frame(Msg::StopAudio(wire::StopAudio {
        stop_all: false,
        peer_node_id: vec![1; 32],
        session_id: vec![2; 16]
    })))
    .is_ok());
    assert!(validate_frame(&frame(Msg::StopAudio(wire::StopAudio::default()))).is_err());
    let mut f = frame(Msg::GetAudioStatus(wire::GetAudioStatus {}));
    f.request_id = 0;
    assert!(validate_frame(&f).is_err());
    let mut pushed = frame(Msg::AudioStatus(status()));
    pushed.request_id = 0;
    assert!(validate_frame(&pushed).is_ok());
}
#[test]
fn devices_and_commands_are_bounded() {
    use wire::gui_frame::Msg;
    let device = wire::AudioDevice {
        id: "device".into(),
        name: "Audio".into(),
        is_default: true,
        available: true,
    };
    let mut devices = wire::AudioDevices {
        kind: wire::AudioDeviceKind::Output as i32,
        revision: 1,
        devices: vec![device.clone()],
        complete: true,
    };
    assert!(validate_frame(&frame(Msg::AudioDevices(devices.clone()))).is_ok());
    devices.devices.push(device);
    assert!(validate_frame(&frame(Msg::AudioDevices(devices))).is_err());
    let mut command = wire::SendMediaCommand {
        peer_node_id: vec![1; 32],
        session_id: vec![2; 16],
        session_token: 0,
        media_revision: 0,
        command: wire::AudioMediaCommand::Toggle as i32,
        position_ms: None,
    };
    // Semantic unavailability returns AUDIO_STALE_MEDIA without closing the GUI link.
    for (token, revision) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        command.session_token = token;
        command.media_revision = revision;
        assert!(validate_frame(&frame(Msg::SendMediaCommand(command.clone()))).is_ok());
    }
    command.command = 0;
    assert!(validate_frame(&frame(Msg::SendMediaCommand(command))).is_err());
}

#[test]
fn seek_payload_presence_is_strict_but_zero_is_valid() {
    use wire::gui_frame::Msg;
    let mut command = wire::SendMediaCommand {
        peer_node_id: vec![1; 32],
        session_id: vec![2; 16],
        session_token: 1,
        media_revision: 1,
        command: wire::AudioMediaCommand::Seek as i32,
        position_ms: Some(0),
    };
    assert!(validate_frame(&frame(Msg::SendMediaCommand(command.clone()))).is_ok());
    command.position_ms = None;
    assert!(validate_frame(&frame(Msg::SendMediaCommand(command.clone()))).is_err());
    command.position_ms = Some(0);
    command.command = wire::AudioMediaCommand::Play as i32;
    assert!(validate_frame(&frame(Msg::SendMediaCommand(command))).is_err());
}
#[tokio::test]
async fn gui_link_validates_both_directions_without_advancing_send_sequence() {
    use wire::gui_frame::Msg;
    let (a, b) = tokio::io::duplex(4096);
    let mut sender = ipc::GuiLink::new(a);
    let mut receiver = ipc::GuiLink::new(b);
    assert!(sender
        .send(1, Msg::StopAudio(wire::StopAudio::default()))
        .await
        .is_err());
    assert_eq!(sender.outbound_next(), 1);
    sender
        .send(1, Msg::GetAudioStatus(wire::GetAudioStatus {}))
        .await
        .unwrap();
    assert_eq!(receiver.recv().await.unwrap().seq, 1);
    sender
        .framed()
        .send(&wire::GuiFrame {
            seq: 2,
            request_id: 2,
            msg: Some(Msg::StopAudio(wire::StopAudio::default())),
        })
        .await
        .unwrap();
    assert!(receiver.recv().await.is_err());
    assert_eq!(receiver.inbound_expected(), 2);
}

#[test]
fn required_enums_and_numeric_boundaries_are_strict() {
    for value in [0, -1, i32::MAX] {
        assert!(ipc::audio::decode_role(value).is_err());
        assert!(ipc::audio::decode_mode(value).is_err());
        assert!(ipc::audio::decode_profile(value).is_err());
        assert!(ipc::audio::decode_session_state(value).is_err());
        assert!(ipc::audio::decode_health(value).is_err());
        assert!(ipc::audio::decode_capability(value).is_err());
        assert!(ipc::audio::decode_reason(value).is_err());
        assert!(ipc::audio::decode_playback(value).is_err());
        assert!(ipc::audio::decode_command(value).is_err());
        assert!(ipc::audio::decode_ptt_arming(value).is_err());
        assert!(ipc::audio::decode_ptt_unavailable_reason(value).is_err());
    }
    for port in [1, 47901, 65535] {
        let mut c = config();
        c.udp_port = port;
        assert!(validate_config(&c).is_ok());
    }
    for (debounce, ramp, gain) in [(100, 20, -60), (2000, 1000, 0)] {
        let mut c = config();
        c.focus_debounce_ms = debounce;
        c.focus_ramp_ms = ramp;
        c.background_gain_db = gain;
        assert!(validate_config(&c).is_ok());
    }
    for (debounce, ramp, gain) in [
        (99, 20, -60),
        (2001, 20, -60),
        (100, 19, -60),
        (100, 1001, -60),
        (100, 20, -61),
        (100, 20, 1),
    ] {
        let mut c = config();
        c.focus_debounce_ms = debounce;
        c.focus_ramp_ms = ramp;
        c.background_gain_db = gain;
        assert!(validate_config(&c).is_err());
    }
    for (mode, profile) in [(0, 1), (1, 0), (99, 1), (1, 99)] {
        let mut c = config();
        c.mode = mode;
        c.profile = profile;
        assert!(validate_config(&c).is_err());
    }
    for length in [1, 15, 17, 31, 33] {
        let mut c = config();
        c.listener_node_id = vec![1; length];
        assert!(validate_config(&c).is_err());
    }
}

#[test]
fn device_pages_allow_64_unique_ids_but_never_65() {
    use wire::gui_frame::Msg;
    let mut devices = wire::AudioDevices {
        kind: wire::AudioDeviceKind::Capture as i32,
        revision: 9,
        complete: false,
        devices: (0..64)
            .map(|index| wire::AudioDevice {
                id: format!("id-{index}"),
                name: "é".repeat(128),
                is_default: index == 0,
                available: true,
            })
            .collect(),
    };
    assert!(validate_frame(&frame(Msg::AudioDevices(devices.clone()))).is_ok());
    devices.devices.push(wire::AudioDevice {
        id: "id-64".into(),
        ..Default::default()
    });
    assert!(validate_frame(&frame(Msg::AudioDevices(devices.clone()))).is_err());
    devices.devices.pop();
    devices.devices[0].name.push('x');
    assert!(validate_frame(&frame(Msg::AudioDevices(devices.clone()))).is_err());
    devices.devices[0].name.clear();
    devices.kind = 0;
    assert!(validate_frame(&frame(Msg::AudioDevices(devices))).is_err());
}

#[test]
fn status_rejects_inconsistent_availability_and_microphone_state() {
    let mut s = status();
    s.backend.push(wire::AudioCapabilityAvailability {
        capability: wire::AudioBackendCapability::Render as i32,
        availability: wire::AudioAvailability::Available as i32,
        reason: 0,
    });
    assert!(validate_status(&s).is_ok());
    s.backend[0].reason = wire::AudioUnavailableReason::DeviceGone as i32;
    assert!(validate_status(&s).is_err());
    s.backend[0].availability = wire::AudioAvailability::Unavailable as i32;
    assert!(validate_status(&s).is_ok());
    s.backend[0].reason = 0;
    assert!(validate_status(&s).is_err());
    s.backend.clear();
    s.microphone.as_mut().unwrap().capture_active = true;
    assert!(validate_status(&s).is_err());
    s.microphone.as_mut().unwrap().capture_active = false;
    s.microphone.as_mut().unwrap().ptt_pressed = true;
    assert!(validate_status(&s).is_err());
    s.microphone.as_mut().unwrap().ptt_pressed = false;
    s.config.as_mut().unwrap().mode = wire::AudioMode::FollowFocus as i32;
    assert!(validate_status(&s).is_err());
}

#[test]
fn status_path_diagnostics_and_ports_are_strict() {
    let mut value = session_status();
    assert!(validate_status(&value).is_ok());
    for unknown in [-1, 5, i32::MAX] {
        value.sessions[0].path_diagnostic = unknown;
        assert!(validate_status(&value).is_err());
    }
    for diagnostic in [
        wire::AudioPathDiagnostic::None,
        wire::AudioPathDiagnostic::NoAuthenticatedPeerPackets,
        wire::AudioPathDiagnostic::PeerProbesMissing,
        wire::AudioPathDiagnostic::ProbeResponsesMissing,
        wire::AudioPathDiagnostic::AuthenticationFailures,
    ] {
        value.sessions[0].path_diagnostic = diagnostic as i32;
        for (local, peer) in [(0, 0), (0, 47903), (47903, 0)] {
            value.sessions[0].local_udp_port = local;
            value.sessions[0].peer_udp_port = peer;
            assert_eq!(
                validate_status(&value).is_ok(),
                diagnostic == wire::AudioPathDiagnostic::None
            );
        }
        for (local, peer) in [(1, 65535), (47901, 47903)] {
            value.sessions[0].local_udp_port = local;
            value.sessions[0].peer_udp_port = peer;
            assert!(validate_status(&value).is_ok());
        }
        for invalid in [47902, 65536, u32::MAX] {
            value.sessions[0].local_udp_port = invalid;
            value.sessions[0].peer_udp_port = 47903;
            assert!(validate_status(&value).is_err());
            value.sessions[0].local_udp_port = 47903;
            value.sessions[0].peer_udp_port = invalid;
            assert!(validate_status(&value).is_err());
        }
    }
}

#[test]
fn status_path_and_remote_quality_roundtrip_without_changing_local_health() {
    let mut value = session_status();
    let encoded = value.encode_to_vec();
    let legacy = wire::AudioStatus::decode(encoded.as_slice()).unwrap();
    assert_eq!(legacy.sessions[0].path_diagnostic, 0);
    assert_eq!(legacy.sessions[0].local_udp_port, 0);
    assert_eq!(legacy.sessions[0].peer_udp_port, 0);
    assert!(legacy.sessions[0].remote_quality.is_none());
    assert!(validate_status(&legacy).is_ok());

    for diagnostic in [
        wire::AudioPathDiagnostic::None,
        wire::AudioPathDiagnostic::NoAuthenticatedPeerPackets,
        wire::AudioPathDiagnostic::PeerProbesMissing,
        wire::AudioPathDiagnostic::ProbeResponsesMissing,
        wire::AudioPathDiagnostic::AuthenticationFailures,
    ] {
        let session = &mut value.sessions[0];
        session.path_diagnostic = diagnostic as i32;
        session.local_udp_port = 47903;
        session.peer_udp_port = 65535;
        session.remote_quality = Some(wire::AudioRemoteQuality {
            received_packets: u64::MAX,
            lost_frames: 2,
            late_frames: 3,
        });
        let decoded = wire::AudioStatus::decode(value.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, value);
        assert!(validate_status(&decoded).is_ok());
        assert_eq!(
            decoded.sessions[0].health_state,
            wire::AudioStreamHealth::Ready as i32
        );
        assert_eq!(
            decoded.sessions[0].health,
            Some(wire::AudioHealth::default())
        );
    }
    value.sessions[0].remote_quality = Some(wire::AudioRemoteQuality::default());
    assert!(validate_status(&value).is_ok());
}

#[test]
fn latency_stages_are_bounded_and_preserve_native_depth_presence() {
    let mut value = session_status();
    assert!(validate_status(&value).is_ok());
    assert!(value.sessions[0].latency_stages.is_none());
    for stage in 0..9 {
        for delay in [0, 2_000_000, 2_000_001, u32::MAX] {
            let mut delays = [0; 9];
            delays[stage] = delay;
            value.sessions[0].latency_stages = Some(wire::AudioLatencyDiagnostics {
                packetization_us: delays[0],
                capture_queue_us: delays[1],
                codec_lookahead_us: delays[2],
                jitter_target_us: delays[3],
                jitter_buffer_us: delays[4],
                decoded_queue_us: delays[5],
                render_queue_us: delays[6],
                resampler_delay_us: delays[7],
                native_render_buffer_us: Some(delays[8]),
            });
            assert_eq!(validate_status(&value).is_ok(), delay <= 2_000_000);
            let decoded = wire::AudioStatus::decode(value.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded, value);
        }
    }
    value.sessions[0].latency_stages = Some(wire::AudioLatencyDiagnostics::default());
    assert!(validate_status(&value).is_ok());
    let decoded = wire::AudioStatus::decode(value.encode_to_vec().as_slice()).unwrap();
    assert_eq!(
        decoded.sessions[0]
            .latency_stages
            .as_ref()
            .unwrap()
            .native_render_buffer_us,
        None
    );
    value.sessions[0]
        .latency_stages
        .as_mut()
        .unwrap()
        .native_render_buffer_us = Some(0);
    let decoded = wire::AudioStatus::decode(value.encode_to_vec().as_slice()).unwrap();
    assert_eq!(
        decoded.sessions[0]
            .latency_stages
            .as_ref()
            .unwrap()
            .native_render_buffer_us,
        Some(0)
    );
}

#[test]
fn system_sharing_on_requires_role_target_revision_and_correlation() {
    use wire::gui_frame::Msg;
    for role in [wire::AudioRole::Source, wire::AudioRole::Listener] {
        let request = wire::SetSystemAudioSharing {
            enabled: true,
            role: role as i32,
            listener_node_id: if role == wire::AudioRole::Source {
                vec![2; 32]
            } else {
                Vec::new()
            },
            expected_revision: 1,
        };
        let valid = frame(Msg::SetSystemAudioSharing(request.clone()));
        assert!(validate_frame(&valid).is_ok());
        assert_eq!(
            wire::GuiFrame::decode(valid.encode_to_vec().as_slice()).unwrap(),
            valid
        );
        for revision in [1, u64::MAX] {
            let mut value = request.clone();
            value.expected_revision = revision;
            assert!(validate_frame(&frame(Msg::SetSystemAudioSharing(value))).is_ok());
        }
        let mut value = request.clone();
        value.expected_revision = 0;
        assert!(validate_frame(&frame(Msg::SetSystemAudioSharing(value))).is_err());
        for invalid_role in [0, -1, 3, i32::MAX] {
            let mut value = request.clone();
            value.role = invalid_role;
            assert!(validate_frame(&frame(Msg::SetSystemAudioSharing(value))).is_err());
        }
        for length in [0, 1, 31, 32, 33] {
            let mut value = request.clone();
            value.listener_node_id = vec![2; length];
            assert_eq!(
                validate_frame(&frame(Msg::SetSystemAudioSharing(value))).is_ok(),
                if role == wire::AudioRole::Source {
                    length == 32
                } else {
                    length == 0
                }
            );
        }
        let mut uncorrelated = valid;
        uncorrelated.request_id = 0;
        assert!(validate_frame(&uncorrelated).is_err());
    }
}

#[test]
fn system_sharing_off_ignores_stale_preferences_but_retains_frame_bounds() {
    use wire::gui_frame::Msg;
    for role in [0, -1, 1, 2, i32::MAX] {
        for expected_revision in [0, 1, u64::MAX] {
            for length in [0, 1, 31, 32, 33] {
                let value = wire::SetSystemAudioSharing {
                    enabled: false,
                    role,
                    listener_node_id: vec![2; length],
                    expected_revision,
                };
                assert!(validate_frame(&frame(Msg::SetSystemAudioSharing(value))).is_ok());
            }
        }
    }
    let mut value = frame(Msg::SetSystemAudioSharing(
        wire::SetSystemAudioSharing::default(),
    ));
    value.request_id = 0;
    assert!(validate_frame(&value).is_err());
    let oversized = frame(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
        listener_node_id: vec![2; 65536],
        ..Default::default()
    }));
    assert!(validate_frame(&oversized).is_err());
    assert!(protocol::encode_message(protocol::Channel::Ipc, &oversized).is_err());
}

#[test]
fn system_sharing_presence_preserves_legacy_config_limits_and_pending_auto_group() {
    for system_sharing in [None, Some(false), Some(true)] {
        for role in [wire::AudioRole::Source, wire::AudioRole::Listener] {
            let mut value = config();
            value.enabled = true;
            value.role = role as i32;
            value.system_sharing = system_sharing;
            if role == wire::AudioRole::Source {
                value.listener_node_id = vec![2; 32];
            }
            assert!(value.group_id.is_empty());
            assert!(validate_config(&value).is_ok());
            let decoded = wire::AudioConfig::decode(value.encode_to_vec().as_slice()).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(decoded.system_sharing, system_sharing);
            value.udp_port = 47902;
            assert!(validate_config(&value).is_err());
        }
        let mut value = status();
        value.system_sharing_requested = system_sharing;
        assert!(validate_status(&value).is_ok());
        let decoded = wire::AudioStatus::decode(value.encode_to_vec().as_slice()).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded.system_sharing_requested, system_sharing);
    }
}

#[test]
fn system_overrides_preserve_presence_and_cannot_bypass_role_or_source_target() {
    for role in [wire::AudioRole::Source, wire::AudioRole::Listener] {
        for send in [None, Some(false), Some(true)] {
            for receive in [None, Some(false), Some(true)] {
                let mut value = config();
                value.role = role as i32;
                value.listener_node_id = vec![2; 32];
                value.peers.push(wire::AudioPeerConfig {
                    node_id: vec![2; 32],
                    system_send_override: send,
                    system_receive_override: receive,
                    volume_percent: 100,
                    ..Default::default()
                });
                let valid = match role {
                    wire::AudioRole::Source => receive != Some(true),
                    wire::AudioRole::Listener => send != Some(true),
                    wire::AudioRole::Unspecified => unreachable!(),
                };
                assert_eq!(validate_config(&value).is_ok(), valid);
                let decoded = wire::AudioConfig::decode(value.encode_to_vec().as_slice()).unwrap();
                assert_eq!(decoded, value);
                value.peers[0].node_id = vec![3; 32];
                assert_eq!(
                    validate_config(&value).is_ok(),
                    valid && (role != wire::AudioRole::Source || send != Some(true))
                );
            }
        }
    }
    let mut value = config();
    value.peers.push(wire::AudioPeerConfig {
        node_id: vec![2; 32],
        system_receive_override: Some(true),
        volume_percent: 101,
        ..Default::default()
    });
    assert!(validate_config(&value).is_err());
    // An explicit deny does not relax Advanced's preexisting structural checks.
    value.peers[0].volume_percent = 100;
    value.peers[0].allow_system_send = true;
    value.peers[0].system_send_override = Some(false);
    assert!(validate_config(&value).is_err());
}

#[tokio::test]
async fn gui_link_validates_system_sharing_in_both_directions() {
    use wire::gui_frame::Msg;
    let (a, b) = tokio::io::duplex(4096);
    let mut sender = ipc::GuiLink::new(a);
    let mut receiver = ipc::GuiLink::new(b);
    let invalid = wire::SetSystemAudioSharing {
        enabled: true,
        ..Default::default()
    };
    assert!(sender
        .send(1, Msg::SetSystemAudioSharing(invalid.clone()))
        .await
        .is_err());
    assert_eq!(sender.outbound_next(), 1);
    sender
        .send(
            1,
            Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing::default()),
        )
        .await
        .unwrap();
    assert_eq!(receiver.recv().await.unwrap().seq, 1);
    sender
        .framed()
        .send(&wire::GuiFrame {
            seq: 2,
            request_id: 2,
            msg: Some(Msg::SetSystemAudioSharing(invalid)),
        })
        .await
        .unwrap();
    assert!(receiver.recv().await.is_err());
    assert_eq!(receiver.inbound_expected(), 2);
}

#[test]
fn system_sharing_status_error_is_only_a_bounded_existing_typed_code() {
    use ipc::audio::codes;
    for code in [
        "",
        codes::AUDIO_CONSENT_REQUIRED,
        codes::AUDIO_ROLE_CONFLICT,
        codes::AUDIO_GROUP_MISMATCH,
        codes::AUDIO_UNSUPPORTED_BACKEND,
        codes::AUDIO_NO_UDP_PATH,
        codes::AUDIO_DEVICE_UNAVAILABLE,
        codes::AUDIO_VIRTUAL_MIC_MISSING,
        codes::AUDIO_CONFIG_WRITE,
        codes::AUDIO_REVISION_CONFLICT,
        codes::AUDIO_STALE_MEDIA,
        codes::AUDIO_UNSUPPORTED_COMMAND,
        codes::AUDIO_UNAVAILABLE,
        codes::AUDIO_START_RATE_LIMITED,
        codes::AUDIO_WORKER_RESTARTING,
    ] {
        let mut value = status();
        value.system_sharing_error = code.into();
        assert!(validate_status(&value).is_ok());
        let encoded = frame(wire::gui_frame::Msg::AudioStatus(value));
        let restored = wire::GuiFrame::decode(encoded.encode_to_vec().as_slice()).unwrap();
        assert_eq!(restored, encoded);
        assert!(validate_frame(&restored).is_ok());
    }
    for code in [
        "AUDIO_UNKNOWN_CODE".to_owned(),
        "device secret payload".to_owned(),
        "AUDIO_UNAVAILABLE\n".to_owned(),
        "audio_unavailable".to_owned(),
        "AUDIO_".to_owned(),
        "AUDIO_é".to_owned(),
        "A".repeat(65),
        "é".repeat(33),
    ] {
        let mut value = status();
        value.system_sharing_error = code;
        assert!(validate_status(&value).is_err());
        assert!(validate_frame(&frame(wire::gui_frame::Msg::AudioStatus(value))).is_err());
    }
}

#[test]
fn advanced_audio_config_remains_strict_when_system_sharing_is_off() {
    for system_sharing in [None, Some(false), Some(true)] {
        let mut value = config();
        value.system_sharing = system_sharing;
        value.role = -1;
        assert!(
            validate_frame(&frame(wire::gui_frame::Msg::SetAudio(wire::SetAudio {
                config: Some(value),
                expected_revision: 1,
            })))
            .is_err()
        );
        let mut value = config();
        value.system_sharing = system_sharing;
        value.listener_node_id = vec![2; 31];
        assert!(
            validate_frame(&frame(wire::gui_frame::Msg::SetAudio(wire::SetAudio {
                config: Some(value),
                expected_revision: 1,
            })))
            .is_err()
        );
    }
}
