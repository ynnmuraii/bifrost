//! Minimal mirrors of the `docs/protocol.proto` messages this state machine needs.
//!
//! # Seam to `crates/protocol`
//!
//! A separate `crates/protocol` crate (prost code generation from
//! `docs/protocol.proto`) is being built in parallel. Everything in this module is a
//! **1:1 stand-in** for it: message and field names match the proto exactly, and every
//! enum variant carries the proto's wire number through [`ErrorCode::as_u32`] /
//! [`LeaveReason::as_u32`] / [`ClaimReason::as_u32`] / [`InjectStatus::as_u32`], so the
//! swap is mechanical and the numbers can be diffed against the proto by the test
//! author.
//!
//! Only the messages the focus/lease layer sends or reads are mirrored; the protocol
//! crate owns the full set (clipboard, pairing, topology documents, framing).

use geometry::{NodeId, Position};
use input_model::InputEvent;

/// `lease_id`: random bytes16, never reused.
pub type LeaseId = [u8; 16];

/// `LeaseChallenge.nonce`: random bytes16, one use, bound to connection and lease.
pub type Nonce = [u8; 16];

/// `ErrorCode` (`docs/protocol.proto`), restricted to the codes this layer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `ERROR_CODE_UNSUPPORTED_SESSION = 5` — the receiver cannot capture/inject.
    UnsupportedSession,
    /// `ERROR_CODE_CAPTURE_FAILED = 6` — the temporary capture failed.
    CaptureFailed,
    /// `ERROR_CODE_TOPOLOGY_CHANGED = 8` — the applied revision moved under the lease.
    TopologyChanged,
    /// `ERROR_CODE_INVALID_FRAME = 9` — a malformed frame or `InputBatch` size.
    InvalidFrame,
    /// `ERROR_CODE_INVALID_SEQUENCE = 10`.
    InvalidSequence,
    /// `ERROR_CODE_LEASE_MISMATCH = 11` — a lease message for the wrong/unknown lease.
    LeaseMismatch,
    /// `ERROR_CODE_INJECTION_FAILED = 12`.
    InjectionFailed,
    /// `ERROR_CODE_LEASE_EXPIRED = 13`.
    LeaseExpired,
    /// `ERROR_CODE_AGENT_LOST = 14`.
    AgentLost,
    /// `ERROR_CODE_NOT_OWNER = 20` — `EnterPrepare` (or any lease message) from a node
    /// that is not the current owner. Never used for an `OwnerClaim`.
    NotOwner,
    /// `ERROR_CODE_STALE_OWNER_SERIAL = 21` — a claim or lease carries an owner serial
    /// not greater than the known pair.
    StaleOwnerSerial,
    /// `ERROR_CODE_CANNOT_OWN = 25` — `OwnerClaim` from a node whose `Hello.can_own`
    /// is `false`.
    CannotOwn,
    /// `ERROR_CODE_MEMBERSHIP_FULL = 23` — `N_max = 4` reached; the link is refused
    /// (`docs/05-protocol.md`). Emitted by the link manager, carried here so a peer can be
    /// told why it was not admitted.
    MembershipFull,
}

impl ErrorCode {
    /// The `docs/protocol.proto` wire number.
    pub const fn as_u32(self) -> u32 {
        match self {
            ErrorCode::UnsupportedSession => 5,
            ErrorCode::CaptureFailed => 6,
            ErrorCode::TopologyChanged => 8,
            ErrorCode::InvalidFrame => 9,
            ErrorCode::InvalidSequence => 10,
            ErrorCode::LeaseMismatch => 11,
            ErrorCode::InjectionFailed => 12,
            ErrorCode::LeaseExpired => 13,
            ErrorCode::AgentLost => 14,
            ErrorCode::NotOwner => 20,
            ErrorCode::StaleOwnerSerial => 21,
            ErrorCode::CannotOwn => 25,
            ErrorCode::MembershipFull => 23,
        }
    }
}

/// `LeaveReason` (`docs/protocol.proto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaveReason {
    /// `LEAVE_REASON_RETURN_EDGE = 1`.
    ReturnEdge,
    /// `LEAVE_REASON_HOTKEY = 2`.
    Hotkey,
    /// `LEAVE_REASON_EMERGENCY = 3`.
    Emergency,
    /// `LEAVE_REASON_RECEIVER_LOCAL_INPUT = 4` — receiver → source, takeover.
    ReceiverLocalInput,
    /// `LEAVE_REASON_AGENT_LOST = 5`.
    AgentLost,
    /// `LEAVE_REASON_TOPOLOGY_CHANGED = 6`.
    TopologyChanged,
    /// `LEAVE_REASON_SUSPEND_OR_LOCK = 7`.
    SuspendOrLock,
    /// `LEAVE_REASON_SHUTDOWN = 8`.
    Shutdown,
    /// `LEAVE_REASON_OWNERSHIP_LOST = 9` — a higher `(serial, node_id)` owner appeared.
    OwnershipLost,
}

