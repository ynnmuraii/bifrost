//! `Hello` exchange ordering and version negotiation for the input/control
//! link.
//!
//! `05-protocol.md`, «Начало сеанса»: «Узлы связанного линка обмениваются
//! `Hello`: каждое направление — свой `Hello` с одним и тем же `connection_id`
//! (его выбрал дозванивающийся узел) и `seq=1`. Первым отправляет `Hello`
//! TCP-инициатор — дозванивающийся узел, который и выбрал `connection_id`;
//! принимающая сторона отвечает своим `Hello` с тем же `connection_id`. Version
//! `major` должен совпасть; negotiated `minor = min` обеих сторон.»
//!
//! [`HelloHandshake`] encodes exactly those rules and nothing else: it produces
//! and checks the two first `Envelope`s of a link. Payload validation after that
//! belongs to the lease FSM.

use crate::limits::CONNECTION_ID_LEN;
use crate::wire;

/// Which end of the TCP connection this node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeSide {
    /// The TCP initiator: picks the `connection_id` and sends `Hello` first.
    Dialer,
    /// The TCP acceptor: answers with its own `Hello` once the dialer's arrived.
    Acceptor,
}

/// The agreed parameters of a linked pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// The dialer-chosen connection id, identical in both directions.
    pub connection_id: [u8; CONNECTION_ID_LEN],
    /// Equal on both sides (a mismatch aborts the link).
    pub major: u32,
    /// `min` of both sides' minor versions.
    pub minor: u32,
}

/// A violation of the Hello ordering or version rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeError {
    /// This side's `Hello` was already produced.
    AlreadySent,
    /// An acceptor may not send its `Hello` before the dialer's arrived.
    NotYourTurn,
    /// The first frame of a direction must carry `seq = 1`.
    BadSeq { got: u64 },
    /// `Envelope.connection_id` is not exactly 16 bytes.
    BadConnectionId,
    /// The peer answered with a different `connection_id` than the dialer chose.
    ConnectionIdMismatch,
    /// The frame is not a first `Hello` (wrong payload, or a second `Hello`).
    NotHello,
    /// No common major version.
    MajorMismatch { local: u32, remote: u32 },
}

impl HandshakeError {
    /// The wire [`wire::ErrorCode`] for a peer-visible violation, or `None` for
    /// local misuse ([`HandshakeError::AlreadySent`],
    /// [`HandshakeError::NotYourTurn`]), which is a programming error rather than
    /// something to report to the peer.
    pub const fn peer_error_code(self) -> Option<wire::ErrorCode> {
        match self {
            HandshakeError::MajorMismatch { .. } => Some(wire::ErrorCode::UnsupportedVersion),
            HandshakeError::BadSeq { .. } => Some(wire::ErrorCode::InvalidSequence),
            HandshakeError::BadConnectionId
            | HandshakeError::ConnectionIdMismatch
            | HandshakeError::NotHello => Some(wire::ErrorCode::InvalidFrame),
            HandshakeError::AlreadySent | HandshakeError::NotYourTurn => None,
        }
    }
}

/// State machine for the two first frames of an input/control link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloHandshake {
    side: HandshakeSide,
    local_major: u32,
    local_minor: u32,
    connection_id: Option<[u8; CONNECTION_ID_LEN]>,
    local_sent: bool,
    remote_received: bool,
    remote_major: u32,
    remote_minor: u32,
    negotiated: Option<Negotiated>,
}

impl HelloHandshake {
    /// The dialing side: it chooses the `connection_id` and may send `Hello`
    /// immediately, before the peer answers.
    pub fn dialer(
        local_major: u32,
        local_minor: u32,
        connection_id: [u8; CONNECTION_ID_LEN],
    ) -> Self {
        Self {
            side: HandshakeSide::Dialer,
            local_major,
            local_minor,
            connection_id: Some(connection_id),
            local_sent: false,
            remote_received: false,
            remote_major: 0,
            remote_minor: 0,
            negotiated: None,
        }
    }

    /// The accepting side: its `connection_id` is unknown until the dialer's
    /// `Hello` arrives.
    pub fn acceptor(local_major: u32, local_minor: u32) -> Self {
        Self {
            side: HandshakeSide::Acceptor,
            local_major,
            local_minor,
            connection_id: None,
            local_sent: false,
            remote_received: false,
            remote_major: 0,
            remote_minor: 0,
            negotiated: None,
        }
    }

