//! Typed failures of the local IPC layer.

use core::fmt;
use std::io;

use crate::peer::{PeerCredentials, PeerRejected};

/// Everything that can go wrong on a local IPC link.
#[derive(Debug)]
pub enum IpcError {
    /// An OS call failed. `context` says what was being attempted, because the
    /// raw error alone is rarely enough for a support log.
    Io {
        /// What was being attempted, e.g. `"reading an IPC frame"`.
        context: &'static str,
        /// The underlying error.
        source: io::Error,
    },
    /// The length prefix is not a valid frame length for the channel
    /// (`protocol::FrameError`: zero, or above the channel limit).
    Framing(protocol::FrameError),
    /// The frame length was valid but the body is not a decodable message of the
    /// expected type.
    Malformed {
        /// Which message type was expected.
        expected: &'static str,
    },
    /// The peer closed the channel (`read` returned 0). For the agent this means
    /// core is gone and capture plus ledger must be released immediately
    /// (`docs/03-architecture.md`, S2).
    Closed,
    /// The accepted local peer is not the one this endpoint serves.
    PeerRejected(PeerRejected),
    /// The link was silent for longer than the watchdog allows. On the agent
    /// side this is rule 6: release capture and ledger, go Local.
    Watchdog {
        /// How long nothing arrived.
        silence_ms: u64,
        /// The configured limit.
        limit_ms: u64,
    },
    /// `seq` of the received frame was not the next expected one; the docs close
    /// the channel on any repetition, regression or gap.
    Sequence(protocol::SeqError),
    /// The message violates a rule of the IPC contract: frame `major` mismatch,
    /// a launch token that does not match, a message that the agent mode does not
    /// allow, a second `AgentAttach`, and so on.
    Protocol {
        /// Human-readable description of the violated rule.
        detail: String,
    },
    /// An endpoint could not be used: bad pipe name, socket path outside the
    /// runtime directory, an out-of-range `core_silence_ms`, and so on.
    Config {
        /// Human-readable description.
        detail: String,
    },
    /// Another process already owns the endpoint (a live socket, or a pipe name
    /// that someone created before us).
    AlreadyRunning {
        /// The endpoint that is already in use.
        endpoint: String,
    },
}

impl IpcError {
    /// Builds an [`IpcError::Io`] with context.
    pub fn io(context: &'static str, source: io::Error) -> Self {
        IpcError::Io { context, source }
    }

    /// Builds an [`IpcError::Protocol`].
    pub fn protocol(detail: impl Into<String>) -> Self {
        IpcError::Protocol {
            detail: detail.into(),
        }
    }

    /// Builds an [`IpcError::Config`].
    pub fn config(detail: impl Into<String>) -> Self {
        IpcError::Config {
            detail: detail.into(),
        }
    }

    /// `true` when the peer is gone and local state must be released.
    pub const fn is_disconnected(&self) -> bool {
        matches!(self, IpcError::Closed)
    }
}

impl From<protocol::FrameError> for IpcError {
    fn from(error: protocol::FrameError) -> Self {
        IpcError::Framing(error)
    }
}

impl From<protocol::SeqError> for IpcError {
    fn from(error: protocol::SeqError) -> Self {
        IpcError::Sequence(error)
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Io { context, source } => write!(f, "{context}: {source}"),
            IpcError::Framing(error) => write!(f, "invalid IPC frame: {error:?}"),
            IpcError::Malformed { expected } => {
                write!(f, "frame body is not a valid {expected}")
            }
            IpcError::Closed => f.write_str("the peer closed the IPC channel"),
            IpcError::PeerRejected(rejected) => write!(f, "{rejected}"),
            IpcError::Watchdog {
                silence_ms,
                limit_ms,
            } => write!(
                f,
                "IPC silence {silence_ms} ms exceeds the {limit_ms} ms limit"
            ),
            IpcError::Sequence(error) => write!(f, "IPC sequence violation: {error:?}"),
            IpcError::Protocol { detail } => write!(f, "IPC protocol violation: {detail}"),
            IpcError::Config { detail } => write!(f, "IPC configuration: {detail}"),
            IpcError::AlreadyRunning { endpoint } => {
                write!(f, "{endpoint} is already in use")
            }
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IpcError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A helper for the common "not the user we serve" case, so the error can carry
/// the endpoint it happened on.
pub(crate) fn rejected_on(endpoint: &str, rejected: PeerRejected) -> IpcError {
    IpcError::PeerRejected(PeerRejected {
        peer: rejected.peer,
        expected: format!("{} ({endpoint})", rejected.expected),
    })
}

/// Convenience for callers that want the credentials of a rejection.
pub fn rejected_peer(error: &IpcError) -> Option<&PeerCredentials> {
    match error {
        IpcError::PeerRejected(rejected) => Some(&rejected.peer),
        _ => None,
    }
}
