use prost::Message;
use protocol::{
    limits,
    validate::{audio_control, audio_format, AudioViolation},
    wire,
};
use wire::audio_control::Body;

fn format(wifi: bool) -> wire::AudioFormat {
    wire::AudioFormat {
        media_version: 1,
        codec: wire::AudioCodec::Opus as i32,
        sample_rate: 48_000,
        channels: 2,
        frame_samples: if wifi { 960 } else { 480 },
        bitrate_bps: if wifi { 64_000 } else { 96_000 },
        fec: wifi,
        profile: if wifi {
            wire::AudioLatencyProfile::Wifi
        } else {
            wire::AudioLatencyProfile::Ethernet
        } as i32,
        max_datagram_bytes: 1_200,
        aead: wire::AudioAead::Chacha20Poly1305 as i32,
    }
}
fn control(body: Body) -> wire::AudioControl {
    wire::AudioControl {
        control_version: 1,
        body: Some(body),
    }
}
fn hello() -> wire::AudioHello {
    wire::AudioHello {
        group_id: vec![1; 16],
        listener_node_id: vec![2; 32],
        role: wire::AudioNodeRole::Listener as i32,
        udp_port: 47903,
        media_versions: vec![1],
        consent_revision: 0,
        directions: 2,
        listener_available: true,
    }
}
fn offer() -> wire::AudioOffer {
    wire::AudioOffer {
        request_id: Vec::new(),
        session_id: vec![1; 16],
        session_nonce: vec![2; 32],
        parent_connection_id: vec![3; 16],
        group_id: vec![4; 16],
        listener_node_id: vec![5; 32],
        source_node_id: vec![6; 32],
        kind: wire::AudioStreamKind::System as i32,
        format: Some(format(false)),
        udp_port: 65535,
        consent_revision: 0,
        key_epoch: 1,
        control_ttl_ms: 3000,
    }
}

fn start_request() -> wire::AudioStartRequest {
    wire::AudioStartRequest {
        request_id: vec![7; 16],
        group_id: vec![4; 16],
        source_node_id: vec![6; 32],
        consent_revision: 2,
        kind: wire::AudioStreamKind::System as i32,
        profile: wire::AudioLatencyProfile::Ethernet as i32,
    }
}

#[test]
fn stable_tags_enums_and_roundtrips() {
    assert_eq!(limits::PROTOCOL_MINOR, 0);
    assert_eq!(wire::Capability::AudioControlV1 as i32, 16);
    assert_eq!(wire::Capability::SystemAudioSource as i32, 17);
    assert_eq!(wire::Capability::AudioFocusReportV1 as i32, 19);
    assert_eq!(wire::Capability::AudioListener as i32, 20);
    for value in [21, 22] {
        assert!(wire::Capability::try_from(value).is_err());
    }
    assert_eq!(wire::AudioNodeRole::Source as i32, 1);
    assert_eq!(wire::AudioNodeRole::Listener as i32, 2);
    assert_eq!(wire::AudioStreamKind::System as i32, 1);
    assert!(wire::AudioStreamKind::try_from(2).is_err());
    let reasons = [
        wire::AudioStopCode::Disabled,
        wire::AudioStopCode::ConsentRequired,
        wire::AudioStopCode::RoleConflict,
        wire::AudioStopCode::UnsupportedFormat,
        wire::AudioStopCode::DeviceUnavailable,
        wire::AudioStopCode::VirtualMicMissing,
        wire::AudioStopCode::ParentLost,
        wire::AudioStopCode::WorkerLost,
        wire::AudioStopCode::Timeout,
        wire::AudioStopCode::RateLimited,
        wire::AudioStopCode::Revoked,
        wire::AudioStopCode::UserStop,
        wire::AudioStopCode::ProtocolViolation,
        wire::AudioStopCode::GroupMismatch,
    ];
    for (index, reason) in reasons.into_iter().enumerate() {
        assert_eq!(reason as usize, index + 1);
    }
    let cases = [
        (10, Body::Hello(hello())),
        (11, Body::Offer(offer())),
        (
            12,
            Body::Answer(wire::AudioAnswer {
                session_id: vec![1; 16],
                accepted: true,
                accepted_format: Some(format(true)),
                udp_port: 1,
                source_nonce: vec![7; 32],
                ..Default::default()
            }),
        ),
        (
            13,
            Body::Commit(wire::AudioCommit {
                session_id: vec![1; 16],
                transcript_sha256: vec![2; 32],
                key_epoch: 1,
                start_muted: true,
            }),
        ),
        (
            14,
            Body::Stop(wire::AudioStop {
                session_id: vec![1; 16],
                reason: 12,
            }),
        ),
        (
            15,
            Body::Keepalive(wire::AudioKeepalive {
                session_id: vec![1; 16],
                control_serial: u64::MAX,
                authorized_for_ms: 3000,
            }),
        ),
        (
            16,
            Body::Report(wire::AudioReport {
                session_id: vec![1; 16],
                received_packets: u64::MAX,
                lost_packets: u64::MAX,
                buffer_ms: 1000,
                underruns: u32::MAX,
                suggested_loss_pct: 100,
                device_state: wire::AudioDeviceState::Ready as i32,
            }),
        ),
        (17, Body::Focus(focus_snapshot())),
        (
            21,
            Body::FocusRequest(wire::AudioFocusRequest {
                group_id: vec![4; 16],
            }),
        ),
        (
            20,
            Body::Consent(wire::AudioConsentChanged {
                group_id: vec![1; 16],
                consent_revision: u32::MAX,
                allowed_directions: 3,
            }),
        ),
        (25, Body::StartRequest(start_request())),
        (
            26,
            Body::StartRefusal(wire::AudioStartRefusal {
                request_id: vec![7; 16],
                reason: wire::AudioStopCode::ConsentRequired as i32,
            }),
        ),
    ];
    for (tag, body) in cases {
        let message = control(body);
        assert_eq!(audio_control(&message), Ok(()));
        let bytes = message.encode_to_vec();
        assert_eq!(&bytes[..2], &[8, 1]);
        let key = (tag << 3) | 2;
        if key < 128 {
            assert_eq!(u32::from(bytes[2]), key);
        } else {
            assert_eq!(&bytes[2..4], &[(key as u8 & 127) | 128, (key >> 7) as u8]);
        }
        assert_eq!(
            wire::AudioControl::decode(bytes.as_slice()).unwrap(),
            message
        );
        let envelope = wire::Envelope {
            payload: Some(wire::envelope::Payload::AudioControl(message)),
            ..Default::default()
        };
        assert_eq!(&envelope.encode_to_vec()[..2], &[0xda, 0x03]); // tag59
    }
    assert!(hello().encode_to_vec().ends_with(&[0x40, 1])); // listener_available field8
}