    /// The side this handshake runs as.
    pub const fn side(&self) -> HandshakeSide {
        self.side
    }

    /// The connection id in use; `None` on an acceptor until the dialer's
    /// `Hello` was accepted.
    pub const fn connection_id(&self) -> Option<[u8; CONNECTION_ID_LEN]> {
        self.connection_id
    }

    /// Whether this side has produced its own `Hello`.
    pub const fn local_sent(&self) -> bool {
        self.local_sent
    }

    /// Whether both `Hello`s have been exchanged and versions agreed.
    pub const fn is_established(&self) -> bool {
        self.negotiated.is_some()
    }

    /// The agreement, once both `Hello`s were exchanged.
    pub const fn negotiated(&self) -> Option<Negotiated> {
        self.negotiated
    }

    /// Build this side's first `Envelope` (`seq = 1`) around `hello`.
    ///
    /// The dialer may call this before receiving anything; the acceptor only
    /// after [`HelloHandshake::on_remote_hello`] returned `Ok`.
    pub fn local_hello(&mut self, hello: wire::Hello) -> Result<wire::Envelope, HandshakeError> {
        if self.local_sent {
            return Err(HandshakeError::AlreadySent);
        }
        let Some(connection_id) = self.connection_id else {
            return Err(HandshakeError::NotYourTurn);
        };
        self.local_sent = true;
        self.try_establish();
        Ok(wire::Envelope {
            major: self.local_major,
            minor: self.local_minor,
            connection_id: connection_id.to_vec(),
            seq: 1,
            lease_id: Vec::new(),
            payload: Some(wire::envelope::Payload::Hello(hello)),
        })
    }

    /// Check the peer's `Hello` and return the agreed parameters.
    ///
    /// Rejects a second `Hello`, a non-`Hello` payload, `seq != 1`, a
    /// `connection_id` that is not 16 bytes or that differs from the dialer's,
    /// and a `major` mismatch. The link is only fully established once both
    /// sides have sent; check [`HelloHandshake::is_established`] for that.
    pub fn on_remote_hello(
        &mut self,
        envelope: &wire::Envelope,
    ) -> Result<Negotiated, HandshakeError> {
        if self.remote_received {
            return Err(HandshakeError::NotHello);
        }
        if envelope.seq != 1 {
            return Err(HandshakeError::BadSeq { got: envelope.seq });
        }
        let Ok(connection_id) =
            <[u8; CONNECTION_ID_LEN]>::try_from(envelope.connection_id.as_slice())
        else {
            return Err(HandshakeError::BadConnectionId);
        };
        // Every check happens *before* the state is touched: a rejected `Hello` must not
        // count as received and must not adopt the dialer's `connection_id`. That field is
        // what gates the acceptor's own `Hello` (`local_hello` returns `NotYourTurn` while
        // it is unset), so committing it early would let a malformed or version-mismatched
        // frame unlock our first frame on a link the peer never opened correctly.
        if let Some(chosen) = self.connection_id {
            if chosen != connection_id {
                return Err(HandshakeError::ConnectionIdMismatch);
            }
        }
        if !matches!(envelope.payload, Some(wire::envelope::Payload::Hello(_))) {
            return Err(HandshakeError::NotHello);
        }
        if envelope.major != self.local_major {
            return Err(HandshakeError::MajorMismatch {
                local: self.local_major,
                remote: envelope.major,
            });
        }
        // Accepted: an acceptor adopts the dialer's id here.
        if self.connection_id.is_none() {
            self.connection_id = Some(connection_id);
        }
        self.remote_received = true;
        self.remote_major = envelope.major;
        self.remote_minor = envelope.minor;
        self.try_establish();
        Ok(Negotiated {
            connection_id,
            major: self.local_major,
            minor: self.local_minor.min(envelope.minor),
        })
    }

    /// Record the agreement as soon as both directions are known.
    fn try_establish(&mut self) {
        if self.local_sent && self.remote_received && self.negotiated.is_none() {
            if let Some(connection_id) = self.connection_id {
                self.negotiated = Some(Negotiated {
                    connection_id,
                    major: self.remote_major,
                    minor: self.local_minor.min(self.remote_minor),
                });
            }
        }
    }
}
