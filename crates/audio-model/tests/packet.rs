use audio_model::packet::{
    validate_datagram, Header, PacketError, PacketKind, HEADER_LEN, MAX_DATAGRAM_LEN,
    MAX_PAYLOAD_LEN, PACKET_NUMBER_LIMIT, TAG_LEN,
};

fn header() -> Header {
    Header {
        kind: PacketKind::Opus,
        flags: 0,
        session_id: [0xa5; 16],
        key_epoch: 1,
        packet_number: 0x0001_0203,
        media_timestamp: 0x1122_3344,
        frame_samples: 480,
        payload_len: 3,
    }
}

#[test]
fn exact_big_endian_wire_vector() {
    let expected = [
        b'B', b'F', b'A', b'1', 1, 1, 0, 0, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5,
        0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0xa5, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 2, 3, 0x11, 0x22,
        0x33, 0x44, 1, 0xe0, 0, 3,
    ];
    assert_eq!(header().encode(), Ok(expected));
    assert_eq!(Header::decode(&expected), Ok(header()));
}

#[test]
fn every_kind_and_frame_size_round_trips() {
    for (wire, kind) in [
        (1, PacketKind::Opus),
        (2, PacketKind::PathChallenge),
        (3, PacketKind::PathResponse),
        (4, PacketKind::ReceiverReport),
        (5, PacketKind::Keepalive),
    ] {
        for samples in [480, 960] {
            let mut value = header();
            value.kind = kind;
            if kind == PacketKind::Opus {
                value.frame_samples = samples;
            } else {
                value.frame_samples = 0;
                value.media_timestamp = 0;
            }
            let bytes = value.encode().unwrap();
            assert_eq!(bytes[5], wire);
            assert_eq!(Header::decode(&bytes), Ok(value));
        }
    }
}

#[test]
fn timestamp_wrap_and_packet_number_extremes_are_preserved() {
    for timestamp in [u32::MAX - 479, 0, 480, u32::MAX] {
        for number in [0, PACKET_NUMBER_LIMIT - 1] {
            let value = Header {
                media_timestamp: timestamp,
                packet_number: number,
                ..header()
            };
            assert_eq!(Header::decode(&value.encode().unwrap()), Ok(value));
        }
    }
    assert_eq!((u32::MAX - 479).wrapping_add(480), 0);
}

#[test]
fn rotation_bound_is_enforced_before_authentication() {
    for packet_number in [PACKET_NUMBER_LIMIT, PACKET_NUMBER_LIMIT + 1, u64::MAX] {
        let value = Header {
            packet_number,
            ..header()
        };
        assert_eq!(value.encode(), Err(PacketError::RotationRequired));
        let mut bytes = header().encode().unwrap();
        bytes[28..36].copy_from_slice(&packet_number.to_be_bytes());
        assert_eq!(Header::decode(&bytes), Err(PacketError::RotationRequired));
        let mut datagram = [0; HEADER_LEN + 3 + TAG_LEN];
        datagram[..HEADER_LEN].copy_from_slice(&bytes);
        assert_eq!(
            validate_datagram(&datagram),
            Err(PacketError::RotationRequired)
        );
    }
}

#[test]
fn malformed_or_unsupported_header_fields_are_rejected() {
    let original = header().encode().unwrap();
    for length in [0, 1, HEADER_LEN - 1, HEADER_LEN + 1] {
        assert_eq!(
            Header::decode(&[0; HEADER_LEN + 1][..length]),
            Err(PacketError::InvalidHeaderLength)
        );
    }
    for (offset, byte, error) in [
        (0, b'X', PacketError::InvalidMagic),
        (4, 0, PacketError::UnsupportedVersion(0)),
        (4, 2, PacketError::UnsupportedVersion(2)),
        (5, 0, PacketError::UnsupportedKind(0)),
        (5, 6, PacketError::UnsupportedKind(6)),
        (5, 255, PacketError::UnsupportedKind(255)),
        (6, 128, PacketError::UnsupportedFlags(0x8000)),
        (7, 1, PacketError::UnsupportedFlags(1)),
        (27, 0, PacketError::UnsupportedKeyEpoch(0)),
        (27, 2, PacketError::UnsupportedKeyEpoch(2)),
        (40, 0, PacketError::InvalidMediaFields),
        (42, 255, PacketError::PayloadTooLarge),
    ] {
        let mut bytes = original;
        bytes[offset] = byte;
        assert_eq!(Header::decode(&bytes), Err(error), "offset {offset}");
    }
}