#[test]
fn format_is_exactly_one_of_two_system_profiles() {
    for wifi in [false, true] {
        let good = format(wifi);
        assert_eq!(audio_format(&good), Ok(()));
        let bad = [
            wire::AudioFormat {
                media_version: 0,
                ..good
            },
            wire::AudioFormat {
                media_version: 2,
                ..good
            },
            wire::AudioFormat { codec: 0, ..good },
            wire::AudioFormat { codec: 99, ..good },
            wire::AudioFormat {
                sample_rate: 47_999,
                ..good
            },
            wire::AudioFormat {
                channels: 1,
                ..good
            },
            wire::AudioFormat {
                frame_samples: 481,
                ..good
            },
            wire::AudioFormat {
                bitrate_bps: good.bitrate_bps + 1,
                ..good
            },
            wire::AudioFormat { fec: !wifi, ..good },
            wire::AudioFormat { profile: 0, ..good },
            wire::AudioFormat {
                profile: 99,
                ..good
            },
            wire::AudioFormat {
                max_datagram_bytes: 1199,
                ..good
            },
            wire::AudioFormat {
                max_datagram_bytes: 1201,
                ..good
            },
            wire::AudioFormat { aead: 0, ..good },
            wire::AudioFormat { aead: 99, ..good },
        ];
        for value in bad {
            assert_eq!(audio_format(&value), Err(AudioViolation::UnsupportedFormat));
        }
    }
}

