use protocol::{ipc, validate, wire, *};

#[test]
fn game_guard_ipc_round_trip_and_append_only_tags() {
    use prost::Message;
    let signals = ipc::GameGuardSignals {
        available: true,
        fullscreen: true,
        cursor_confined: true,
        presentation: true,
        app: "game.exe".into(),
    };
    let frame = ipc::AgentFrame {
        msg: Some(ipc::agent_frame::Msg::GameGuardSignals(signals.clone())),
        ..Default::default()
    };
    assert_eq!(&frame.encode_to_vec()[..2], &[0xd2, 0x03]);
    round_trip(Channel::Ipc, frame);
    round_trip(Channel::Ipc, signals.clone());
    let set = ipc::SetGameGuard {
        mode: "auto".into(),
        block_apps: vec!["game.exe".into()],
        allow_apps: vec!["player.exe".into()],
    };
    let frame = ipc::GuiFrame {
        msg: Some(ipc::gui_frame::Msg::SetGameGuard(set)),
        ..Default::default()
    };
    assert_eq!(&frame.encode_to_vec()[..2], &[0x8a, 0x03]);
    round_trip(Channel::Ipc, frame);
    round_trip(
        Channel::Ipc,
        ipc::Status {
            game_guard_signals: Some(signals),
            game_guard_reason: "fullscreen".into(),
            game_guard_mode: "auto".into(),
            game_guard_last_app: "game.exe".into(),
            game_guard_block_apps: vec!["game.exe".into()],
            game_guard_allow_apps: vec![],
            ..Default::default()
        },
    );
    assert_eq!(ipc::HotkeyAction::GameGuardToggle as i32, 5);
}

#[test]
fn discovery_ipc_fields_use_append_only_numbers() {
    use prost::Message;

    let peer = ipc::DiscoveredPeer {
        node_id: "n".into(),
        display_name: "d".into(),
        address: "a".into(),
        system_name: "s".into(),
        pairing_open: true,
        protocol_version: 1,
    };
    assert_eq!(
        peer.encode_to_vec(),
        [10, 1, b'n', 18, 1, b'd', 26, 1, b'a', 34, 1, b's', 40, 1, 48, 1]
    );
    let status = ipc::Status {
        discovered_peers: vec![ipc::DiscoveredPeer::default()],
        discovery_enabled: true,
        ..Default::default()
    };
    // Status fields 29 (message) and 30 (bool); fields 26..28 already exist.
    assert_eq!(status.encode_to_vec(), [0xea, 0x01, 0, 0xf0, 0x01, 1]);
    let frame = ipc::GuiFrame {
        msg: Some(ipc::gui_frame::Msg::SetDiscovery(ipc::SetDiscovery {
            enabled: true,
        })),
        ..Default::default()
    };
    // GuiFrame field 35, containing SetDiscovery.enabled at field 1.
    assert_eq!(frame.encode_to_vec(), [0x9a, 0x02, 2, 8, 1]);
    round_trip(Channel::Ipc, peer);
    round_trip(Channel::Ipc, status);
    round_trip(Channel::Ipc, frame);
}

#[test]
fn capability_refresh_is_append_only_and_preserves_optional_enum_numbers() {
    use prost::Message;
    let envelope = wire::Envelope {
        payload: Some(wire::envelope::Payload::CapabilitiesChanged(
            wire::CapabilitiesChanged {
                capabilities: vec![wire::Capability::TextClipboard as i32, 12345],
            },
        )),
        ..Default::default()
    };
    // Envelope field 41, wire type 2: the append-only payload number is contractual.
    assert_eq!(&envelope.encode_to_vec()[..2], &[0xca, 0x02]);
    round_trip(Channel::InputControl, envelope);
}