impl LeaveReason {
    /// The `docs/protocol.proto` wire number.
    pub const fn as_u32(self) -> u32 {
        match self {
            LeaveReason::ReturnEdge => 1,
            LeaveReason::Hotkey => 2,
            LeaveReason::Emergency => 3,
            LeaveReason::ReceiverLocalInput => 4,
            LeaveReason::AgentLost => 5,
            LeaveReason::TopologyChanged => 6,
            LeaveReason::SuspendOrLock => 7,
            LeaveReason::Shutdown => 8,
            LeaveReason::OwnershipLost => 9,
        }
    }

    /// Whether this reason is a *local* intent (edge, hotkey, emergency) as opposed
    /// to a failure or an ownership event.
    pub const fn is_local_intent(self) -> bool {
        matches!(
            self,
            LeaveReason::ReturnEdge | LeaveReason::Hotkey | LeaveReason::Emergency
        )
    }
}

/// `ClaimReason` (`docs/protocol.proto`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaimReason {
    /// `CLAIM_REASON_STARTUP = 1` — automatic claim when `claim_on_startup = true`.
    Startup,
    /// `CLAIM_REASON_USER = 2` — GUI button or claim hotkey.
    User,
    /// `CLAIM_REASON_TAKEOVER = 3` — the user took over a physical machine.
    Takeover,
}

impl ClaimReason {
    /// The `docs/protocol.proto` wire number.
    pub const fn as_u32(self) -> u32 {
        match self {
            ClaimReason::Startup => 1,
            ClaimReason::User => 2,
            ClaimReason::Takeover => 3,
        }
    }
}

/// `InjectStatus` (`docs/protocol.proto`). `Partial`/`Failed` abort the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InjectStatus {
    /// `INJECT_STATUS_OK = 1`.
    Ok,
    /// `INJECT_STATUS_PARTIAL = 2`.
    Partial,
    /// `INJECT_STATUS_FAILED = 3`.
    Failed,
}

impl InjectStatus {
    /// The `docs/protocol.proto` wire number.
    pub const fn as_u32(self) -> u32 {
        match self {
            InjectStatus::Ok => 1,
            InjectStatus::Partial => 2,
            InjectStatus::Failed => 3,
        }
    }

    /// Whether this status aborts the lease (`docs/05-protocol.md`: «`PARTIAL`/`FAILED`
    /// отменяют lease»).
    pub const fn aborts_lease(self) -> bool {
        matches!(self, InjectStatus::Partial | InjectStatus::Failed)
    }
}

/// The `Capability` bits the focus/lease layer reads from `Hello`.
///
/// The full set lives in `docs/protocol.proto`; only the bits that gate a transition
/// are mirrored, with their wire numbers:
/// `CAPABILITY_CAPTURE = 1`, `CAPABILITY_INJECT = 2`,
/// `CAPABILITY_ABSOLUTE_POINTER = 3`, `CAPABILITY_EMERGENCY_HOTKEY = 10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// `CAPABILITY_CAPTURE`.
    pub capture: bool,
    /// `CAPABILITY_INJECT`.
    pub inject: bool,
    /// `CAPABILITY_ABSOLUTE_POINTER`: without it a node cannot be a receiver
    /// («without absolute coordinates the transition and return model breaks»).
    pub absolute_pointer: bool,
    /// `CAPABILITY_EMERGENCY_HOTKEY`: without it a node may not be a source while
    /// `input.require_emergency_hotkey = true` (AC-03).
    pub emergency_hotkey: bool,
}

impl Capabilities {
    /// Whether this node can act as a receiver for the product at all.
    pub const fn receiver_ready(self) -> bool {
        self.inject && self.absolute_pointer
    }

    /// Whether this node can act as a source while an emergency hotkey is required.
    pub const fn source_ready(self) -> bool {
        self.capture && self.emergency_hotkey
    }
}