#[test]
fn hello_list_identity_roles_directions_and_port_bounds() {
    for versions in [vec![], vec![0, 1], vec![1, 1], vec![2], vec![1, 2, 3, 4, 5]] {
        assert_eq!(
            audio_control(&control(Body::Hello(wire::AudioHello {
                media_versions: versions,
                ..hello()
            }))),
            Err(AudioViolation::MediaVersions)
        );
    }
    assert!(audio_control(&control(Body::Hello(wire::AudioHello {
        media_versions: vec![1, 2, 3, 4],
        ..hello()
    })))
    .is_ok());
    for role in [0, 3, 999] {
        assert_eq!(
            audio_control(&control(Body::Hello(wire::AudioHello { role, ..hello() }))),
            Err(AudioViolation::Role)
        );
    }
    for port in [0, 65536, u32::MAX] {
        assert_eq!(
            audio_control(&control(Body::Hello(wire::AudioHello {
                udp_port: port,
                ..hello()
            }))),
            Err(AudioViolation::Port)
        );
    }
    for value in 0..=3 {
        assert!(audio_control(&control(Body::Hello(wire::AudioHello {
            directions: value,
            ..hello()
        })))
        .is_ok());
    }
    assert_eq!(
        audio_control(&control(Body::Hello(wire::AudioHello {
            directions: 4,
            ..hello()
        }))),
        Err(AudioViolation::Directions)
    );
    let mut source = hello();
    source.role = 1;
    source.group_id.clear();
    assert_eq!(audio_control(&control(Body::Hello(source.clone()))), Ok(()));
    source.role = 2;
    assert_eq!(
        audio_control(&control(Body::Hello(source))),
        Err(AudioViolation::Identifier)
    );
    for len in [0, 15, 17] {
        assert_eq!(
            audio_control(&control(Body::Hello(wire::AudioHello {
                group_id: vec![0; len],
                ..hello()
            }))),
            Err(AudioViolation::Identifier)
        );
    }
    for len in [0, 31, 33] {
        assert_eq!(
            audio_control(&control(Body::Hello(wire::AudioHello {
                listener_node_id: vec![0; len],
                ..hello()
            }))),
            Err(AudioViolation::Identifier)
        );
    }
}

#[test]
fn setup_requires_ids_contributory_nonces_format_epoch_ttl_and_typed_refusal() {
    let good = offer();
    for len in [0, 31, 33] {
        let bad = wire::AudioOffer {
            session_nonce: vec![1; len],
            ..good.clone()
        };
        assert_eq!(
            audio_control(&control(Body::Offer(bad))),
            Err(AudioViolation::Nonce)
        );
    }
    for bad in [
        wire::AudioOffer {
            session_id: vec![],
            ..good.clone()
        },
        wire::AudioOffer {
            parent_connection_id: vec![],
            ..good.clone()
        },
        wire::AudioOffer {
            group_id: vec![],
            ..good.clone()
        },
        wire::AudioOffer {
            listener_node_id: vec![],
            ..good.clone()
        },
        wire::AudioOffer {
            source_node_id: vec![],
            ..good.clone()
        },
    ] {
        assert_eq!(
            audio_control(&control(Body::Offer(bad))),
            Err(AudioViolation::Identifier)
        );
    }
    for value in [0, 2, u32::MAX] {
        assert_eq!(
            audio_control(&control(Body::Offer(wire::AudioOffer {
                key_epoch: value,
                ..good.clone()
            }))),
            Err(AudioViolation::Epoch)
        );
    }
    for value in [0, 2999, 3001] {
        assert_eq!(
            audio_control(&control(Body::Offer(wire::AudioOffer {
                control_ttl_ms: value,
                ..good.clone()
            }))),
            Err(AudioViolation::Ttl)
        );
    }
    assert_eq!(
        audio_control(&control(Body::Offer(wire::AudioOffer {
            format: None,
            ..good.clone()
        }))),
        Err(AudioViolation::UnsupportedFormat)
    );
    for kind in [0, 2, 99] {
        assert_eq!(
            audio_control(&control(Body::Offer(wire::AudioOffer {
                kind,
                ..good.clone()
            }))),
            Err(AudioViolation::UnsupportedFormat)
        );
    }
    let mut answer = wire::AudioAnswer {
        session_id: vec![1; 16],
        reason: 2,
        ..Default::default()
    };
    assert_eq!(
        audio_control(&control(Body::Answer(answer.clone()))),
        Ok(())
    );
    for reason in [0, -1, i32::MAX] {
        answer.reason = reason;
        assert_eq!(
            audio_control(&control(Body::Answer(answer.clone()))),
            Err(AudioViolation::Reason)
        );
    }
    answer.reason = 2;
    answer.source_nonce = vec![1; 32];
    assert_eq!(
        audio_control(&control(Body::Answer(answer.clone()))),
        Err(AudioViolation::Reason)
    );
    answer.source_nonce.clear();
    answer.udp_port = 1;
    assert_eq!(
        audio_control(&control(Body::Answer(answer.clone()))),
        Err(AudioViolation::Reason)
    );
    answer.udp_port = 0;
    answer.accepted_format = Some(format(false));
    assert_eq!(
        audio_control(&control(Body::Answer(answer.clone()))),
        Err(AudioViolation::Reason)
    );
    answer.accepted = true;
    answer.reason = 0;
    answer.udp_port = 1;
    assert_eq!(
        audio_control(&control(Body::Answer(answer.clone()))),
        Err(AudioViolation::Nonce)
    );
    answer.source_nonce = vec![2; 32];
    assert_eq!(audio_control(&control(Body::Answer(answer))), Ok(()));
}

