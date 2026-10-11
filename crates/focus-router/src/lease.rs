//! The lease slot: one node holds **at most one** lease at a time.
//!
//! «узел держит не более одной активной lease, и она бывает либо source-ролью
//! (собственный lease owner-а), либо receiver-ролью (чужой lease)»
//! (`docs/03-architecture.md`, «Соответствие сетевым сообщениям»). That is why the
//! two lease roles live in the two variants of [`LeaseSlot`] rather than in two
//! independent fields: the invariant «never two active leases on one node» is then a
//! property of the type, not of the update code.
//!
//! This module is pure state plus the small decisions that belong to a lease
//! (`deadline` arithmetic, challenge accounting, expiry). Emitting outputs is the
//! router's job, which keeps the invariants unit-testable without a message queue.

use geometry::{NodeId, Position};

use crate::flow::{FocusState, Identifiers};
use crate::wire::{LeaseId, LeaveReason, Nonce};

/// How many challenge nonces a receiver keeps, per `docs/05-protocol.md`
/// («хранить максимум 8»).
pub const MAX_CHALLENGES: usize = 8;

/// The one lease slot of a node.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseSlot {
    /// No lease: the node is in `Local`.
    Idle,
    /// This node owns the lease: it is the source, the peer is the receiver.
    Source(Box<SourceLease>),
    /// A peer owns the lease: this node is the receiver.
    Receiver(Box<ReceiverLease>),
}

impl LeaseSlot {
    /// No lease is held.
    pub fn is_idle(&self) -> bool {
        matches!(self, LeaseSlot::Idle)
    }

    /// The focus state this slot contributes.
    pub fn focus(&self) -> FocusState {
        match self {
            LeaseSlot::Idle => FocusState::Local,
            LeaseSlot::Source(lease) => lease.focus(),
            LeaseSlot::Receiver(lease) => lease.focus(),
        }
    }

    /// The source-side lease, when this node is the source.
    pub fn source(&self) -> Option<&SourceLease> {
        match self {
            LeaseSlot::Source(lease) => Some(lease),
            _ => None,
        }
    }

    /// The source-side lease, mutably.
    pub fn source_mut(&mut self) -> Option<&mut SourceLease> {
        match self {
            LeaseSlot::Source(lease) => Some(lease),
            _ => None,
        }
    }

    /// The receiver-side lease, when this node is the receiver.
    pub fn receiver(&self) -> Option<&ReceiverLease> {
        match self {
            LeaseSlot::Receiver(lease) => Some(lease),
            _ => None,
        }
    }

    /// The receiver-side lease, mutably.
    pub fn receiver_mut(&mut self) -> Option<&mut ReceiverLease> {
        match self {
            LeaseSlot::Receiver(lease) => Some(lease),
            _ => None,
        }
    }
}

/// Sub-state of a source-side lease (`docs/03-architecture.md`, first `stateDiagram`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceState {
    /// Temporary capture installed, `EnterPrepare` in flight.
    Preparing,
    /// `EnterCommit` sent; injection is live.
    Remote,
    /// `Leave` sent (or the lease aborted); waiting for `LeaveAck` or the guard.
    Releasing,
    /// One-shot marker used while the router moves the slot to `Idle`.
    Done,
}

/// Source-side lease: the local node owns it and drives the receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceLease {
    /// The lease id (never reused).
    pub lease_id: LeaseId,
    /// The receiver.
    pub peer: NodeId,
    /// Applied topology revision at the moment the transition started.
    pub topology_revision: u64,
    /// Entry position sent in `EnterPrepare`.
    pub entry_position: Position,
    /// Sub-state.
    pub state: SourceState,
    /// The agent reported the capture ready.
    pub capture_ready: bool,
    /// `EnterAck{accepted}` arrived (the peer is preparing).
    pub enter_ack_received: bool,
    /// The second `BeginCapture` (the `Remote` one) was requested; the next
    /// `CaptureReady` commits the lease.
    pub remote_capture_requested: bool,
    /// `EnterCommit` was sent (the lease became active). The lease deadline is
    /// anchored to this moment: «Активный deadline первоначально `commit_time +
    /// lease_ttl`».
    pub commit_ms: Option<u64>,
    /// Challenges we received and have not renewed yet (FIFO, bounded).
    pub unanswered_challenges: Vec<Nonce>,
    /// Last `input_batch_seq` the receiver acknowledged.
    pub last_applied_batch_seq: u64,
    /// Last position the receiver reported in `InjectAck`.
    pub actual_position: Option<Position>,
    /// The link to the receiver is gone; only the release guard is left.
    pub channel_gone: bool,
    /// When the lease was created, for delay accounting in tests.
    pub started_ms: u64,
}

impl SourceLease {
    /// A fresh lease in `Preparing`.
    pub fn new(
        lease_id: LeaseId,
        peer: NodeId,
        topology_revision: u64,
        entry_position: Position,
        now_ms: u64,
    ) -> Self {
        Self {
            lease_id,
            peer,
            topology_revision,
            entry_position,
            state: SourceState::Preparing,
            capture_ready: false,
            enter_ack_received: false,
            remote_capture_requested: false,
            commit_ms: None,
            unanswered_challenges: Vec::new(),
            last_applied_batch_seq: 0,
            actual_position: None,
            channel_gone: false,
            started_ms: now_ms,
        }
    }