#[test]
fn framing_prefix_boundaries_on_every_channel() {
    for (channel, max) in [
        (Channel::InputControl, 65536),
        (Channel::Clipboard, 32768),
        (Channel::Pairing, 8192),
        (Channel::Ipc, 65536),
    ] {
        assert_eq!(channel.max_frame_len(), max);
        let decoder = FrameDecoder::new(channel);
        for len in [0, 1, max - 1, max, max + 1, u32::MAX as usize] {
            let prefix = (len as u32).to_be_bytes();
            let expected = if len == 0 {
                Err(FrameError::Empty)
            } else if len > max {
                Err(FrameError::TooLarge { len, max })
            } else {
                Ok(None)
            };
            assert_eq!(decoder.decode(&prefix), expected, "{channel:?}, {len}");
        }
    }
}

#[test]
fn fragmented_and_concatenated_frames_borrow_the_original_buffer() {
    let mut bytes = encode(Channel::InputControl, b"first").unwrap();
    encode_to(Channel::InputControl, b"second", &mut bytes).unwrap();
    let decoder = FrameDecoder::new(Channel::InputControl);
    for end in 0..9 {
        assert_eq!(decoder.decode(&bytes[..end]), Ok(None));
    }
    let first = decoder.decode(&bytes).unwrap().unwrap();
    assert_eq!(first.body(), b"first");
    assert_eq!(first.body().as_ptr(), bytes[4..].as_ptr());
    assert_eq!(first.len(), 5);
    assert!(!first.is_empty());
    assert_eq!(first.consumed_len(), 9);
    let second = decoder
        .decode(&bytes[first.consumed_len()..])
        .unwrap()
        .unwrap();
    assert_eq!(second.body(), b"second");
    assert_eq!(second.consumed_len(), 10);
}

#[test]
fn encoding_boundaries_and_errors_preserve_the_destination() {
    for channel in Channel::ALL {
        for len in [1, channel.max_frame_len()] {
            let body = vec![0x5a; len];
            let framed = encode(channel, &body).unwrap();
            assert_eq!(&framed[..4], &(len as u32).to_be_bytes());
            assert_eq!(
                FrameDecoder::new(channel)
                    .decode(&framed)
                    .unwrap()
                    .unwrap()
                    .body(),
                body
            );
        }
        for (body, error) in [
            (vec![], FrameError::Empty),
            (
                vec![0; channel.max_frame_len() + 1],
                FrameError::TooLarge {
                    len: channel.max_frame_len() + 1,
                    max: channel.max_frame_len(),
                },
            ),
        ] {
            let mut destination = vec![1, 2, 3];
            let capacity = destination.capacity();
            assert_eq!(encode_to(channel, &body, &mut destination), Err(error));
            assert_eq!(destination, [1, 2, 3]);
            assert_eq!(destination.capacity(), capacity);
        }
    }
}

fn round_trip<M: prost::Message + Default + PartialEq + std::fmt::Debug>(
    channel: Channel,
    message: M,
) {
    let bytes = encode_message(channel, &message).unwrap();
    for end in 0..bytes.len() {
        assert_eq!(decode_message::<M>(channel, &bytes[..end]), Ok(None));
    }
    assert_eq!(decode_message::<M>(channel, &bytes), Ok(Some(message)));
}

#[test]
fn generated_wire_and_ipc_messages_round_trip() {
    round_trip(Channel::InputControl, hello());
    round_trip(
        Channel::Pairing,
        wire::PairingEnvelope {
            seq: 1,
            ..Default::default()
        },
    );
    round_trip(
        Channel::Ipc,
        ipc::AgentFrame {
            seq: 1,
            ..Default::default()
        },
    );
    round_trip(
        Channel::Ipc,
        ipc::GuiFrame {
            seq: 1,
            ..Default::default()
        },
    );
    // The field the window needs to name this computer on a follower in a 3+ node mesh;
    // appended as number 28, so a core from before it still decodes every older field.
    round_trip(
        Channel::Ipc,
        ipc::Status {
            local_node_id: "aa11aa11".repeat(8),
            owner_node_id: "bb22bb22".repeat(8),
            is_owner: false,
            ..Default::default()
        },
    );
    assert_eq!(
        encode_message(Channel::InputControl, &wire::Envelope::default()),
        Err(FrameError::Empty)
    );
    let oversized = wire::Envelope {
        connection_id: vec![1; 65537],
        ..Default::default()
    };
    assert!(matches!(
        encode_message(Channel::InputControl, &oversized),
        Err(FrameError::TooLarge { max: 65536, .. })
    ));
}