#[test]
fn complete_encoded_ceiling_precedes_scans_and_unknown_body_is_audio_only() {
    let unknown = wire::AudioControl::decode(&[8, 1, 0xfa, 3, 0][..]).unwrap(); // unknown body63
    assert_eq!(audio_control(&unknown), Err(AudioViolation::MissingBody));
    assert_eq!(
        audio_control(&wire::AudioControl {
            control_version: 2,
            body: None
        }),
        Err(AudioViolation::Version)
    );
    // Find exact encoded boundary using an existing bytes field; structural failure
    // below the ceiling must differ from the size failure above it.
    let mut message = control(Body::Hello(hello()));
    if let Some(Body::Hello(hello)) = &mut message.body {
        hello.group_id.resize(1986, 0);
    }
    while message.encoded_len() < 2048 {
        if let Some(Body::Hello(hello)) = &mut message.body {
            hello.group_id.push(0);
        }
    }
    assert_eq!(message.encoded_len(), 2048);
    assert_eq!(audio_control(&message), Err(AudioViolation::Identifier));
    if let Some(Body::Hello(hello)) = &mut message.body {
        hello.group_id.push(0);
    }
    assert_eq!(message.encoded_len(), 2049);
    assert_eq!(audio_control(&message), Err(AudioViolation::TooLarge));
    let huge = control(Body::Hello(wire::AudioHello {
        media_versions: vec![1; 2048],
        ..hello()
    }));
    assert_eq!(audio_control(&huge), Err(AudioViolation::TooLarge));
}

#[test]
fn health_keepalive_stop_and_consent_are_bounded() {
    let report = wire::AudioReport {
        session_id: vec![1; 16],
        device_state: 1,
        ..Default::default()
    };
    for bad in [
        wire::AudioReport {
            buffer_ms: 1001,
            ..report.clone()
        },
        wire::AudioReport {
            suggested_loss_pct: 101,
            ..report.clone()
        },
        wire::AudioReport {
            device_state: 0,
            ..report.clone()
        },
        wire::AudioReport {
            device_state: 99,
            ..report
        },
    ] {
        assert_eq!(
            audio_control(&control(Body::Report(bad))),
            Err(AudioViolation::Health)
        );
    }
    for ttl in [0, 3001] {
        assert_eq!(
            audio_control(&control(Body::Keepalive(wire::AudioKeepalive {
                session_id: vec![1; 16],
                authorized_for_ms: ttl,
                control_serial: 0
            }))),
            Err(AudioViolation::Ttl)
        );
    }
    for reason in [0, -1, i32::MAX] {
        assert_eq!(
            audio_control(&control(Body::Stop(wire::AudioStop {
                session_id: vec![1; 16],
                reason
            }))),
            Err(AudioViolation::Reason)
        );
    }
    assert_eq!(
        audio_control(&control(Body::Consent(wire::AudioConsentChanged {
            group_id: vec![1; 16],
            allowed_directions: 4,
            consent_revision: 0
        }))),
        Err(AudioViolation::Directions)
    );
}

#[test]
fn received_audio_size_counts_only_complete_field59_bodies_including_duplicates() {
    use prost::encoding::{encode_key, encode_varint, WireType};
    use protocol::validate::audio_control_wire_size;
    fn field(tag: u32, len: usize, bytes: &mut Vec<u8>) {
        encode_key(tag, WireType::LengthDelimited, bytes);
        encode_varint(len as u64, bytes);
        bytes.resize(bytes.len() + len, 0);
    }
    let mut bytes = Vec::new();
    field(100, 4096, &mut bytes);
    assert_eq!(audio_control_wire_size(&bytes).unwrap(), None);
    field(59, 2048, &mut bytes);
    assert_eq!(audio_control_wire_size(&bytes).unwrap(), None);
    field(59, 1, &mut bytes);
    assert_eq!(
        audio_control_wire_size(&bytes).unwrap(),
        Some(AudioViolation::TooLarge)
    );
    bytes.pop();
    assert!(audio_control_wire_size(&bytes).is_err());
    assert!(audio_control_wire_size(&[0]).is_err());
    assert!(audio_control_wire_size(&[0x80]).is_err());
}