/// A network message of the input/control link, restricted to the focus/lease layer.
///
/// Lease messages carry the envelope's `lease_id` (`Envelope.lease_id`, «exactly 16 on
/// lease messages») as a field: `docs/protocol.proto` keeps it on the envelope, and
/// lifting it into the message here is what lets the state machine be self-contained.
/// The transport puts it back where the wire wants it.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// `EnterPrepare`: the owner asks a peer to become the receiver.
    EnterPrepare {
        /// The lease being proposed.
        lease_id: LeaseId,
        /// Applied topology revision the sender believes in.
        topology_revision: u64,
        /// Entry position on the receiver's display (display-local).
        position: Position,
        /// The sender's owner serial, checked against the receiver's known owner.
        owner_serial: u64,
    },
    /// `EnterAck`: the receiver's answer. `accepted = false` carries a reason.
    EnterAck {
        /// The lease being answered.
        lease_id: LeaseId,
        /// Whether the receiver prepared the lease.
        accepted: bool,
        /// Why not, when `accepted = false`.
        reason: Option<ErrorCode>,
    },
    /// `EnterCommit`: injection is allowed from this point in the stream.
    EnterCommit {
        /// The lease being committed.
        lease_id: LeaseId,
    },
    /// `Leave`: the source (or the receiver, for takeover) ends the lease.
    Leave {
        /// The lease being ended.
        lease_id: LeaseId,
        /// Why the lease ends.
        reason: LeaveReason,
    },
    /// `LeaveAck`: the peer confirms the release.
    LeaveAck {
        /// The lease being acknowledged.
        lease_id: LeaseId,
    },
    /// `InputBatch`: 1..=128 events, valid only for an active lease.
    InputBatch {
        /// The lease the events belong to.
        lease_id: LeaseId,
        /// Sender's envelope sequence of this batch.
        input_batch_seq: u64,
        /// The events.
        events: Vec<InputEvent>,
    },
    /// `InjectAck`: the receiver's outcome for one `InputBatch`.
    InjectAck {
        /// The lease that was injected.
        lease_id: LeaseId,
        /// Echo of the acknowledged batch sequence.
        input_batch_seq: u64,
        /// How many events were applied.
        applied_count: u32,
        /// Where the receiver's cursor ended up, when it knows.
        actual_position: Option<Position>,
        /// Injection outcome; `Partial`/`Failed` abort the lease.
        status: InjectStatus,
    },
    /// `LeaseChallenge`: receiver → source, every `min(lease_ttl/5, 100 ms)`.
    LeaseChallenge {
        /// The lease being challenged.
        lease_id: LeaseId,
        /// One-use nonce.
        nonce: Nonce,
        /// The receiver's `lease_ttl_ms`, so the source can show it.
        ttl_ms: u32,
    },
    /// `LeaseRenew`: source → receiver, answering a challenge.
    LeaseRenew {
        /// The lease being renewed.
        lease_id: LeaseId,
        /// The nonce being renewed.
        nonce: Nonce,
    },
    /// `LeaseStatus`: receiver → source, the applied renewal.
    LeaseStatus {
        /// The lease being reported.
        lease_id: LeaseId,
        /// The renewed nonce.
        nonce: Nonce,
        /// Whether the renewal was applied.
        accepted: bool,
        /// Last applied input batch sequence.
        last_input_batch_seq: u64,
    },
    /// `OwnerClaim`: a follower asks for ownership.
    OwnerClaim {
        /// `max_seen_serial + 1` at the claimant.
        serial: u64,
        /// Why the claim is being made.
        reason: ClaimReason,
    },
    /// `OwnerState`: the ownership that some node is publishing.
    OwnerState {
        /// Lamport serial of this ownership.
        serial: u64,
        /// The owner, or `None` for "no owner" (empty `owner_node_id` in the wire).
        owner: Option<NodeId>,
        /// `true` when the sender grants the claim it is answering.
        granted: bool,
    },
    /// `ProtocolError`: a typed refusal; the link stays open.
    ProtocolError {
        /// The refusal.
        code: ErrorCode,
        /// Envelope sequence this error relates to, when known.
        related_seq: u64,
    },
    /// `Shutdown`: the link is going away deliberately.
    Shutdown {
        /// Why the peer is leaving.
        reason: LeaveReason,
    },
}

/// The gossip fields of `Hello` that the ownership and capability gates need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerHello {
    /// `Hello.can_own`: may this peer claim ownership at all.
    pub can_own: bool,
    /// `Hello.capabilities`: the bits that gate a transition.
    pub capabilities: Capabilities,
    /// `Hello.owner_serial`: the last owner claim serial the peer knows (0 = none).
    pub owner_serial: u64,
    /// `Hello.owner_node_id`: the last owner the peer knows (empty = none).
    pub owner: Option<NodeId>,
}

/// Order of ownership candidates: the lexicographic pair `(serial, node_id)`.
///
/// «порядок лексикографический: кандидат новее тогда и только тогда, когда его пара
/// больше; ... при равном `serial` побеждает больший `node_id`»
/// (`docs/05-protocol.md`, «Владение вводом»).
pub fn order(serial: u64, node: &NodeId, other_serial: u64, other: &NodeId) -> std::cmp::Ordering {
    serial.cmp(&other_serial).then_with(|| node.cmp(other))
}
