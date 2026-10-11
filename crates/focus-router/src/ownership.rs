//! Ownership: the `(serial, node_id)` Lamport pair, claims, hand-off and deposition.
//!
//! `docs/05-protocol.md`, «Владение вводом»: «Владение кодируется парой `(serial,
//! node_id)`: порядок лексикографический — кандидат новее тогда и только тогда, когда
//! его пара больше, а при равном `serial` побеждает больший `node_id`. Это не выборы:
//! нет кворума, голосования и большинства». `docs/06-development-plan.md` (KVM-071)
//! adds: «норма — hand-off: текущий owner сначала освобождает захват (source →
//! `EndCapture`, receiver → `ReleaseSession`) и лишь затем публикует
//! `OwnerState{granted}`».
//!
//! This module holds the belief state and the pure decisions. The *ordering* of the
//! effects (release first, grant second) is enforced by [`crate::router`], which cannot
//! publish a grant while the lease slot is not idle — see [`Ownership::may_grant`].

use geometry::NodeId;

use crate::flow::OwnershipState;
use crate::wire::{order, ClaimReason, ErrorCode};

/// A claim this node made and is waiting to see granted (or to time out).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingClaim {
    /// `max_seen_serial + 1` when the claim was made.
    pub serial: u64,
    /// Why we claimed.
    pub reason: ClaimReason,
    /// When the self-grant timer fires.
    pub deadline_ms: u64,
}

/// A hand-off this node is performing: the claimant wins, we release first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingGrant {
    /// Who claimed.
    pub claimant: NodeId,
    /// The serial being granted.
    pub serial: u64,
}

/// The local node's ownership belief and state.
#[derive(Debug, Clone, PartialEq)]
pub struct Ownership {
    local: NodeId,
    state: OwnershipState,
    /// Serial of the current ownership (0 = none known).
    serial: u64,
    /// The current owner, when one is known.
    owner: Option<NodeId>,
    /// `max_seen_serial`: persisted by core, never decreased
    /// (`docs/06-development-plan.md`, KVM-071).
    max_seen_serial: u64,
    /// Our own `Hello.can_own`.
    can_own: bool,
    /// A claim of ours that is still in flight.
    pending_claim: Option<PendingClaim>,
    /// A hand-off we owe a claimant.
    pending_grant: Option<PendingGrant>,
    /// We saw a higher pair and must finish releasing before announcing the loss.
    deposed_by: Option<(u64, NodeId)>,
}

impl Ownership {
    /// A follower with `max_seen_serial = 0` and no known owner.
    pub fn new(local: NodeId, can_own: bool) -> Self {
        Self {
            local,
            state: OwnershipState::Follower,
            serial: 0,
            owner: None,
            max_seen_serial: 0,
            can_own,
            pending_claim: None,
            pending_grant: None,
            deposed_by: None,
        }
    }

    /// Restores persisted state (core persists `max_seen_serial` and the last pair).
    pub fn restore(mut self, max_seen_serial: u64, owner: Option<(u64, NodeId)>) -> Self {
        self.max_seen_serial = self.max_seen_serial.max(max_seen_serial);
        if let Some((serial, node)) = owner {
            self.max_seen_serial = self.max_seen_serial.max(serial);
            self.serial = serial;
            self.owner = Some(node);
            self.state = if node == self.local {
                OwnershipState::Owner
            } else {
                OwnershipState::Follower
            };
        }
        self
    }

    /// The local node.
    pub const fn local(&self) -> &NodeId {
        &self.local
    }

    /// Current ownership state.
    pub const fn state(&self) -> OwnershipState {
        self.state
    }

    /// Whether the local node is the owner.
    pub const fn is_owner(&self) -> bool {
        matches!(self.state, OwnershipState::Owner)
    }

    /// `Hello.can_own` of this node.
    pub const fn can_own(&self) -> bool {
        self.can_own
    }

    /// `max_seen_serial`.
    pub const fn max_seen_serial(&self) -> u64 {
        self.max_seen_serial
    }

    /// The serial of the current ownership.
    pub const fn serial(&self) -> u64 {
        self.serial
    }

    /// The owner, when known.
    pub const fn owner(&self) -> Option<&NodeId> {
        self.owner.as_ref()
    }

    /// The in-flight claim of ours.
    pub const fn pending_claim(&self) -> Option<PendingClaim> {
        self.pending_claim
    }

    /// The hand-off we owe.
    pub const fn pending_grant(&self) -> Option<PendingGrant> {
        self.pending_grant
    }

    /// The serial a *fresh* claim must carry: `max_seen + 1` (1 when `max_seen = 0`).
    pub const fn next_claim_serial(&self) -> u64 {
        self.max_seen_serial.saturating_add(1)
    }

    /// Whether `(serial, node)` is strictly newer than the current belief.
    pub fn is_newer_than_current(&self, serial: u64, node: &NodeId) -> bool {
        match self.owner {
            Some(current) => order(serial, node, self.serial, &current).is_gt(),
            None => true,
        }
    }

    /// Whether `(serial, node)` is strictly newer than anything we have seen, even
    /// without a known owner.
    pub fn is_newer_than_seen(&self, serial: u64, node: &NodeId) -> bool {
        if serial > self.max_seen_serial {
            return true;
        }
        if serial < self.max_seen_serial {
            return false;
        }
        match self.owner {
            Some(current) => node > &current,
            None => true,
        }
    }

    /// Raises `max_seen_serial` to `serial` if it is higher.
    pub fn observe_serial(&mut self, serial: u64) {
        if serial > self.max_seen_serial {
            self.max_seen_serial = serial;
        }
    }