#[test]
fn encode_enforces_the_same_field_rules_as_decode() {
    for value in [
        Header {
            flags: 1,
            ..header()
        },
        Header {
            key_epoch: 0,
            ..header()
        },
        Header {
            key_epoch: 2,
            ..header()
        },
        Header {
            frame_samples: 0,
            ..header()
        },
        Header {
            frame_samples: 481,
            ..header()
        },
        Header {
            payload_len: 1141,
            ..header()
        },
    ] {
        assert!(value.encode().is_err());
    }
    for kind in [
        PacketKind::PathChallenge,
        PacketKind::PathResponse,
        PacketKind::ReceiverReport,
        PacketKind::Keepalive,
    ] {
        for (timestamp, samples) in [(1, 0), (0, 480), (0, 960)] {
            let value = Header {
                kind,
                media_timestamp: timestamp,
                frame_samples: samples,
                ..header()
            };
            assert_eq!(value.encode(), Err(PacketError::InvalidMediaFields));
            let mut bytes = header().encode().unwrap();
            bytes[5] = kind as u8;
            bytes[36..40].copy_from_slice(&timestamp.to_be_bytes());
            bytes[40..42].copy_from_slice(&samples.to_be_bytes());
            assert_eq!(Header::decode(&bytes), Err(PacketError::InvalidMediaFields));
        }
    }
}

#[test]
fn exact_datagram_lengths_and_opaque_ciphertext_and_tag() {
    assert_eq!(HEADER_LEN, 44);
    assert_eq!(TAG_LEN, 16);
    assert_eq!(MAX_PAYLOAD_LEN, 1140);
    assert_eq!(MAX_DATAGRAM_LEN, 1200);
    for payload_len in [0, 1, 3, MAX_PAYLOAD_LEN as u16] {
        let value = Header {
            payload_len,
            ..header()
        };
        let mut datagram = [0xc7; MAX_DATAGRAM_LEN + 1];
        datagram[..HEADER_LEN].copy_from_slice(&value.encode().unwrap());
        let length = HEADER_LEN + usize::from(payload_len) + TAG_LEN;
        let original = datagram;
        assert_eq!(validate_datagram(&datagram[..length]), Ok(value));
        assert_eq!(datagram, original);
        assert_eq!(
            validate_datagram(&datagram[..length - 1]),
            Err(PacketError::InvalidDatagramLength)
        );
        let trailing_error = if length == MAX_DATAGRAM_LEN {
            PacketError::DatagramTooLarge
        } else {
            PacketError::InvalidDatagramLength
        };
        assert_eq!(
            validate_datagram(&datagram[..length + 1]),
            Err(trailing_error)
        );
        // Header-only parsing deliberately does not require ciphertext or a tag.
        assert_eq!(Header::decode(&datagram[..HEADER_LEN]), Ok(value));
        for truncated in [0, HEADER_LEN - 1, HEADER_LEN, HEADER_LEN + TAG_LEN - 1] {
            assert_eq!(
                validate_datagram(&datagram[..truncated]),
                Err(PacketError::InvalidDatagramLength)
            );
        }
    }
    let mut malformed = [0; HEADER_LEN + TAG_LEN];
    malformed[..HEADER_LEN].copy_from_slice(&header().encode().unwrap());
    malformed[4] = 2;
    // Exact size precedes header semantics under the S3 acceptance contract.
    assert_eq!(
        validate_datagram(&malformed),
        Err(PacketError::InvalidDatagramLength)
    );
    malformed[42..44].copy_from_slice(&0u16.to_be_bytes());
    assert_eq!(
        validate_datagram(&malformed),
        Err(PacketError::UnsupportedVersion(2))
    );
}