#[test]
fn malformed_protobuf_is_distinct_from_incomplete_or_invalid_framing() {
    let bytes = [0, 0, 0, 1, 0x80];
    assert_eq!(
        decode_message::<wire::Envelope>(Channel::InputControl, &bytes[..4]),
        Ok(None)
    );
    assert_eq!(
        decode_message::<wire::Envelope>(Channel::InputControl, &bytes),
        Err(DecodeError::Malformed)
    );
    assert_eq!(
        decode_message::<wire::Envelope>(Channel::InputControl, &[0; 4]),
        Err(DecodeError::Frame(FrameError::Empty))
    );
}

#[test]
fn sequence_rejections_preserve_progress_and_channels_are_independent() {
    let mut seq = SeqValidator::new();
    for got in [0, 2, u64::MAX] {
        assert_eq!(seq.accept(got), Err(SeqError::NotStarted { got }));
        assert_eq!(seq.expected(), 1);
    }
    for accepted in 1..=128 {
        assert_eq!(seq.accept(accepted), Ok(()));
        let expected = accepted + 1;
        assert_eq!(
            seq.accept(accepted),
            Err(SeqError::Replay {
                expected,
                got: accepted
            })
        );
        assert_eq!(
            seq.accept(expected + 1),
            Err(SeqError::Gap {
                expected,
                got: expected + 1
            })
        );
        assert_eq!(seq.expected(), expected);
        assert!(!seq.is_exhausted());
    }
    let mut other_direction = SeqValidator::new();
    assert_eq!(other_direction.accept(1), Ok(()));
    assert_eq!(seq.expected(), 129);
}