#[test]
fn request_ids_reasons_and_profiles_are_bounded() {
    for len in [0, 15, 17, 2048] {
        let request = wire::AudioStartRequest {
            request_id: vec![1; len],
            ..start_request()
        };
        assert!(audio_control(&control(Body::StartRequest(request))).is_err());
    }
    for len in [1, 15, 17] {
        assert_eq!(
            audio_control(&control(Body::Offer(wire::AudioOffer {
                request_id: vec![1; len],
                ..offer()
            }))),
            Err(AudioViolation::Identifier)
        );
    }
    assert!(audio_control(&control(Body::Offer(wire::AudioOffer {
        request_id: vec![1; 16],
        ..offer()
    })))
    .is_ok());
    for request in [
        wire::AudioStartRequest {
            group_id: vec![],
            ..start_request()
        },
        wire::AudioStartRequest {
            source_node_id: vec![1; 31],
            ..start_request()
        },
        wire::AudioStartRequest {
            kind: 0,
            ..start_request()
        },
        wire::AudioStartRequest {
            kind: 2,
            ..start_request()
        },
        wire::AudioStartRequest {
            profile: 0,
            ..start_request()
        },
        wire::AudioStartRequest {
            profile: 99,
            ..start_request()
        },
    ] {
        assert!(audio_control(&control(Body::StartRequest(request))).is_err());
    }
    for reason in [0, 99] {
        assert_eq!(
            audio_control(&control(Body::StartRefusal(wire::AudioStartRefusal {
                request_id: vec![1; 16],
                reason,
            }))),
            Err(AudioViolation::Reason)
        );
    }
    assert_eq!(
        audio_control(&control(Body::StartRefusal(wire::AudioStartRefusal {
            request_id: vec![1; 15],
            reason: 2,
        }))),
        Err(AudioViolation::Identifier)
    );
}

fn focus_snapshot() -> wire::AudioFocusSnapshot {
    wire::AudioFocusSnapshot {
        owner_node_id: vec![1; 32],
        owner_serial: 1,
        focus_revision: 1,
        active_node_id: vec![2; 32],
        lease_id: vec![3; 16],
        committed: true,
    }
}

#[test]
fn focus_requires_positive_namespace_and_exact_committed_shapes() {
    let valid = focus_snapshot();
    let check = |snapshot| audio_control(&control(Body::Focus(snapshot)));
    assert_eq!(check(valid.clone()), Ok(()));
    let mut idle = valid.clone();
    idle.active_node_id = idle.owner_node_id.clone();
    idle.lease_id.clear();
    assert_eq!(check(idle.clone()), Ok(()));
    let mut unknown = idle.clone();
    unknown.committed = false;
    unknown.active_node_id.clear();
    assert_eq!(check(unknown.clone()), Ok(()));
    for size in [0, 31, 33] {
        let mut bad = valid.clone();
        bad.owner_node_id.resize(size, 0);
        assert_eq!(check(bad), Err(AudioViolation::Identifier));
        let mut bad = valid.clone();
        bad.active_node_id.resize(size, 0);
        assert_eq!(check(bad), Err(AudioViolation::Identifier));
    }
    for size in [0, 15, 17] {
        let mut bad = valid.clone();
        bad.lease_id.resize(size, 0);
        assert_eq!(check(bad), Err(AudioViolation::Identifier));
        assert_eq!(
            audio_control(&control(Body::FocusRequest(wire::AudioFocusRequest {
                group_id: vec![4; size],
            }))),
            Err(AudioViolation::Identifier)
        );
    }
    for bad in [
        wire::AudioFocusSnapshot {
            owner_serial: 0,
            ..valid.clone()
        },
        wire::AudioFocusSnapshot {
            focus_revision: 0,
            ..valid.clone()
        },
    ] {
        assert_eq!(check(bad), Err(AudioViolation::Epoch));
    }
    idle.lease_id = vec![3; 16];
    assert_eq!(check(idle), Err(AudioViolation::Lease));
    unknown.lease_id = vec![3; 16];
    assert_eq!(check(unknown), Err(AudioViolation::Lease));
    let mut bad = valid;
    bad.committed = false;
    assert_eq!(check(bad), Err(AudioViolation::Lease));
}

