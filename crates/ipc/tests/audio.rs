use ipc::audio::{validate_config, validate_frame, validate_status};
use protocol::ipc as wire;

fn config() -> wire::AudioConfig {
    wire::AudioConfig {
        role: wire::AudioRole::Listener as i32,
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
fn frame(message: wire::gui_frame::Msg) -> wire::GuiFrame {
    wire::GuiFrame {
        seq: 1,
        request_id: 1,
        msg: Some(message),
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
    };
    assert!(validate_frame(&frame(Msg::SendMediaCommand(command.clone()))).is_ok());
    command.command = 0;
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