fn hello() -> wire::Envelope {
    HelloHandshake::dialer(0, 3, [7; 16])
        .local_hello(wire::Hello {
            node_id: vec![1; 32],
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn dialer_first_hello_negotiates_only_after_both_directions() {
    for (dialer_minor, acceptor_minor) in [(3, 1), (1, 3), (3, 3)] {
        let mut dialer = HelloHandshake::dialer(0, dialer_minor, [7; 16]);
        let mut acceptor = HelloHandshake::acceptor(0, acceptor_minor);
        assert_eq!(
            acceptor.local_hello(Default::default()),
            Err(HandshakeError::NotYourTurn)
        );
        let request = dialer.local_hello(Default::default()).unwrap();
        assert_eq!(request.seq, 1);
        assert!(!dialer.is_established());
        acceptor.on_remote_hello(&request).unwrap();
        assert!(!acceptor.is_established());
        let reply = acceptor.local_hello(Default::default()).unwrap();
        assert_eq!(reply.seq, 1);
        assert_eq!(reply.connection_id, request.connection_id);
        assert!(acceptor.is_established());
        dialer.on_remote_hello(&reply).unwrap();
        assert_eq!(
            dialer.negotiated(),
            Some(Negotiated {
                connection_id: [7; 16],
                major: 0,
                minor: dialer_minor.min(acceptor_minor)
            })
        );
        assert_eq!(dialer.negotiated(), acceptor.negotiated());
        assert_eq!(
            dialer.local_hello(Default::default()),
            Err(HandshakeError::AlreadySent)
        );
        assert_eq!(
            acceptor.on_remote_hello(&request),
            Err(HandshakeError::NotHello)
        );
    }
}

#[test]
fn hello_rejects_wrong_sequence_ids_version_and_payload() {
    let base = hello();
    let mut cases = vec![];
    for seq in [0, 2, u64::MAX] {
        cases.push((
            wire::Envelope {
                seq,
                ..base.clone()
            },
            HandshakeError::BadSeq { got: seq },
        ));
    }
    for len in [0, 15, 17] {
        cases.push((
            wire::Envelope {
                connection_id: vec![7; len],
                ..base.clone()
            },
            HandshakeError::BadConnectionId,
        ));
    }
    cases.push((
        wire::Envelope {
            major: 1,
            ..base.clone()
        },
        HandshakeError::MajorMismatch {
            local: 0,
            remote: 1,
        },
    ));
    cases.push((
        wire::Envelope {
            payload: None,
            ..base.clone()
        },
        HandshakeError::NotHello,
    ));
    cases.push((
        wire::Envelope {
            payload: Some(wire::envelope::Payload::EnterCommit(Default::default())),
            ..base.clone()
        },
        HandshakeError::NotHello,
    ));
    for (message, expected) in cases {
        let mut acceptor = HelloHandshake::acceptor(0, 3);
        assert_eq!(acceptor.on_remote_hello(&message), Err(expected));
        assert!(!acceptor.is_established());
    }
    assert_eq!(
        HelloHandshake::dialer(0, 3, [8; 16]).on_remote_hello(&base),
        Err(HandshakeError::ConnectionIdMismatch)
    );
}

#[test]
fn rejected_remote_hello_does_not_unlock_acceptor_send() {
    for message in [
        wire::Envelope {
            major: 1,
            ..hello()
        },
        wire::Envelope {
            payload: None,
            ..hello()
        },
    ] {
        let mut acceptor = HelloHandshake::acceptor(0, 3);
        assert!(acceptor.on_remote_hello(&message).is_err());
        assert_eq!(
            acceptor.local_hello(Default::default()),
            Err(HandshakeError::NotYourTurn)
        );
    }
}

#[test]
fn envelope_checks_payload_and_identifier_lengths_without_lease_policy() {
    assert_eq!(
        validate::envelope(&wire::Envelope {
            payload: None,
            ..hello()
        }),
        Err(Violation::EmptyPayload)
    );
    for len in [0, 15, 16, 17] {
        let value = wire::Envelope {
            connection_id: vec![0; len],
            ..hello()
        };
        assert_eq!(
            validate::envelope(&value),
            if len == 16 {
                Ok(())
            } else {
                Err(Violation::ConnectionIdLen { len })
            }
        );
        let value = wire::Envelope {
            lease_id: vec![0; len],
            ..hello()
        };
        assert_eq!(
            validate::envelope(&value),
            if len == 0 || len == 16 {
                Ok(())
            } else {
                Err(Violation::LeaseIdLen { len })
            }
        );
    }
}

#[test]
fn hello_limits_count_utf8_bytes_and_display_entries() {
    let base = wire::Hello {
        node_id: vec![0; 32],
        ..Default::default()
    };
    for len in [0, 31, 32, 33] {
        assert_eq!(
            validate::hello(&wire::Hello {
                node_id: vec![0; len],
                ..base.clone()
            }),
            if len == 32 {
                Ok(())
            } else {
                Err(Violation::NodeIdLen { len })
            }
        );
    }
    for (name, expected) in [
        ("é".repeat(64), Ok(())),
        (
            format!("{}a", "é".repeat(64)),
            Err(Violation::DisplayNameTooLong { len: 129 }),
        ),
    ] {
        assert_eq!(
            validate::hello(&wire::Hello {
                display_name: name,
                ..base.clone()
            }),
            expected
        );
    }
    for got in [0, 16, 17] {
        assert_eq!(
            validate::hello(&wire::Hello {
                displays: vec![Default::default(); got],
                ..base.clone()
            }),
            if got <= 16 {
                Ok(())
            } else {
                Err(Violation::TooManyDisplays { got })
            }
        );
    }
}

#[test]
fn input_and_clipboard_limits_cover_both_sides_of_each_boundary() {
    for got in [0, 1, 128, 129] {
        assert_eq!(
            validate::input_batch(&wire::InputBatch {
                events: vec![Default::default(); got]
            }),
            if (1..=128).contains(&got) {
                Ok(())
            } else {
                Err(Violation::InputBatchSize { got })
            }
        );
    }
    for got in [0, 1048576, 1048577] {
        assert_eq!(
            validate::clipboard_offer(&wire::ClipboardOffer {
                size_bytes: got,
                ..Default::default()
            }),
            if got <= 1048576 {
                Ok(())
            } else {
                Err(Violation::ClipboardPayloadTooLarge { got })
            }
        );
    }
    for count in [0, 1, 64, 65] {
        let data = wire::ClipboardData {
            chunk_count: count,
            ..Default::default()
        };
        assert_eq!(
            validate::clipboard_data(&data),
            if (1..=64).contains(&count) {
                Ok(())
            } else {
                Err(Violation::ClipboardChunkCount { got: count })
            }
        );
    }
    for index in [0, 63, 64, 65] {
        let data = wire::ClipboardData {
            chunk_count: 64,
            chunk_index: index,
            ..Default::default()
        };
        assert_eq!(
            validate::clipboard_data(&data),
            if index < 64 {
                Ok(())
            } else {
                Err(Violation::ClipboardChunkIndex { index, count: 64 })
            }
        );
    }
    for got in [0, 16384, 16385] {
        let data = wire::ClipboardData {
            chunk_count: 1,
            data: vec![0; got],
            ..Default::default()
        };
        assert_eq!(
            validate::clipboard_data(&data),
            if got <= 16384 {
                Ok(())
            } else {
                Err(Violation::ClipboardChunkTooLarge { got })
            }
        );
    }
}

#[test]
fn typed_errors_map_to_documented_wire_codes() {
    for error in [
        FrameError::Empty,
        FrameError::TooLarge { len: 2, max: 1 },
        FrameError::EncodeFailed,
    ] {
        assert_eq!(error.error_code(), wire::ErrorCode::InvalidFrame);
        assert_eq!(
            DecodeError::from(error).error_code(),
            wire::ErrorCode::InvalidFrame
        );
    }
    assert_eq!(
        DecodeError::Malformed.error_code(),
        wire::ErrorCode::InvalidFrame
    );
    for error in [
        SeqError::NotStarted { got: 0 },
        SeqError::Replay {
            expected: 2,
            got: 1,
        },
        SeqError::Gap {
            expected: 2,
            got: 3,
        },
        SeqError::Exhausted,
    ] {
        assert_eq!(error.error_code(), wire::ErrorCode::InvalidSequence);
    }
    for error in [
        Violation::EmptyPayload,
        Violation::ConnectionIdLen { len: 0 },
        Violation::LeaseIdLen { len: 1 },
        Violation::NodeIdLen { len: 1 },
        Violation::DisplayNameTooLong { len: 129 },
        Violation::TooManyDisplays { got: 17 },
        Violation::InputBatchSize { got: 0 },
        Violation::ClipboardChunkCount { got: 0 },
        Violation::ClipboardChunkIndex { index: 1, count: 1 },
    ] {
        assert_eq!(error.error_code(), wire::ErrorCode::InvalidFrame);
    }
    for error in [
        Violation::ClipboardPayloadTooLarge { got: 1048577 },
        Violation::ClipboardChunkTooLarge { got: 16385 },
    ] {
        assert_eq!(error.error_code(), wire::ErrorCode::ClipboardTooLarge);
    }
    for (error, code) in [
        (HandshakeError::AlreadySent, None),
        (HandshakeError::NotYourTurn, None),
        (
            HandshakeError::BadSeq { got: 0 },
            Some(wire::ErrorCode::InvalidSequence),
        ),
        (
            HandshakeError::BadConnectionId,
            Some(wire::ErrorCode::InvalidFrame),
        ),
        (
            HandshakeError::ConnectionIdMismatch,
            Some(wire::ErrorCode::InvalidFrame),
        ),
        (
            HandshakeError::NotHello,
            Some(wire::ErrorCode::InvalidFrame),
        ),
        (
            HandshakeError::MajorMismatch {
                local: 0,
                remote: 1,
            },
            Some(wire::ErrorCode::UnsupportedVersion),
        ),
    ] {
        assert_eq!(error.peer_error_code(), code);
    }
}

#[test]
fn application_versions_are_strict_bounded_semver() {
    for version in ["0.1.6", "1.2.3-rc.1", "1.2.3+build.42"] {
        assert_eq!(protocol::sanitize_app_version(version), version);
    }
    for version in [
        "",
        "unknown",
        "v1.2.3",
        "1.2",
        "01.2.3",
        "1.2.3\n",
        " 1.2.3",
        "1.2.3-01",
        "https://example.org/1.2.3",
    ] {
        assert_eq!(protocol::sanitize_app_version(version), "", "{version:?}");
    }
    let boundary = format!("1.2.3+{}", "a".repeat(26));
    assert_eq!(boundary.len(), 32);
    assert_eq!(protocol::sanitize_app_version(&boundary), boundary);
    assert!(protocol::sanitize_app_version(&(boundary + "a")).is_empty());
    assert_eq!(
        protocol::app_version(),
        option_env!("BIFROST_SMOKE_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn application_version_comparison_is_semantic_not_lexical() {
    use semver::Version;
    let ordered = [
        "1.2.3-alpha",
        "1.2.3-alpha.2",
        "1.2.3-alpha.10",
        "1.2.3-rc.1",
        "1.2.3",
        "1.2.10",
        "1.10.0",
        "2.0.0",
    ];
    for pair in ordered.windows(2) {
        assert!(Version::parse(pair[0]).unwrap() < Version::parse(pair[1]).unwrap());
    }
    assert_eq!(
        Version::parse("1.2.3+one")
            .unwrap()
            .cmp_precedence(&Version::parse("1.2.3+two").unwrap()),
        std::cmp::Ordering::Equal
    );
}

#[test]
fn remote_update_wire_tags_round_trip_and_old_hello_remains_readable() {
    use prost::Message;
    use wire::envelope::Payload;
    assert_eq!(wire::Capability::RemoteUpdate as i32, 11);
    let hello = wire::Hello {
        app_version: "1.2.3".into(),
        ..Default::default()
    };
    assert_eq!(hello.encode_to_vec(), b"\x7a\x051.2.3");
    assert_eq!(
        wire::Hello::decode(hello.encode_to_vec().as_slice()).unwrap(),
        hello
    );
    assert!(wire::Hello::decode(&[][..]).unwrap().app_version.is_empty());
    // A pre-version decoder ignores the new field and retains all fields it understands.
    #[derive(Clone, PartialEq, Message)]
    struct OldHello {
        #[prost(string, tag = "2")]
        display_name: String,
    }
    let current = wire::Hello {
        display_name: "desktop".into(),
        ..hello
    };
    let old = OldHello::decode(current.encode_to_vec().as_slice()).unwrap();
    assert_eq!(old.display_name, "desktop");
    assert!(wire::Hello::decode(old.encode_to_vec().as_slice())
        .unwrap()
        .app_version
        .is_empty());

    for (payload, tag) in [
        (
            Payload::UpdateRequest(wire::UpdateRequest {
                target_version: "1.2.3".into(),
            }),
            [0xd2, 0x02],
        ),
        (
            Payload::UpdateResult(wire::UpdateResult {
                target_version: "1.2.3".into(),
                status: "downloading".into(),
                detail: "verified source".into(),
            }),
            [0xda, 0x02],
        ),
    ] {
        let envelope = wire::Envelope {
            payload: Some(payload),
            ..Default::default()
        };
        let bytes = envelope.encode_to_vec();
        assert_eq!(&bytes[..2], &tag);
        assert_eq!(wire::Envelope::decode(bytes.as_slice()).unwrap(), envelope);
        #[derive(Clone, PartialEq, Message)]
        struct OldEnvelope {
            #[prost(uint32, tag = "1")]
            major: u32,
        }
        assert_eq!(OldEnvelope::decode(bytes.as_slice()).unwrap().major, 0);
    }
    // A future unknown oneof remains absent, preserving the existing invalid-frame policy.
    let unknown = wire::Envelope::decode(&[0xe2, 0x02, 0x00][..]).unwrap();
    assert!(unknown.payload.is_none());
}

#[test]
fn update_ipc_fields_are_append_only_and_round_trip() {
    use prost::Message;
    use protocol::ipc;
    let member = ipc::MemberStatus {
        app_version: "1.2.3".into(),
        remote_update: true,
        ..Default::default()
    };
    assert_eq!(member.encode_to_vec(), b"\x32\x051.2.3\x38\x01");
    assert_eq!(
        ipc::MemberStatus::decode(member.encode_to_vec().as_slice()).unwrap(),
        member
    );
    let old = ipc::MemberStatus::decode(&[][..]).unwrap();
    assert!(old.app_version.is_empty());
    assert!(!old.remote_update);
    let topology = ipc::MemberTopology {
        app_version: "1.2.3".into(),
        ..Default::default()
    };
    assert_eq!(topology.encode_to_vec(), b"\x32\x051.2.3");
    assert_eq!(
        ipc::MemberTopology::decode(topology.encode_to_vec().as_slice()).unwrap(),
        topology
    );
    let status = ipc::Status {
        allow_remote_update: true,
        update_nodes: vec![ipc::NodeUpdate {
            node_id: "node".into(),
            display_name: "desktop".into(),
            target_version: "1.2.3".into(),
            status: "pending".into(),
            detail: String::new(),
        }],
        update_notice: "update".into(),
        ..Default::default()
    };
    assert_eq!(
        ipc::Status::decode(status.encode_to_vec().as_slice()).unwrap(),
        status
    );
    let status_flag = ipc::Status {
        allow_remote_update: true,
        ..Default::default()
    };
    assert_eq!(status_flag.encode_to_vec(), [0xf8, 0x01, 0x01]);
    for (msg, tag) in [
        (
            ipc::gui_frame::Msg::UpdateAll(ipc::UpdateAll {
                target_version: "1.2.3".into(),
            }),
            [0xa2, 0x02],
        ),
        (
            ipc::gui_frame::Msg::SetRemoteUpdate(ipc::SetRemoteUpdate { enabled: true }),
            [0xaa, 0x02],
        ),
        (
            ipc::gui_frame::Msg::UpdateProgress(ipc::UpdateProgress {
                target_version: "1.2.3".into(),
                status: "downloading".into(),
                detail: String::new(),
            }),
            [0xb2, 0x02],
        ),
        (
            ipc::gui_frame::Msg::UpdateReady(ipc::UpdateReady {
                target_version: "1.2.3".into(),
            }),
            [0xba, 0x02],
        ),
    ] {
        let frame = ipc::GuiFrame {
            msg: Some(msg),
            ..Default::default()
        };
        let bytes = frame.encode_to_vec();
        assert_eq!(&bytes[..2], &tag);
        assert_eq!(ipc::GuiFrame::decode(bytes.as_slice()).unwrap(), frame);
    }
}

#[test]
fn clipboard_routing_schema_tags_and_roundtrips_are_append_only() {
    use ipc::gui_frame::Msg;
    use prost::Message;
    use wire::envelope::Payload;

    assert_eq!(wire::Capability::ClipboardApplyAck as i32, 12);
    assert_eq!(protocol::limits::PROTOCOL_MINOR, 3);
    let ack = ipc::ClipboardApplied {
        origin_version: 7,
        sha256: vec![1],
        ok: true,
        detail: "d".into(),
    };
    let ack_bytes = [8, 7, 18, 1, 1, 24, 1, 34, 1, b'd'];
    assert_eq!(ack.encode_to_vec(), ack_bytes);
    let peer_ack = wire::ClipboardApplied {
        origin_version: ack.origin_version,
        sha256: ack.sha256.clone(),
        ok: ack.ok,
        detail: ack.detail.clone(),
    };
    assert_eq!(peer_ack.encode_to_vec(), ack_bytes);
    let agent = ipc::AgentFrame {
        msg: Some(ipc::agent_frame::Msg::ClipboardApplied(ack)),
        ..Default::default()
    };
    assert_eq!(&agent.encode_to_vec()[..2], &[0xda, 0x03]); // field 59
    round_trip(Channel::Ipc, agent);

    let send = ipc::SendClipboardHistoryItem {
        id: 7,
        peer_node_id: vec![1],
    };
    assert_eq!(send.encode_to_vec(), [8, 7, 18, 1, 1]);
    let policy = ipc::SetClipboardPolicy {
        policy: "follow".into(),
        settle_ms: 700,
    };
    assert_eq!(
        policy.encode_to_vec(),
        [10, 6, b'f', b'o', b'l', b'l', b'o', b'w', 16, 0xbc, 0x05]
    );
    for (msg, tag) in [
        (Msg::SendClipboardHistoryItem(send), [0x92, 0x03]),
        (Msg::SetClipboardPolicy(policy), [0x9a, 0x03]),
    ] {
        let frame = ipc::GuiFrame {
            msg: Some(msg),
            ..Default::default()
        };
        assert_eq!(&frame.encode_to_vec()[..2], &tag);
        round_trip(Channel::Ipc, frame);
    }
    let status = ipc::Status {
        clipboard_policy: "f".into(),
        clipboard_settle_ms: 700,
        ..Default::default()
    };
    assert_eq!(
        status.encode_to_vec(),
        [0xc2, 0x02, 1, b'f', 0xc8, 0x02, 0xbc, 0x05]
    );
    round_trip(Channel::Ipc, status);

    let route = wire::ClipboardRoute {
        action: "carry".into(),
        origin_node_id: vec![2],
        origin_version: 7,
        sha256: vec![3],
        target_node_id: vec![4],
    };
    assert_eq!(
        route.encode_to_vec(),
        [10, 5, b'c', b'a', b'r', b'r', b'y', 18, 1, 2, 24, 7, 34, 1, 3, 42, 1, 4]
    );
    for (payload, tag) in [
        (Payload::ClipboardApplied(peer_ack), [0xe2, 0x02]),
        (Payload::ClipboardRoute(route), [0xea, 0x02]),
    ] {
        let envelope = wire::Envelope {
            payload: Some(payload),
            ..Default::default()
        };
        assert_eq!(&envelope.encode_to_vec()[..2], &tag);
        round_trip(Channel::InputControl, envelope);
    }
}

#[test]
fn locally_copied_history_direction_is_append_only_and_round_trips() {
    use prost::Message;

    assert_eq!(ipc::ClipboardHistoryDirection::Sent as i32, 1);
    assert_eq!(ipc::ClipboardHistoryDirection::Received as i32, 2);
    assert_eq!(ipc::ClipboardHistoryDirection::Copied as i32, 3);
    let entry = ipc::ClipboardHistoryEntry {
        direction: ipc::ClipboardHistoryDirection::Copied as i32,
        ..Default::default()
    };
    assert_eq!(entry.encode_to_vec(), [16, 3]);
    round_trip(Channel::Ipc, entry);
}