#[test]
fn microphone_and_media_append_only_tags_and_roundtrip() {
    assert_eq!(wire::Capability::MicControlV1 as i32, 18);
    assert_eq!(wire::Capability::MediaControlV1 as i32, 23);
    let bodies = [
        (
            18,
            Body::MicState(wire::MicState {
                group_id: vec![1; 16],
                revision: 1,
                ..Default::default()
            }),
        ),
        (
            19,
            Body::MicAck(wire::MicAck {
                group_id: vec![1; 16],
                request_id: vec![2; 16],
                revision: 1,
                result: 0,
            }),
        ),
        (
            22,
            Body::MediaInfo(wire::MediaInfo {
                session_id: vec![3; 16],
                ..Default::default()
            }),
        ),
        (
            23,
            Body::MediaCommand(wire::MediaCommand {
                request_id: vec![2; 16],
                session_id: vec![3; 16],
                token: 1,
                revision: 1,
                command: wire::MediaCommandKind::Play as i32,
                position_ms: None,
            }),
        ),
        (
            24,
            Body::MediaAck(wire::MediaAck {
                request_id: vec![2; 16],
                session_id: vec![3; 16],
                result: 0,
            }),
        ),
        (
            27,
            Body::MicControl(wire::MicControl {
                group_id: vec![1; 16],
                request_id: vec![2; 16],
                expected_revision: 1,
                op: wire::MicControlOp::SetMute as i32,
                muted: true,
                ..Default::default()
            }),
        ),
    ];
    for (tag, body) in bodies {
        let message = control(body);
        assert_eq!(audio_control(&message), Ok(()));
        let encoded = message.encode_to_vec();
        let mut bytes = encoded.as_slice();
        let mut found = false;
        while !bytes.is_empty() {
            let (key, ty) = prost::encoding::decode_key(&mut bytes).unwrap();
            found |= key == tag;
            prost::encoding::skip_field(ty, key, &mut bytes, Default::default()).unwrap();
        }
        assert!(found);
        assert_eq!(
            wire::AudioControl::decode(encoded.as_slice()).unwrap(),
            message
        );
    }
}

#[test]
fn microphone_shapes_and_unknown_control_enums_are_rejected() {
    let press = wire::MicControl {
        group_id: vec![1; 16],
        request_id: vec![2; 16],
        expected_revision: 1,
        op: wire::MicControlOp::Press as i32,
        press_id: vec![3; 16],
        challenge: 1,
        expires_ns: 250_000_000,
        muted: false,
    };
    assert_eq!(
        audio_control(&control(Body::MicControl(press.clone()))),
        Ok(())
    );
    for bad in [
        wire::MicControl {
            group_id: vec![1; 15],
            ..press.clone()
        },
        wire::MicControl {
            request_id: vec![],
            ..press.clone()
        },
        wire::MicControl {
            expected_revision: 0,
            ..press.clone()
        },
        wire::MicControl {
            op: 0,
            ..press.clone()
        },
        wire::MicControl {
            op: 99,
            ..press.clone()
        },
        wire::MicControl {
            muted: true,
            ..press.clone()
        },
        wire::MicControl {
            press_id: vec![],
            ..press.clone()
        },
        wire::MicControl {
            challenge: 0,
            ..press.clone()
        },
        wire::MicControl {
            expires_ns: 0,
            ..press.clone()
        },
        wire::MicControl {
            op: wire::MicControlOp::SetMute as i32,
            ..press.clone()
        },
        wire::MicControl {
            op: wire::MicControlOp::Release as i32,
            ..press
        },
    ] {
        assert!(audio_control(&control(Body::MicControl(bad))).is_err());
    }
    assert_eq!(
        audio_control(&control(Body::MicAck(wire::MicAck {
            group_id: vec![1; 16],
            request_id: vec![2; 16],
            revision: 1,
            result: 99,
        }))),
        Err(AudioViolation::Reason)
    );
}

#[test]
fn emergency_talk_release_is_append_only_and_has_no_press_or_lease_payload() {
    assert_eq!(wire::MicControlOp::ReleaseAll as i32, 5);
    let release = wire::MicControl {
        group_id: vec![1; 16],
        request_id: vec![2; 16],
        expected_revision: 1,
        op: wire::MicControlOp::ReleaseAll as i32,
        ..Default::default()
    };
    let message = control(Body::MicControl(release.clone()));
    assert_eq!(audio_control(&message), Ok(()));
    assert_eq!(
        wire::AudioControl::decode(message.encode_to_vec().as_slice()).unwrap(),
        message
    );
    for bad in [
        wire::MicControl {
            muted: true,
            ..release.clone()
        },
        wire::MicControl {
            press_id: vec![3; 16],
            ..release.clone()
        },
        wire::MicControl {
            challenge: 1,
            ..release.clone()
        },
        wire::MicControl {
            expires_ns: 1,
            ..release
        },
    ] {
        assert_eq!(
            audio_control(&control(Body::MicControl(bad))),
            Err(AudioViolation::Shape)
        );
    }
}