    /// Accepts or refuses a lease message from `(serial, node)`.
    ///
    /// `docs/05-protocol.md`: «Receiver применяет `EnterPrepare` только от текущего
    /// owner: сообщение несёт `owner_serial`, и при несовпадении возвращается
    /// `EnterAck{accepted=false, reason=NOT_OWNER}` (`STALE_OWNER_SERIAL` — если serial
    /// старше известного)».
    pub fn accepts_lease_from(&self, serial: u64, node: &NodeId) -> Result<(), ErrorCode> {
        let Some(current) = self.owner else {
            // No owner is known, so nobody may open a lease.
            return Err(ErrorCode::NotOwner);
        };
        if serial < self.serial {
            return Err(ErrorCode::StaleOwnerSerial);
        }
        if order(serial, node, self.serial, &current).is_eq() {
            Ok(())
        } else {
            Err(ErrorCode::NotOwner)
        }
    }

    /// Starts our own claim: `serial = max_seen + 1` and a `claim_grant_timeout_ms`
    /// timer waiting for the current owner to hand off.
    pub fn begin_claim(&mut self, reason: ClaimReason, now_ms: u64, timeout_ms: u64) -> u64 {
        let serial = self.next_claim_serial();
        self.pending_claim = Some(PendingClaim {
            serial,
            reason,
            deadline_ms: now_ms.saturating_add(timeout_ms),
        });
        self.state = OwnershipState::Claiming;
        serial
    }

    /// Ends a claim without ownership (refusal, or the claimant gave up).
    pub fn abandon_claim(&mut self) {
        self.pending_claim = None;
        self.state = OwnershipState::Follower;
    }

    /// The self-grant path: no owner answered within `claim_grant_timeout_ms`, so we
    /// publish the same serial ourselves.
    ///
    /// Safe because the receiver is the one enforcing ownership:
    /// «Это возможно только потому, что требование безопасности лежит на получателе»
    /// (ADR-012).
    pub fn self_grant(&mut self) -> Option<(u64, NodeId)> {
        let claim = self.pending_claim.take()?;
        self.serial = claim.serial;
        self.owner = Some(self.local);
        self.observe_serial(claim.serial);
        self.state = OwnershipState::Owner;
        Some((claim.serial, self.local))
    }

    /// Adopts an ownership pair published by a peer (or by ourselves after a grant).
    pub fn adopt(&mut self, serial: u64, owner: NodeId) {
        self.observe_serial(serial);
        self.serial = serial;
        self.owner = Some(owner);
        self.pending_claim = None;
        self.pending_grant = None;
        self.deposed_by = None;
        self.state = if owner == self.local {
            OwnershipState::Owner
        } else {
            OwnershipState::Follower
        };
    }

    /// Records that we owe `claimant` the ownership at `serial`; the grant is
    /// published only after the lease slot is idle.
    pub fn begin_hand_off(&mut self, claimant: NodeId, serial: u64) {
        self.observe_serial(serial);
        self.pending_grant = Some(PendingGrant { claimant, serial });
    }

    /// Records that a higher pair exists; the announcement waits for the release.
    pub fn begin_deposition(&mut self, serial: u64, owner: NodeId) {
        self.observe_serial(serial);
        self.deposed_by = Some((serial, owner));
        self.state = OwnershipState::Deposed;
    }

    /// The pair we are being deposed by, when a deposition is in progress.
    pub const fn deposed_by(&self) -> Option<(u64, NodeId)> {
        self.deposed_by
    }

    /// May a grant be published right now?
    ///
    /// Only when the release is finished: «owner сначала завершает освобождение ... и
    /// лишь затем публикует `OwnerState{granted=true}`». The router calls this with the
    /// lease slot idle flag, which makes the ordering impossible to invert by accident.
    pub fn may_grant(&self, lease_idle: bool) -> bool {
        lease_idle && (self.pending_grant.is_some() || self.deposed_by.is_some())
    }

    /// Finishes the hand-off: drops our ownership and returns the canvas.
    pub fn complete_hand_off(&mut self) -> Option<(u64, NodeId)> {
        let grant = self.pending_grant.take()?;
        self.adopt(grant.serial, grant.claimant);
        Some((grant.serial, grant.claimant))
    }

    /// Finishes a deposition: we are a follower of the winner.
    pub fn complete_deposition(&mut self) {
        if let Some((serial, owner)) = self.deposed_by.take() {
            self.adopt(serial, owner);
        }
    }

    /// The user abandons ownership: no owner remains, the serial is kept.
    pub fn abandon(&mut self) -> u64 {
        self.owner = None;
        self.state = OwnershipState::Follower;
        self.pending_grant = None;
        self.pending_claim = None;
        self.serial
    }

    /// Whether a claim from `peer` must be refused with `CANNOT_OWN`.
    ///
    /// `can_own` comes from the claimant's `Hello` (`docs/05-protocol.md`: «кандидат
    /// проверяется по `can_own` (из `Hello`) и свежести `serial`, иначе это
    /// `STALE_OWNER_SERIAL` или `CANNOT_OWN`»). A peer we have not heard from has not
    /// declared `can_own = true`, so its claim is refused too: links exchange `Hello`
    /// before any ownership traffic, and "no evidence that it may own" must not become
    /// "may own".
    pub fn accepts_claim_from(&self, peer: &NodeId, peer_can_own: bool) -> Result<(), ErrorCode> {
        if *peer == self.local {
            // Our own claim is not answered over the wire.
            return Err(ErrorCode::CannotOwn);
        }
        if !peer_can_own {
            return Err(ErrorCode::CannotOwn);
        }
        Ok(())
    }
}
