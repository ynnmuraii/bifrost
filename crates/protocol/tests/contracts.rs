use protocol::{ipc, validate, wire, *};

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
