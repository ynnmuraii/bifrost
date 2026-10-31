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
    for value in [18, 21, 22, 23] {
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
    for reason in [0, 14, 999] {
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
    let unknown = wire::AudioControl::decode(&[8, 1, 0xb2, 1, 0][..]).unwrap(); // unknown body22
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
    for reason in [0, 14, 999] {
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
