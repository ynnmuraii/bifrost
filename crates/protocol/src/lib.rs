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
