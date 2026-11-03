use prost::Message;
use protocol::ipc::{self as wire, gui_frame::Msg};

#[test]
fn audio_tags_are_append_only_and_round_trip() {
    let messages = [
        Msg::GetAudioStatus(wire::GetAudioStatus {}),
        Msg::SetAudio(wire::SetAudio::default()),
        Msg::StartAudio(wire::StartAudio::default()),
        Msg::StopAudio(wire::StopAudio::default()),
        Msg::SetMicrophoneMute(wire::SetMicrophoneMute::default()),
        Msg::SetPtt(wire::SetPtt::default()),
        Msg::AudioStatus(wire::AudioStatus::default()),
        Msg::ListAudioDevices(wire::ListAudioDevices::default()),
        Msg::AudioDevices(wire::AudioDevices::default()),
        Msg::SetAudioPeerGain(wire::SetAudioPeerGain::default()),
        Msg::SendMediaCommand(wire::SendMediaCommand::default()),
        Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing::default()),
        Msg::GetAudioArtwork(wire::GetAudioArtwork::default()),
        Msg::AudioArtwork(wire::AudioArtwork::default()),
    ];
    for (index, message) in messages.into_iter().enumerate() {
        let frame = wire::GuiFrame {
            seq: 0,
            request_id: 0,
            msg: Some(message),
        };
        let encoded = frame.encode_to_vec();
        let key = ((61 + index) << 3) | 2;
        assert_eq!(
            &encoded[..2],
            &[(key as u8 & 0x7f) | 0x80, (key >> 7) as u8]
        );
        assert_eq!(wire::GuiFrame::decode(encoded.as_slice()).unwrap(), frame);
    }
    let existing = wire::GuiFrame {
        seq: 0,
        request_id: 0,
        msg: Some(Msg::SetClipboardImages(wire::SetClipboardImages::default())),
    };
    assert_eq!(&existing.encode_to_vec()[..2], &[0xe2, 0x03]);
}
#[test]
fn media_debug_is_redacted_in_nested_frames_and_presence_survives() {
    let media = wire::MediaInfo {
        app_id: "secret-app".into(),
        title: "secret-title".into(),
        subtitle: "secret-subtitle".into(),
        position_ms: Some(0),
        duration_ms: None,
        ..Default::default()
    };
    assert_eq!(
        wire::MediaInfo::decode(media.encode_to_vec().as_slice()).unwrap(),
        media
    );
    let frame = wire::GuiFrame {
        msg: Some(Msg::AudioStatus(wire::AudioStatus {
            sessions: vec![wire::AudioSessionStatus {
                media_info: Some(media),
                ..Default::default()
            }],
            ..Default::default()
        })),
        ..Default::default()
    };
    let debug = format!("{frame:?}");
    for secret in ["secret-app", "secret-title", "secret-subtitle"] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("REDACTED"));
}
#[test]
fn unknown_future_oneof_is_absent_and_ipc_limit_stays_64k() {
    // Field 2047 is outside the allocated oneof range, including artwork at 73/74.
    assert!(wire::GuiFrame::decode(&[0xfa, 0x7f, 0x00][..])
        .unwrap()
        .msg
        .is_none());
    assert_eq!(protocol::Channel::Ipc.max_frame_len(), 65536);
    let frame = wire::GuiFrame {
        msg: Some(Msg::AudioDevices(wire::AudioDevices {
            devices: vec![wire::AudioDevice {
                name: "x".repeat(65536),
                ..Default::default()
            }],
            ..Default::default()
        })),
        ..Default::default()
    };
    assert!(protocol::encode_message(protocol::Channel::Ipc, &frame).is_err());
}

#[test]
fn system_sharing_optional_tags_preserve_absent_false_and_true() {
    for (value, encoded_bool) in [(None, None), (Some(false), Some(0)), (Some(true), Some(1))] {
        let config = wire::AudioConfig {
            system_sharing: value,
            ..Default::default()
        };
        let encoded = config.encode_to_vec();
        assert_eq!(
            encoded,
            encoded_bool.map_or_else(Vec::new, |v| vec![0x98, 0x01, v])
        );
        assert_eq!(
            wire::AudioConfig::decode(encoded.as_slice()).unwrap(),
            config
        );

        let status = wire::AudioStatus {
            system_sharing_requested: value,
            ..Default::default()
        };
        let encoded = status.encode_to_vec();
        assert_eq!(
            encoded,
            encoded_bool.map_or_else(Vec::new, |v| vec![0x48, v])
        );
        assert_eq!(
            wire::AudioStatus::decode(encoded.as_slice()).unwrap(),
            status
        );

        for send in [None, Some(false), Some(true)] {
            let peer = wire::AudioPeerConfig {
                volume_percent: 73,
                system_send_override: send,
                system_receive_override: value,
                ..Default::default()
            };
            let mut expected = vec![0x40, 73]; // Existing volume_percent stays field 8.
            if let Some(send) = send {
                expected.extend_from_slice(&[0x48, u8::from(send)]);
            }
            if let Some(receive) = value {
                expected.extend_from_slice(&[0x50, u8::from(receive)]);
            }
            let encoded = peer.encode_to_vec();
            assert_eq!(encoded, expected);
            assert_eq!(
                wire::AudioPeerConfig::decode(encoded.as_slice()).unwrap(),
                peer
            );
            let legacy = LegacyAudioPeer::decode(encoded.as_slice()).unwrap();
            assert_eq!(legacy.volume_percent, 73);
            let restored =
                wire::AudioPeerConfig::decode(legacy.encode_to_vec().as_slice()).unwrap();
            assert_eq!(restored.system_send_override, None);
            assert_eq!(restored.system_receive_override, None);
        }
    }
}

#[derive(Clone, PartialEq, Message)]
struct LegacyAudioPeer {
    #[prost(uint32, tag = "8")]
    volume_percent: u32,
}

#[derive(Clone, PartialEq, Message)]
struct LegacyGuiFrame {
    #[prost(uint64, tag = "1")]
    seq: u64,
    #[prost(uint64, tag = "2")]
    request_id: u64,
    #[prost(oneof = "legacy_gui_frame::Msg", tags = "62")]
    msg: Option<legacy_gui_frame::Msg>,
}

mod legacy_gui_frame {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Msg {
        #[prost(message, tag = "62")]
        SetAudio(protocol::ipc::SetAudio),
    }
}

#[test]
fn old_peer_ignores_sharing_request_without_interpreting_it_as_set_audio() {
    for enabled in [false, true] {
        let frame = wire::GuiFrame {
            seq: 7,
            request_id: 9,
            msg: Some(Msg::SetSystemAudioSharing(wire::SetSystemAudioSharing {
                enabled,
                role: wire::AudioRole::Source as i32,
                listener_node_id: vec![2; 32],
                expected_revision: 11,
            })),
        };
        let legacy = LegacyGuiFrame::decode(frame.encode_to_vec().as_slice()).unwrap();
        assert_eq!(legacy.seq, 7);
        assert_eq!(legacy.request_id, 9);
        assert!(legacy.msg.is_none());
        let restored = wire::GuiFrame::decode(legacy.encode_to_vec().as_slice()).unwrap();
        assert!(restored.msg.is_none());
    }
    let advanced = wire::GuiFrame {
        msg: Some(Msg::SetAudio(wire::SetAudio::default())),
        ..Default::default()
    };
    assert!(matches!(
        LegacyGuiFrame::decode(advanced.encode_to_vec().as_slice())
            .unwrap()
            .msg,
        Some(legacy_gui_frame::Msg::SetAudio(_))
    ));
}
