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
    // Unknown future field 72 remains an unknown operation, never a default audio request.
    assert!(wire::GuiFrame::decode(&[0xc2, 0x04, 0x00][..])
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
