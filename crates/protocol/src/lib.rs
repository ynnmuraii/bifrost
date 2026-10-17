#![forbid(unsafe_code)]
//! Wire protocol of Bifrost (KVM-031): generated Protobuf types, the
//! framing codec, its limits, sequence validation and the `Hello` exchange.
//!
//! # What is here
//!
//! * [`wire`] — `lan_workspace.v1` messages: `Envelope`, `Hello`, leases,
//!   clipboard, `PairingEnvelope`.
//! * [`ipc`] — `lan_workspace.ipc.v1` messages: `AgentFrame`, `GuiFrame`.
//! * [`frame`] — `4-byte big-endian length || Protobuf body` encode/decode over
//!   byte buffers. Sans-IO: no `tokio`, no sockets, no allocation while
//!   decoding; a decode never allocates before the declared length was checked
//!   against the per-channel limit.
//! * [`seq`] — the strictly increasing per-direction, per-channel `seq`
//!   validator.
//! * [`hello`] — the dialer-first `Hello` ordering and version negotiation.
//! * [`limits`] — ports, ALPN strings, protocol version and every hard limit in
//!   one place.
//! * [`validate`] — post-decode size/identifier limit checks.
//!
//! # Sources
//!
//! The `.proto` files in `proto/` are verbatim copies of the design drafts in
//! `docs/`; `docs/` is git-ignored, so the copies are the in-repo source of
//! truth for code generation and **must be kept in sync with `docs/`**.
//! Everything else comes from `docs/05-protocol.md`, `docs/ipc.proto` and
//! `docs/03-architecture.md`; the constants and rules are quoted where they are
//! used.
//!
//! # Framing in one paragraph
//!
//! Every frame is a 4-byte unsigned big-endian length followed by a serialized
//! message: `Envelope` on the input/control and clipboard channels,
//! `PairingEnvelope` on the pairing channel, `AgentFrame`/`GuiFrame` on local
//! IPC. The declared length must be inside `1..=channel.max_frame_len()` and is
//! checked from the four prefix bytes alone, so a hostile prefix can never make
//! the decoder reserve memory. [`frame::decode_message`] composes framing and
//! Protobuf decoding and reports `Ok(None)` while a frame is incomplete.

pub mod frame;
pub mod hello;
pub mod limits;
pub mod seq;
pub mod validate;

/// Generated `lan_workspace.v1` messages (`docs/protocol.proto`).
#[allow(clippy::all)]
pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/lan_workspace.v1.rs"));
}

/// Generated `lan_workspace.ipc.v1` messages (`docs/ipc.proto`).
#[allow(clippy::all)]
pub mod ipc {
    include!(concat!(env!("OUT_DIR"), "/lan_workspace.ipc.v1.rs"));
}

pub use frame::{
    decode_message, encode, encode_message, encode_to, Channel, DecodeError, Frame, FrameDecoder,
    FrameError, LENGTH_PREFIX_LEN,
};
pub use hello::{HandshakeError, HandshakeSide, HelloHandshake, Negotiated};
pub use limits::*;
pub use seq::{SeqError, SeqValidator};
pub use validate::Violation;

/// Bounds non-authoritative OS labels from local probes and untrusted peers.
pub fn sanitize_system_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| {
            !ch.is_control()
                && !matches!(
                    ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}'
                )
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(32)
        .collect()
}

#[cfg(test)]
mod system_name_tests {
    #[test]
    fn network_labels_are_bounded_and_have_no_controls_or_bidi_overrides() {
        assert_eq!(
            super::sanitize_system_name(" Ubuntu\n 24.04\u{202e} "),
            "Ubuntu 24.04"
        );
        assert_eq!(
            super::sanitize_system_name(&"я".repeat(80)).chars().count(),
            32
        );
        assert_eq!(super::sanitize_system_name(""), "");
    }
}

#[cfg(test)]
mod metadata_roundtrip_tests {
    use prost::Message;
    #[test]
    fn append_only_system_name_and_native_mode_round_trip_with_old_peer_defaults() {
        let hello = super::wire::Hello {
            system_name: "Ubuntu 24.04".into(),
            ..Default::default()
        };
        assert_eq!(
            super::wire::Hello::decode(hello.encode_to_vec().as_slice())
                .unwrap()
                .system_name,
            "Ubuntu 24.04"
        );
        let encoded = hello.encode_to_vec();
        assert_eq!(
            &encoded[..2],
            &[0x72, 0x0b],
            "Hello.system_name stays tag 14, wire type 2"
        );
        assert_eq!(&encoded[2..], b"Ubuntu 24.04");
        assert!(super::wire::Hello::decode(&[][..])
            .unwrap()
            .system_name
            .is_empty());
        let display = super::ipc::DisplayView {
            logical_w: 2560.0 / 1.5,
            logical_h: 1600.0 / 1.5,
            native_width: 2560,
            native_height: 1600,
            native_known: true,
            scale: 1.5,
            ..Default::default()
        };
        let display_bytes = display.encode_to_vec();
        assert_eq!(
            super::ipc::DisplayView::decode(display_bytes.as_slice()).unwrap(),
            display
        );
        let old = super::ipc::DisplayView::decode(&[][..]).unwrap();
        assert_eq!((old.native_width, old.native_height), (0, 0));
        assert_eq!(
            &display_bytes[display_bytes.len() - 6..],
            &[0x50, 0x80, 0x14, 0x58, 0xC0, 0x0C],
            "native_width/native_height stay tags 10 and 11"
        );
        let member = super::ipc::MemberTopology {
            system_name: "CachyOS".into(),
            ..Default::default()
        };
        assert_eq!(
            super::ipc::MemberTopology::decode(member.encode_to_vec().as_slice()).unwrap(),
            member
        );
        assert_eq!(
            member.encode_to_vec(),
            [0x2a, 0x07, b'C', b'a', b'c', b'h', b'y', b'O', b'S'],
            "MemberTopology.system_name stays tag 5"
        );
        assert!(super::ipc::MemberTopology::decode(&[][..])
            .unwrap()
            .system_name
            .is_empty());
    }
}
