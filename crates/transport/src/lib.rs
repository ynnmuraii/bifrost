//! Bifrost transport: pinned TLS links, mesh dedup and liveness (KVM-032/070).

pub mod clipboard;
pub mod convert;
pub mod link;
pub mod manager;
pub mod pairing;
pub mod policy;
pub mod queue;
pub mod rtt;

pub use clipboard::{
    dial_clipboard, ClipboardConnection, ClipboardListener, ClipboardMessage, ClipboardReader,
    ClipboardViolation, ClipboardWriter, Grants, Outgoing,
};
pub use convert::{ConvertError, PeerInfo};
pub use link::{
    read_frame, write_frame, Direction, LeaseTransportSnapshot, Link, LinkConfig, LinkDownReason,
    LinkEvent, LinkHandle, LinkReport, Stream,
};
pub use manager::{LinkManager, ManagerConfig};
pub use pairing::{PairingConnection, PairingGate};
pub use policy::{
    admit_member, liveness_action, next_liveness_wakeup, prefer_incoming, reconnect_delay,
    winning_dialer, BackoffBounds, LinkLiveness, LivenessAction, MembershipFull,
};
pub use queue::{
    classify, coalesce_motion, is_motion, merge_motion_batches, MessageClass, Outbound,
};
pub use rtt::{RttMeter, RttSnapshot};

pub use geometry::NodeId;
pub use identity::random16;

/// Transport failures.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Socket or TLS stream error.
    #[error("link i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Framing, sequence or conversion violation.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The link's task is gone.
    #[error("the link is closed")]
    LinkClosed,
    /// TLS configuration or handshake failure.
    #[error("tls error: {0}")]
    Tls(#[from] rustls::Error),
    /// Identity/key store failure.
    #[error("identity error: {0}")]
    Identity(#[from] identity::IdentityError),
    /// A pairing record has no usable socket address.
    #[error("invalid peer address: {0}")]
    InvalidPeerAddress(String),
}