#[test]
fn media_metadata_bounds_clear_shape_and_nested_debug_are_private() {
    let info = wire::MediaInfo {
        session_id: vec![3; 16],
        present: true,
        token: 1,
        revision: 1,
        app: "a".repeat(128),
        title: "é".repeat(128),
        subtitle: "s".repeat(256),
        position_ms: Some(10),
        duration_ms: Some(20),
        playback: wire::MediaPlayback::Playing as i32,
        commands: 31,
        allow_control: false,
        can_seek: None,
        seek_min_ms: None,
        seek_max_ms: None,
    };
    assert_eq!(
        audio_control(&control(Body::MediaInfo(info.clone()))),
        Ok(())
    );
    for bad in [
        wire::MediaInfo {
            app: "a".repeat(129),
            ..info.clone()
        },
        wire::MediaInfo {
            title: "é".repeat(129),
            ..info.clone()
        },
        wire::MediaInfo {
            subtitle: "s".repeat(257),
            ..info.clone()
        },
        wire::MediaInfo {
            title: "private\nmetadata".into(),
            ..info.clone()
        },
        wire::MediaInfo {
            playback: 99,
            ..info.clone()
        },
        wire::MediaInfo {
            commands: 32,
            ..info.clone()
        },
        wire::MediaInfo {
            position_ms: Some(21),
            ..info.clone()
        },
        wire::MediaInfo {
            present: false,
            ..info.clone()
        },
        wire::MediaInfo {
            token: 0,
            ..info.clone()
        },
        wire::MediaInfo {
            revision: 0,
            ..info.clone()
        },
        wire::MediaInfo {
            session_id: vec![],
            ..info
        },
    ] {
        assert!(audio_control(&control(Body::MediaInfo(bad))).is_err());
    }
    let private = wire::MediaInfo {
        session_id: vec![3; 16],
        present: true,
        token: 0x123456789abcdef,
        revision: 1,
        app: "PRIVATE_APP_SENTINEL".into(),
        title: "PRIVATE_TITLE_SENTINEL".into(),
        subtitle: "PRIVATE_SUBTITLE_SENTINEL".into(),
        ..Default::default()
    };
    let debug = format!(
        "{:?}",
        wire::Envelope {
            payload: Some(wire::envelope::Payload::AudioControl(control(
                Body::MediaInfo(private)
            ))),
            ..Default::default()
        }
    );
    assert!(debug.contains("REDACTED"));
    for secret in [
        "PRIVATE_APP_SENTINEL",
        "PRIVATE_TITLE_SENTINEL",
        "PRIVATE_SUBTITLE_SENTINEL",
        "81985529216486895",
    ] {
        assert!(!debug.contains(secret));
    }
    assert_eq!(
        audio_control(&control(Body::MediaInfo(wire::MediaInfo {
            session_id: vec![3; 16],
            revision: 12,
            ..Default::default()
        }))),
        Ok(())
    );
}

#[test]
fn control_identifiers_results_and_media_command_enums_are_strict() {
    let bad_bodies = [
        Body::MicState(wire::MicState {
            group_id: vec![1; 15],
            revision: 1,
            ..Default::default()
        }),
        Body::MicState(wire::MicState {
            group_id: vec![1; 16],
            revision: 1,
            reason: 99,
            ..Default::default()
        }),
        Body::MicState(wire::MicState {
            group_id: vec![1; 16],
            revision: 1,
            controller_node_id: vec![2; 31],
            ..Default::default()
        }),
        Body::MicState(wire::MicState {
            group_id: vec![1; 16],
            revision: 1,
            challenge: 1,
            expires_ns: 0,
            ..Default::default()
        }),
        Body::MicState(wire::MicState {
            group_id: vec![1; 16],
            revision: 1,
            ptt_active: true,
            ..Default::default()
        }),
        Body::MicAck(wire::MicAck {
            group_id: vec![1; 16],
            request_id: vec![2; 15],
            revision: 1,
            result: 0,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 16],
            session_id: vec![2; 15],
            token: 1,
            revision: 1,
            command: 1,
            position_ms: None,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 15],
            session_id: vec![2; 16],
            token: 1,
            revision: 1,
            command: 1,
            position_ms: None,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 16],
            session_id: vec![2; 16],
            token: 0,
            revision: 1,
            command: 1,
            position_ms: None,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 16],
            session_id: vec![2; 16],
            token: 1,
            revision: 0,
            command: 1,
            position_ms: None,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 16],
            session_id: vec![2; 16],
            token: 1,
            revision: 1,
            command: 0,
            position_ms: None,
        }),
        Body::MediaCommand(wire::MediaCommand {
            request_id: vec![1; 16],
            session_id: vec![2; 16],
            token: 1,
            revision: 1,
            command: 99,
            position_ms: None,
        }),
        Body::MediaAck(wire::MediaAck {
            request_id: vec![1; 16],
            session_id: vec![2; 15],
            result: 0,
        }),
        Body::MediaAck(wire::MediaAck {
            request_id: vec![1; 16],
            session_id: vec![2; 16],
            result: 99,
        }),
    ];
    for bad in bad_bodies {
        assert!(audio_control(&control(bad)).is_err());
    }
    for result in 0..=11 {
        assert_eq!(
            audio_control(&control(Body::MicAck(wire::MicAck {
                group_id: vec![1; 16],
                request_id: vec![2; 16],
                revision: 1,
                result,
            }))),
            Ok(())
        );
        assert_eq!(
            audio_control(&control(Body::MediaAck(wire::MediaAck {
                request_id: vec![1; 16],
                session_id: vec![2; 16],
                result,
            }))),
            Ok(())
        );
    }
    for command in 1..=5 {
        assert_eq!(
            audio_control(&control(Body::MediaCommand(wire::MediaCommand {
                request_id: vec![1; 16],
                session_id: vec![2; 16],
                token: 1,
                revision: 1,
                command,
                position_ms: None,
            }))),
            Ok(())
        );
    }
}