    /// Focus state of a source lease.
    pub fn focus(&self) -> FocusState {
        match self.state {
            SourceState::Preparing => FocusState::Preparing,
            SourceState::Remote => FocusState::Remote,
            SourceState::Releasing | SourceState::Done => FocusState::Releasing,
        }
    }

    /// Injection is live only in `Remote`.
    pub fn is_active(&self) -> bool {
        self.state == SourceState::Remote
    }

    /// Remembers a challenge, keeping at most [`MAX_CHALLENGES`] (oldest dropped).
    ///
    /// Returns `true` when the nonce was new: a repeated challenge must not produce a
    /// second `LeaseRenew` (the nonce is one-use).
    pub fn remember_challenge(&mut self, nonce: Nonce) -> bool {
        if self.unanswered_challenges.contains(&nonce) {
            return false;
        }
        if self.unanswered_challenges.len() == MAX_CHALLENGES {
            self.unanswered_challenges.remove(0);
        }
        self.unanswered_challenges.push(nonce);
        true
    }

    /// Marks a challenge as renewed (one-use nonce).
    pub fn take_challenge(&mut self, nonce: &Nonce) -> bool {
        match self.unanswered_challenges.iter().position(|n| n == nonce) {
            Some(index) => {
                self.unanswered_challenges.remove(index);
                true
            }
            None => false,
        }
    }
}

/// Sub-state of a receiver-side lease (`docs/05-protocol.md`, second `stateDiagram`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiverState {
    /// `EnterPrepare` accepted; waiting for `EnterCommit` before `prepare_timeout`.
    Prepared,
    /// Injection is live.
    Active,
    /// `Leave` received (or sent, for takeover); releasing and acknowledging.
    Releasing,
    /// Terminal for this `lease_id`: «Терминально для этого lease_id, renew и input
    /// отклоняются».
    Expired,
}

/// Receiver-side lease: a peer owns it and injects into this node.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiverLease {
    /// The lease id (never reused).
    pub lease_id: LeaseId,
    /// The source (the owner).
    pub source: NodeId,
    /// Applied topology revision the source declared.
    pub topology_revision: u64,
    /// Sub-state.
    pub state: ReceiverState,
    /// When `Prepared` started (anchor for `prepare_timeout`).
    pub prepared_ms: u64,
    /// Current active deadline, absolute monotonic ms.
    pub deadline_ms: u64,
    /// Outstanding challenges with their issue times (FIFO, bounded).
    pub challenges: Vec<(Nonce, u64)>,
    /// Last applied input batch sequence.
    pub last_input_batch_seq: u64,
    /// Last moment input or a valid renew arrived (repeat guard).
    pub last_activity_ms: u64,
    /// A `Leave` we sent and are waiting to be acknowledged.
    pub pending_leave: Option<LeaveReason>,
    /// The link is gone.
    pub channel_gone: bool,
}

impl ReceiverLease {
    /// A fresh lease in `Prepared`.
    pub fn new(lease_id: LeaseId, source: NodeId, topology_revision: u64, now_ms: u64) -> Self {
        Self {
            lease_id,
            source,
            topology_revision,
            state: ReceiverState::Prepared,
            prepared_ms: now_ms,
            deadline_ms: now_ms,
            challenges: Vec::new(),
            last_input_batch_seq: 0,
            last_activity_ms: now_ms,
            pending_leave: None,
            channel_gone: false,
        }
    }

    /// Focus state of a receiver lease. While `Prepared`/`Expired` the node's own
    /// input is still local (injection has not started).
    pub fn focus(&self) -> FocusState {
        match self.state {
            ReceiverState::Prepared | ReceiverState::Expired => FocusState::Local,
            ReceiverState::Active => FocusState::Remote,
            ReceiverState::Releasing => FocusState::Releasing,
        }
    }

    /// Injection is accepted only while `Active`: «Ввод в Idle/Prepared/Expired
    /// отклоняется».
    pub fn accepts_input(&self) -> bool {
        self.state == ReceiverState::Active
    }

    /// Issues a challenge and returns `(nonce, issue_ms)`.
    ///
    /// «Receiver каждые `lease_ttl/5` выпускает `LeaseChallenge` с random nonce16,
    /// хранит собственное monotonic время выдачи». Only meaningful while `Active`.
    pub fn issue_challenge(&mut self, ids: &mut dyn Identifiers, now_ms: u64) -> (Nonce, u64) {
        let nonce = ids.nonce();
        if self.challenges.len() == MAX_CHALLENGES {
            self.challenges.remove(0);
        }
        self.challenges.push((nonce, now_ms));
        (nonce, now_ms)
    }

    /// Applies a renewal for `nonce` and returns the new deadline, or `None` when the
    /// nonce is unknown/stale or the lease is not active.
    ///
    /// The deadline is anchored to the challenge **issue** time, not to the arrival of
    /// the renew: «Новый deadline ставится на `issue_time + lease_ttl`, а не
    /// `receive_time + lease_ttl`. Поэтому задержанный старый renew не может
    /// бесконечно продлевать ввод.» The nonce is consumed, so a replay cannot extend
    /// the lease either.
    pub fn apply_renew(&mut self, nonce: &Nonce, ttl_ms: u64) -> Option<u64> {
        if self.state != ReceiverState::Active {
            return None;
        }
        let index = self.challenges.iter().position(|(n, _)| n == nonce)?;
        let (_, issue_ms) = self.challenges.remove(index);
        Some(issue_ms.saturating_add(ttl_ms))
    }

    /// Records input activity (repeat guard anchor).
    pub fn note_activity(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }
}