#[test]
fn group_mismatch_is_a_typed_refusal_and_stop() {
    let reason = wire::AudioStopCode::GroupMismatch as i32;
    assert_eq!(
        audio_control(&control(Body::Answer(wire::AudioAnswer {
            session_id: vec![1; 16],
            reason,
            ..Default::default()
        }))),
        Ok(())
    );
    assert_eq!(
        audio_control(&control(Body::Stop(wire::AudioStop {
            session_id: vec![1; 16],
            reason,
        }))),
        Ok(())
    );
}

#[test]
fn seek_payload_and_range_admission() {
    let mut command = wire::MediaCommand {
        request_id: vec![1; 16],
        session_id: vec![2; 16],
        token: 3,
        revision: 4,
        command: wire::MediaCommandKind::Seek as i32,
        position_ms: Some(0),
    };
    assert!(audio_control(&control(Body::MediaCommand(command.clone()))).is_ok());
    command.position_ms = None;
    assert_eq!(
        audio_control(&control(Body::MediaCommand(command.clone()))),
        Err(AudioViolation::Shape)
    );
    command.position_ms = Some(0);
    command.command = wire::MediaCommandKind::Play as i32;
    assert_eq!(
        audio_control(&control(Body::MediaCommand(command))),
        Err(AudioViolation::Shape)
    );
    let mut info = wire::MediaInfo {
        session_id: vec![2; 16],
        present: true,
        token: 3,
        revision: 4,
        position_ms: Some(20),
        duration_ms: Some(100),
        can_seek: Some(true),
        seek_min_ms: Some(0),
        seek_max_ms: Some(100),
        ..Default::default()
    };
    assert!(audio_control(&control(Body::MediaInfo(info.clone()))).is_ok());
    for (duration, min, max, position, enabled) in [
        (Some(0), Some(0), Some(0), Some(0), Some(true)),
        (Some(100), Some(90), Some(80), Some(20), Some(true)),
        (Some(100), Some(0), Some(101), Some(20), Some(true)),
        (Some(100), None, Some(100), Some(20), Some(true)),
        (Some(100), Some(0), Some(100), None, Some(true)),
        (Some(100), Some(0), Some(100), Some(20), Some(false)),
    ] {
        info.duration_ms = duration;
        info.seek_min_ms = min;
        info.seek_max_ms = max;
        info.position_ms = position;
        info.can_seek = enabled;
        assert_eq!(
            audio_control(&control(Body::MediaInfo(info.clone()))),
            Err(AudioViolation::Metadata)
        );
    }
}

#[test]
fn legacy_media_decoder_ignores_append_only_seek_fields() {
    #[derive(Clone, PartialEq, Message)]
    struct LegacyCommand {
        #[prost(bytes = "vec", tag = "1")]
        request_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        session_id: Vec<u8>,
        #[prost(uint64, tag = "3")]
        token: u64,
        #[prost(uint64, tag = "4")]
        revision: u64,
        #[prost(int32, tag = "5")]
        command: i32,
    }
    let current = wire::MediaCommand {
        request_id: vec![1; 16],
        session_id: vec![2; 16],
        token: 3,
        revision: 4,
        command: wire::MediaCommandKind::Seek as i32,
        position_ms: Some(0),
    };
    let bytes = current.encode_to_vec();
    let legacy = LegacyCommand::decode(bytes.as_slice()).unwrap();
    assert_eq!(legacy.token, 3);
    assert_eq!(legacy.command, 6);
    assert_eq!(
        wire::MediaCommand::decode(bytes.as_slice())
            .unwrap()
            .position_ms,
        Some(0)
    );
    let legacy = LegacyCommand {
        command: 1,
        ..legacy
    };
    let decoded = wire::MediaCommand::decode(legacy.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.position_ms, None);
    assert!(audio_control(&control(Body::MediaCommand(decoded))).is_ok());
}
