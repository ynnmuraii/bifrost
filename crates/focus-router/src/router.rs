//! The router: one node's focus, lease and ownership machines, composed.
//!
//! Three machines, one slot:
//!
//! * [`Ownership`] — the `(serial, node_id)` pair, claims, hand-off, deposition.
//! * the **source** lease ([`SourceLease`]) — this node owns the lease and drives a
//!   receiver (`docs/03-architecture.md`, first `stateDiagram-v2`).
//! * the **receiver** lease ([`ReceiverLease`]) — a peer owns the lease and injects
//!   into this node (`docs/05-protocol.md`, second `stateDiagram-v2`).
//!
//! They are composed in one router because the docs relate them: only the owner may
//! open a lease, and an ownership change must release the current lease *first*
//! (`docs/03-architecture.md`: «owner сначала завершает свой локальный захват
//! (`EndCapture`, роль source) и освобождает session-owned состояние получателя
//! (`ReleaseSession`, роль receiver), дожидается `Released`/`CaptureReady` (то есть
//! Local), и только затем публикует `OwnerState{serial, granted=true}`»). That
//! ordering is enforced by construction here: a pending ownership change is only
//! published from [`FocusRouter::settle_ownership`], whose precondition is
//! `lease.is_idle()`.
//!
//! The two lease roles are the two variants of [`crate::lease::LeaseSlot`], so
//! «never two active leases on one node» cannot be violated by a code path — there is
//! nowhere to put a second lease.
//!
//! Everything is deterministic: no clock reads (every call takes `now_ms`), no
//! randomness ([`Identifiers`] is injected), no I/O, no async.

use std::collections::{BTreeMap, BTreeSet};

use geometry::{NodeId, Position};
use input_model::{CaptureKind, HidKey, MouseButton};

use crate::config::Timing;
use crate::flow::{
    AgentCommand, AgentEvent, FocusState, Identifiers, Input, LocalInput, Notice, Output,
    OwnershipState, SwitchTrigger, Timer, TransitionRefusal,
};
use crate::lease::{LeaseSlot, ReceiverLease, ReceiverState, SourceLease, SourceState};
use crate::ownership::Ownership;
use crate::wire::{
    order, Capabilities, ClaimReason, ErrorCode, InjectStatus, LeaseId, LeaveReason, Message,
    Nonce, PeerHello,
};

/// The conservative physical pressed set.
///
/// «Нормальный переход только с пустым physical pressed set» — the agent's ledger is
/// the authority; core mirrors it so a transition can be refused without asking the
/// agent. The emergency path ignores this set entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PressedSet {
    keys: BTreeSet<HidKey>,
    buttons: BTreeSet<MouseButton>,
}

impl PressedSet {
    /// Nothing is held.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Keys currently held.
    pub fn keys(&self) -> impl Iterator<Item = HidKey> + '_ {
        self.keys.iter().copied()
    }

    /// Buttons currently held.
    pub fn buttons(&self) -> impl Iterator<Item = MouseButton> + '_ {
        self.buttons.iter().copied()
    }
}

/// A peer link as far as this layer cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Peer {
    hello: Option<PeerHello>,
    online: bool,
}

/// The addressing of one inbound lease frame.
///
/// `docs/protocol.proto` puts `lease_id` on the `Envelope` and the sender on the link,
/// so these two always travel together; grouping them keeps the busiest handlers
/// readable (and inside clippy's arity limit) without inventing a message wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    from: NodeId,
    lease_id: LeaseId,
}

/// An ownership change that must not be published before the release is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOwnership {
    /// A claimant wins; the grant is published after we are Local.
    Grant { claimant: NodeId, serial: u64 },
    /// A higher pair appeared; the loss is announced after we are Local.
    Depose {
        serial: u64,
        owner: NodeId,
        notify: Option<NodeId>,
    },
    /// The user abandoned ownership; the announcement waits for the release.
    Abandon { serial: u64 },
    /// Our `claim_grant_timeout_ms` ran out and the claim still wins: the self-grant is
    /// published from [`FocusRouter::settle_ownership`], i.e. only once the lease slot is
    /// idle.
    ///
    /// This is what makes «смена owner-а возможна только когда все узлы в Local»
    /// (`docs/03-architecture.md`) structural rather than incidental: a claimant can be
    /// the receiver of the outgoing owner's lease, so even a self-grant has to release
    /// first — and routing it through the same settle point is what
    /// `invariant_5_grants_require_idle_slot_after_release` checks.
    SelfGrant { serial: u64 },
}

/// One node's focus/lease/ownership router.
#[derive(Debug)]
pub struct FocusRouter<I: Identifiers = crate::flow::SequentialIds> {
    local: NodeId,
    timing: Timing,
    ids: I,
    own_capabilities: Capabilities,
    require_emergency_hotkey: bool,
    claim_on_startup: bool,
    ownership: Ownership,
    lease: LeaseSlot,
    pressed: PressedSet,
    peers: BTreeMap<NodeId, Peer>,
    /// `lease_id`s that have expired or been released: «`lease_id` — random bytes16,
    /// never reused» and «После истечения deadline lease необратимо Expired ... нужен
    /// новый `EnterPrepare`».
    burned: BTreeSet<LeaseId>,
    /// The applied topology revision, checked against `EnterPrepare`.
    topology_revision: u64,
    /// The input agent is gone (`Blocked`).
    blocked: bool,
    pending_ownership: Option<PendingOwnership>,
    last_focus: FocusState,
    last_ownership: OwnershipState,
}

impl<I: Identifiers> FocusRouter<I> {
    /// A router for `local`, with `own_capabilities` as this node's `Hello` bits.
    pub fn new(local: NodeId, timing: Timing, ids: I, own_capabilities: Capabilities) -> Self {
        let last_focus = FocusState::Local;
        let last_ownership = OwnershipState::Follower;
        Self {
            ownership: Ownership::new(local, true),
            local,
            timing,
            ids,
            own_capabilities,
            require_emergency_hotkey: true,
            claim_on_startup: false,
            lease: LeaseSlot::Idle,
            pressed: PressedSet::default(),
            peers: BTreeMap::new(),
            burned: BTreeSet::new(),
            topology_revision: 0,
            blocked: false,
            pending_ownership: None,
            last_focus,
            last_ownership,
        }
    }

    /// `input.claim_on_startup` (default `true` on Windows): `on_start` claims
    /// ownership with `max_seen + 1`.
    pub fn with_claim_on_startup(mut self, claim_on_startup: bool) -> Self {
        self.claim_on_startup = claim_on_startup;
        self
    }

    /// `input.require_emergency_hotkey` (default `true`): without an emergency hotkey
    /// capability, `true` makes this node unable to be a source (AC-03).
    pub fn with_require_emergency_hotkey(mut self, require: bool) -> Self {
        self.require_emergency_hotkey = require;
        self
    }

    /// Restores the persisted ownership counters.
    pub fn with_persisted_ownership(
        mut self,
        max_seen_serial: u64,
        owner: Option<(u64, NodeId)>,
    ) -> Self {
        self.ownership = self.ownership.restore(max_seen_serial, owner);
        self.last_ownership = self.ownership.state();
        self
    }

    /// The local node.
    pub const fn local(&self) -> &NodeId {
        &self.local
    }

    /// The timings in use.
    pub const fn timing(&self) -> &Timing {
        &self.timing
    }

    /// The current focus state (including `Blocked`).
    pub fn focus(&self) -> FocusState {
        if self.blocked {
            FocusState::Blocked
        } else {
            self.lease.focus()
        }
    }

    /// The lease slot.
    pub const fn lease(&self) -> &LeaseSlot {
        &self.lease
    }

    /// The ownership machine.
    pub const fn ownership(&self) -> &Ownership {
        &self.ownership
    }

    /// Whether this node currently owns the workspace.
    pub fn is_owner(&self) -> bool {
        self.ownership.is_owner()
    }

    /// The current owner, when one is known.
    pub fn owner(&self) -> Option<&NodeId> {
        self.ownership.owner()
    }

    /// The current ownership serial.
    pub fn serial(&self) -> u64 {
        self.ownership.serial()
    }

    /// `max_seen_serial` (persisted by core).
    pub fn max_seen_serial(&self) -> u64 {
        self.ownership.max_seen_serial()
    }

    /// The applied topology revision.
    pub const fn topology_revision(&self) -> u64 {
        self.topology_revision
    }

    /// The pressed set core mirrors from the agent.
    pub const fn pressed(&self) -> &PressedSet {
        &self.pressed
    }

    /// Whether the input agent is considered gone.
    pub const fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// Sets the applied topology revision (the caller owns the layout document).
    pub fn set_topology_revision(&mut self, revision: u64) {
        self.topology_revision = revision;
    }

    /// Startup: claim ownership when `claim_on_startup` is set.
    pub fn on_start(&mut self, now_ms: u64) -> Vec<Output> {
        let mut out = Vec::new();
        if self.claim_on_startup && !self.ownership.is_owner() {
            self.begin_claim(now_ms, ClaimReason::Startup, &mut out);
        }
        out
    }

    /// Whether this node may be the source at all: it needs `CAPTURE` and, when
    /// `input.require_emergency_hotkey` is set, `CAPABILITY_EMERGENCY_HOTKEY`
    /// (AC-03: «Источник без этой capability не входит в Remote»).
    pub fn can_be_source(&self) -> bool {
        self.own_capabilities.capture
            && (!self.require_emergency_hotkey || self.own_capabilities.emergency_hotkey)
    }

    /// Whether this node may be a receiver: `INJECT` + `ABSOLUTE_POINTER`.
    pub fn can_be_receiver(&self) -> bool {
        self.own_capabilities.receiver_ready()
    }

    /// Handles one input at `now_ms` and returns the outputs it produced.
    pub fn handle(&mut self, now_ms: u64, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::TimerExpired(timer) => self.on_timer(now_ms, timer, &mut out),
            Input::Hello { peer, hello } => self.on_hello(now_ms, peer, hello, &mut out),
            Input::LinkDown { peer } => self.on_link_down(now_ms, peer, &mut out),
            Input::Agent(event) => self.on_agent(now_ms, event, &mut out),
            Input::Local(local) => self.on_local(now_ms, local, &mut out),
            Input::Net { from, message } => self.on_net(now_ms, from, message, &mut out),
        }
        // Ordering invariant: never publish an ownership change before the release.
        self.settle_ownership(&mut out);
        self.emit_ownership(&mut out);
        self.emit_focus(&mut out);
        out
    }

    // ---------------------------------------------------------------- ownership

    fn on_hello(&mut self, now_ms: u64, peer: NodeId, hello: PeerHello, out: &mut Vec<Output>) {
        self.peers.insert(
            peer,
            Peer {
                hello: Some(hello),
                online: true,
            },
        );
        // Hello carries the owner pair as gossip: a strictly newer pair wins, exactly
        // like an OwnerState, and a deposed owner must return to Local at once.
        if let Some(owner) = hello.owner {
            if hello.owner_serial > 0
                && owner != self.local
                && self
                    .ownership
                    .is_newer_than_current(hello.owner_serial, &owner)
            {
                self.begin_deposition(now_ms, hello.owner_serial, owner, Some(peer), out);
            }
            self.ownership.observe_serial(hello.owner_serial);
        }
    }

    fn on_local(&mut self, now_ms: u64, local: LocalInput, out: &mut Vec<Output>) {
        match local {
            LocalInput::SwitchRequest {
                trigger,
                target,
                entry_position,
                topology_revision,
            } => self.on_switch(
                now_ms,
                trigger,
                target,
                entry_position,
                topology_revision,
                out,
            ),
            LocalInput::Emergency => self.on_emergency(now_ms, out),
            LocalInput::Claim { reason } => self.begin_claim(now_ms, reason, out),
            LocalInput::Abandon => self.on_abandon(out),
            LocalInput::SuspendOrLock => {
                self.on_session_end(now_ms, LeaveReason::SuspendOrLock, out)
            }
            LocalInput::Resume => {}
            LocalInput::TopologyChanged { revision } => {
                self.topology_revision = revision;
                self.on_session_end(now_ms, LeaveReason::TopologyChanged, out);
            }
            LocalInput::DisplaysChanged => {
                self.on_session_end(now_ms, LeaveReason::TopologyChanged, out);
            }
        }
    }

    fn on_switch(
        &mut self,
        now_ms: u64,
        trigger: SwitchTrigger,
        target: NodeId,
        entry_position: Position,
        topology_revision: u64,
        out: &mut Vec<Output>,
    ) {
        if self.blocked {
            return self.refuse(trigger, TransitionRefusal::AgentBlocked, out);
        }
        match &self.lease {
            LeaseSlot::Idle => {
                if !self.ownership.is_owner() {
                    return self.refuse(trigger, TransitionRefusal::NotOwner, out);
                }
                if !self.can_be_source() {
                    return self.refuse(trigger, TransitionRefusal::SourceCapabilityMissing, out);
                }
                if !self.pressed.is_empty() {
                    return self.refuse(trigger, TransitionRefusal::PressedInputHeld, out);
                }
                if !self.peer_is_receiver_ready(&target) {
                    return self.refuse(trigger, TransitionRefusal::ReceiverUnsupported, out);
                }
                let lease_id = self.ids.lease_id();
                let entry_position = Position::new(
                    entry_position.display_id.clone(),
                    entry_position.x,
                    entry_position.y,
                );
                self.lease = LeaseSlot::Source(Box::new(SourceLease::new(
                    lease_id,
                    target,
                    topology_revision,
                    entry_position,
                    now_ms,
                )));
                out.push(Output::Agent(AgentCommand::BeginCapture {
                    kind: CaptureKind::Temporary,
                    lease_id,
                    self_timeout_ms: self.timing.prepare_timeout_ms,
                }));
                out.push(Output::ArmTimer {
                    timer: Timer::PrepareTimeout,
                    deadline_ms: now_ms.saturating_add(self.timing.prepare_timeout_ms),
                });
            }
            LeaseSlot::Source(lease) if lease.state == SourceState::Remote => {
                let (peer, lease_id) = (lease.peer, lease.lease_id);
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::Send {
                    peer,
                    message: Message::Leave {
                        lease_id,
                        reason: trigger.leave_reason(),
                    },
                });
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                if let Some(lease) = self.lease.source_mut() {
                    lease.state = SourceState::Releasing;
                }
            }
            LeaseSlot::Source(lease) if lease.state == SourceState::Preparing => {
                // A second trigger while preparing cancels the attempt and replays the
                // buffered events locally: «Нельзя «потерять» нажатие в Preparing».
                let lease_id = lease.lease_id;
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: true,
                }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
            // Releasing, or we are the receiver: only the owner opens leases.
            _ => self.refuse(trigger, TransitionRefusal::NotOwner, out),
        }
    }

    fn on_emergency(&mut self, _now_ms: u64, out: &mut Vec<Output>) {
        match &self.lease {
            LeaseSlot::Idle => {}
            LeaseSlot::Source(lease) => {
                let (peer, lease_id) = (lease.peer, lease.lease_id);
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::Send {
                    peer,
                    message: Message::Leave {
                        lease_id,
                        reason: LeaveReason::Emergency,
                    },
                });
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
            LeaseSlot::Receiver(lease) => {
                let (peer, lease_id) = (lease.source, lease.lease_id);
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::Send {
                    peer,
                    message: Message::Leave {
                        lease_id,
                        reason: LeaveReason::Emergency,
                    },
                });
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
        }
    }

    fn begin_claim(&mut self, now_ms: u64, reason: ClaimReason, out: &mut Vec<Output>) {
        if !self.ownership.can_own() {
            out.push(Output::Notice(Notice::ClaimRefused {
                code: Some(ErrorCode::CannotOwn),
            }));
            return;
        }
        if self.ownership.pending_claim().is_some() {
            return;
        }
        let serial = self
            .ownership
            .begin_claim(reason, now_ms, self.timing.claim_grant_timeout_ms);
        out.push(Output::Broadcast(Message::OwnerClaim { serial, reason }));
        out.push(Output::ArmTimer {
            timer: Timer::ClaimGrant,
            deadline_ms: now_ms.saturating_add(self.timing.claim_grant_timeout_ms),
        });
    }

    fn on_abandon(&mut self, out: &mut Vec<Output>) {
        if !self.ownership.is_owner() {
            return;
        }
        let serial = self.ownership.abandon();
        self.pending_ownership = Some(PendingOwnership::Abandon { serial });
        out.push(Output::CancelTimer(Timer::ClaimGrant));
    }

    fn on_owner_claim(
        &mut self,
        _now_ms: u64,
        from: NodeId,
        serial: u64,
        on_claim: ClaimReason,
        out: &mut Vec<Output>,
    ) {
        let _ = on_claim;
        let peer_can_own = self
            .peers
            .get(&from)
            .and_then(|peer| peer.hello)
            .map(|hello| hello.can_own)
            .unwrap_or(false);
        if let Err(code) = self.ownership.accepts_claim_from(&from, peer_can_own) {
            out.push(Output::Send {
                peer: from,
                message: Message::ProtocolError {
                    code,
                    related_seq: 0,
                },
            });
            out.push(Output::Notice(Notice::Rejected { peer: from, code }));
            return;
        }
        // «заявка со старым serial отвергается STALE_OWNER_SERIAL»
        if serial < self.ownership.max_seen_serial() {
            out.push(Output::Send {
                peer: from,
                message: Message::ProtocolError {
                    code: ErrorCode::StaleOwnerSerial,
                    related_seq: 0,
                },
            });
            out.push(Output::Notice(Notice::Rejected {
                peer: from,
                code: ErrorCode::StaleOwnerSerial,
            }));
            return;
        }
        if !self.ownership.is_newer_than_current(serial, &from) {
            // Our belief already wins the ordering: answer with it so the claimant
            // «видит, кто владелец» instead of waiting for a timeout in the dark.
            if let Some(owner) = self.ownership.owner().copied() {
                out.push(Output::Send {
                    peer: from,
                    message: Message::OwnerState {
                        serial: self.ownership.serial(),
                        owner: Some(owner),
                        granted: false,
                    },
                });
            }
            return;
        }
        let _ = self.begin_deposition_for_claim(from, serial, out);
    }

    /// A claim strictly newer than our belief: hand off if we own, otherwise record.
    fn begin_deposition_for_claim(
        &mut self,
        from: NodeId,
        serial: u64,
        out: &mut Vec<Output>,
    ) -> bool {
        if self.ownership.is_owner() {
            self.ownership.begin_hand_off(from, serial);
            self.pending_ownership = Some(PendingOwnership::Grant {
                claimant: from,
                serial,
            });
            self.release_lease_for_ownership(out);
            return true;
        }
        // A follower cannot grant; record the serial and answer only with an older
        // owner it knows, so the claimant's self-grant timer decides otherwise.
        self.ownership.observe_serial(serial);
        if let Some(owner) = self.ownership.owner().copied() {
            out.push(Output::Send {
                peer: from,
                message: Message::OwnerState {
                    serial: self.ownership.serial(),
                    owner: Some(owner),
                    granted: false,
                },
            });
        }
        false
    }

    fn on_owner_state(
        &mut self,
        now_ms: u64,
        _from: NodeId,
        serial: u64,
        owner: Option<NodeId>,
        granted: bool,
        out: &mut Vec<Output>,
    ) {
        let Some(owner) = owner else {
            // "no owner": keep waiting; only an explicit claim decides.
            if !granted {
                self.ownership.observe_serial(serial);
            }
            return;
        };
        if !self.ownership.is_newer_than_current(serial, &owner) {
            // Confirms our own ownership, or is stale: adopt it only when it is ours.
            if owner == self.local && serial == self.ownership.serial() {
                self.ownership.adopt(serial, owner);
            }
            return;
        }
        if owner == self.local {
            self.ownership.adopt(serial, owner);
            return;
        }
        if self.ownership.is_owner() {
            self.begin_deposition(now_ms, serial, owner, None, out);
            return;
        }
        if self.ownership.pending_claim().is_some() {
            self.ownership.abandon_claim();
            out.push(Output::CancelTimer(Timer::ClaimGrant));
            out.push(Output::Notice(Notice::ClaimRefused { code: None }));
        }
        self.ownership.adopt(serial, owner);
    }

    fn begin_deposition(
        &mut self,
        _now_ms: u64,
        serial: u64,
        owner: NodeId,
        notify: Option<NodeId>,
        out: &mut Vec<Output>,
    ) {
        if self.ownership.deposed_by().is_some() {
            return;
        }
        let notify = notify.or_else(|| self.lease_peer());
        self.ownership.begin_deposition(serial, owner);
        out.push(Output::Notice(Notice::OwnershipLost { serial, owner }));
        self.pending_ownership = Some(PendingOwnership::Depose {
            serial,
            owner,
            notify,
        });
        self.release_lease_for_ownership(out);
    }

    fn on_protocol_error(
        &mut self,
        _now_ms: u64,
        from: NodeId,
        code: ErrorCode,
        out: &mut Vec<Output>,
    ) {
        match code {
            ErrorCode::CannotOwn | ErrorCode::StaleOwnerSerial => {
                if self.ownership.pending_claim().is_some() {
                    self.ownership.abandon_claim();
                    out.push(Output::CancelTimer(Timer::ClaimGrant));
                }
                out.push(Output::Notice(Notice::ClaimRefused { code: Some(code) }));
            }
            ErrorCode::NotOwner | ErrorCode::UnsupportedSession | ErrorCode::CaptureFailed => {
                out.push(Output::Notice(Notice::Rejected { peer: from, code }));
                self.cancel_preparing_source(from, out);
            }
            _ => {
                out.push(Output::Notice(Notice::Rejected { peer: from, code }));
            }
        }
    }

    /// Cancels a source lease that is still preparing toward `peer`.
    fn cancel_preparing_source(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        let lease = match &self.lease {
            LeaseSlot::Source(lease)
                if lease.peer == peer && lease.state == SourceState::Preparing =>
            {
                lease
            }
            _ => return,
        };
        let lease_id = lease.lease_id;
        out.push(Output::Agent(AgentCommand::EndCapture {
            lease_id,
            replay_buffered_locally: true,
        }));
        out.push(Output::CancelTimer(Timer::PrepareTimeout));
        self.lease = LeaseSlot::Idle;
        self.burned.insert(lease_id);
    }

    /// The role split of an ownership change: `source → EndCapture`,
    /// `receiver → ReleaseSession`, and nothing is published until the agent answers.
    fn release_lease_for_ownership(&mut self, out: &mut Vec<Output>) {
        let depose = matches!(
            self.pending_ownership,
            Some(PendingOwnership::Depose { .. })
        );
        match &self.lease {
            LeaseSlot::Idle => {}
            LeaseSlot::Source(lease) => {
                let (peer, lease_id) = (lease.peer, lease.lease_id);
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
                if depose {
                    // «разослать `Leave{OWNERSHIP_LOST}` по активным получателям»
                    out.push(Output::Send {
                        peer,
                        message: Message::Leave {
                            lease_id,
                            reason: LeaveReason::OwnershipLost,
                        },
                    });
                }
                if let Some(lease) = self.lease.source_mut() {
                    lease.state = SourceState::Releasing;
                }
            }
            LeaseSlot::Receiver(lease) => {
                let (peer, lease_id) = (lease.source, lease.lease_id);
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                if depose {
                    out.push(Output::Send {
                        peer,
                        message: Message::Leave {
                            lease_id,
                            reason: LeaveReason::OwnershipLost,
                        },
                    });
                }
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Releasing;
                }
            }
        }
    }

    /// Publishes a pending ownership change, but only once the lease is released.
    fn settle_ownership(&mut self, out: &mut Vec<Output>) {
        let Some(pending) = self.pending_ownership else {
            return;
        };
        if !self.lease.is_idle() {
            return;
        }
        match pending {
            PendingOwnership::Grant { claimant, serial } => {
                self.ownership.complete_hand_off();
                out.push(Output::Broadcast(Message::OwnerState {
                    serial,
                    owner: Some(claimant),
                    granted: true,
                }));
            }
            PendingOwnership::Depose { .. } => {
                self.ownership.complete_deposition();
            }
            PendingOwnership::Abandon { serial } => {
                out.push(Output::Broadcast(Message::OwnerState {
                    serial,
                    owner: None,
                    granted: false,
                }));
            }
            PendingOwnership::SelfGrant { .. } => {
                // The release has landed; the claim must still exist and still win (a
                // refusal or a higher pair may have arrived while we were releasing).
                if self.ownership.pending_claim().is_some() && self.claim_still_wins() {
                    if let Some((serial, owner)) = self.ownership.self_grant() {
                        out.push(Output::Broadcast(Message::OwnerState {
                            serial,
                            owner: Some(owner),
                            granted: true,
                        }));
                    }
                } else {
                    self.ownership.abandon_claim();
                    out.push(Output::Notice(Notice::ClaimRefused { code: None }));
                }
            }
        }
        self.pending_ownership = None;
    }

    fn on_session_end(&mut self, _now_ms: u64, reason: LeaveReason, out: &mut Vec<Output>) {
        match &self.lease {
            LeaseSlot::Idle => {}
            LeaseSlot::Source(lease) => {
                let (peer, lease_id) = (lease.peer, lease.lease_id);
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::Send {
                    peer,
                    message: Message::Leave { lease_id, reason },
                });
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
            LeaseSlot::Receiver(lease) => {
                let (peer, lease_id) = (lease.source, lease.lease_id);
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::Send {
                    peer,
                    message: Message::Leave { lease_id, reason },
                });
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
        }
    }

    fn on_timer(&mut self, now_ms: u64, timer: Timer, out: &mut Vec<Output>) {
        match timer {
            Timer::PrepareTimeout => match &self.lease {
                LeaseSlot::Source(lease) if lease.state == SourceState::Preparing => {
                    let lease_id = lease.lease_id;
                    out.push(Output::Agent(AgentCommand::EndCapture {
                        lease_id,
                        replay_buffered_locally: true,
                    }));
                    self.lease = LeaseSlot::Idle;
                    self.burned.insert(lease_id);
                }
                LeaseSlot::Receiver(lease) if lease.state == ReceiverState::Prepared => {
                    // «Prepared expires через prepare_timeout (по умолчанию 200 ms)»
                    let lease_id = lease.lease_id;
                    out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                    if let Some(lease) = self.lease.receiver_mut() {
                        lease.state = ReceiverState::Expired;
                    }
                    self.burned.insert(lease_id);
                    out.push(Output::Notice(Notice::LeaseExpired { lease_id }));
                }
                _ => {}
            },
            Timer::SourceLeaseWatchdog => {
                if let LeaseSlot::Source(lease) = &self.lease {
                    if lease.state == SourceState::Remote {
                        let (peer, lease_id) = (lease.peer, lease.lease_id);
                        // «Source при отсутствии `LeaseStatus` за `lease_ttl` закрывает
                        // канал и восстанавливает локальное управление».
                        out.push(Output::Agent(AgentCommand::EndCapture {
                            lease_id,
                            replay_buffered_locally: false,
                        }));
                        out.push(Output::CloseLink {
                            peer,
                            code: ErrorCode::LeaseExpired,
                        });
                        self.lease = LeaseSlot::Idle;
                        self.burned.insert(lease_id);
                        out.push(Output::Notice(Notice::LeaseExpired { lease_id }));
                    }
                }
            }
            Timer::ReceiverLeaseDeadline => {
                if let LeaseSlot::Receiver(lease) = &self.lease {
                    if lease.state == ReceiverState::Active {
                        // «Active --> Expired: deadline passed or suspend or lock»
                        let lease_id = lease.lease_id;
                        out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                        if let Some(lease) = self.lease.receiver_mut() {
                            lease.state = ReceiverState::Expired;
                        }
                        self.burned.insert(lease_id);
                        out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                        out.push(Output::CancelTimer(Timer::RepeatGuard));
                        out.push(Output::Notice(Notice::LeaseExpired { lease_id }));
                    }
                }
            }
            Timer::ReceiverChallenge => {
                let active = matches!(
                    &self.lease,
                    LeaseSlot::Receiver(lease) if lease.state == ReceiverState::Active
                );
                if active {
                    let deadline_ms = self.issue_challenge(now_ms, out);
                    out.push(Output::ArmTimer {
                        timer: Timer::ReceiverChallenge,
                        deadline_ms,
                    });
                }
            }
            Timer::RepeatGuard => {
                if let LeaseSlot::Receiver(lease) = &self.lease {
                    if lease.state == ReceiverState::Active
                        && now_ms
                            >= lease
                                .last_activity_ms
                                .saturating_add(self.timing.repeat_guard_ms)
                    {
                        // AC-14: stop autorepeat after `repeat_guard_ms` of silence in
                        // both input and renew.
                        out.push(Output::Notice(Notice::RepeatGuardElapsed {
                            lease_id: lease.lease_id,
                        }));
                    }
                }
            }
            Timer::ClaimGrant => {
                let Some(claim) = self.ownership.pending_claim() else {
                    return;
                };
                if now_ms < claim.deadline_ms {
                    return;
                }
                if !self.claim_still_wins() {
                    self.ownership.abandon_claim();
                    out.push(Output::Notice(Notice::ClaimRefused { code: None }));
                    return;
                }
                // Release first, grant from `settle_ownership`. The old owner is
                // unreachable (that is why we are self-granting), but we may still be the
                // receiver of its lease, and a node must be in Local before it becomes the
                // owner.
                let serial = claim.serial;
                self.pending_ownership = Some(PendingOwnership::SelfGrant { serial });
                self.release_lease_for_ownership(out);
            }
            Timer::ReleaseGuard => match &self.lease {
                LeaseSlot::Source(lease) if lease.state == SourceState::Releasing => {
                    let lease_id = lease.lease_id;
                    self.burned.insert(lease_id);
                    self.lease = LeaseSlot::Idle;
                }
                LeaseSlot::Receiver(lease) if lease.state == ReceiverState::Releasing => {
                    let lease_id = lease.lease_id;
                    self.burned.insert(lease_id);
                    self.lease = LeaseSlot::Idle;
                }
                _ => {}
            },
        }
    }

    /// Whether our pending claim is still the newest pair we know.
    fn claim_still_wins(&self) -> bool {
        let Some(claim) = self.ownership.pending_claim() else {
            return false;
        };
        if claim.serial < self.ownership.max_seen_serial() {
            return false;
        }
        match self.ownership.owner() {
            None => true,
            Some(owner) if *owner == self.local => true,
            Some(owner) => order(claim.serial, &self.local, self.ownership.serial(), owner).is_gt(),
        }
    }

    // -------------------------------------------------------------------- agent

    fn on_agent(&mut self, now_ms: u64, event: AgentEvent, out: &mut Vec<Output>) {
        match event {
            AgentEvent::PressedState { keys, buttons } => {
                self.pressed = PressedSet {
                    keys: keys.into_iter().collect(),
                    buttons: buttons.into_iter().collect(),
                };
            }
            AgentEvent::AgentReady => {
                self.blocked = false;
            }
            AgentEvent::AgentLost => {
                self.on_session_end(now_ms, LeaveReason::AgentLost, out);
                self.blocked = true;
            }
            AgentEvent::CaptureReady { lease_id } => self.on_capture_ready(now_ms, lease_id, out),
            AgentEvent::Ready { lease_id } => self.on_agent_ready(now_ms, lease_id, out),
            AgentEvent::Released { lease_id } => self.on_released(lease_id, out),
            AgentEvent::InjectResult {
                lease_id,
                input_batch_seq,
                applied_count,
                actual_position,
                status,
            } => self.on_agent_inject_result(
                lease_id,
                input_batch_seq,
                applied_count,
                actual_position,
                status,
                out,
            ),
            AgentEvent::LocalInputDetected { lease_id } => {
                self.on_local_input_detected(lease_id, out);
            }
            AgentEvent::DeadlineExpired { lease_id } => {
                self.on_agent_deadline_expired(lease_id, out);
            }
            AgentEvent::Error { lease_id, code } => match code {
                ErrorCode::AgentLost => {
                    self.on_session_end(now_ms, LeaveReason::AgentLost, out);
                    self.blocked = true;
                }
                // «Preparing --> Local: timeout 200 ms or capture failed or NACK»
                // (`docs/05-protocol.md`): the agent's capture/session failure cancels the
                // transition, and a buffered press is replayed locally («Нельзя «потерять»
                // нажатие в Preparing»).
                ErrorCode::CaptureFailed | ErrorCode::UnsupportedSession => match lease_id {
                    Some(lease_id) => self.cancel_failed_transition(lease_id, code, out),
                    None => out.push(Output::Notice(Notice::Rejected {
                        peer: self.local,
                        code,
                    })),
                },
                _ => out.push(Output::Notice(Notice::Rejected {
                    peer: self.local,
                    code,
                })),
            },
        }
    }

    /// Cancels the lease an agent failure was reported for.
    fn cancel_failed_transition(
        &mut self,
        lease_id: LeaseId,
        code: ErrorCode,
        out: &mut Vec<Output>,
    ) {
        match &self.lease {
            LeaseSlot::Source(lease) if lease.lease_id == lease_id => {
                // A temporary capture that failed can be replayed locally; a live capture
                // cannot, so it is only released.
                let replay = lease.state != SourceState::Remote;
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: replay,
                }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
            }
            LeaseSlot::Receiver(lease) if lease.lease_id == lease_id => {
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Releasing;
                }
                self.burned.insert(lease_id);
            }
            _ => {}
        }
        out.push(Output::Notice(Notice::Rejected {
            peer: self.local,
            code,
        }));
    }

    fn on_capture_ready(&mut self, now_ms: u64, lease_id: LeaseId, out: &mut Vec<Output>) {
        let Some(lease) = self.lease.source() else {
            return;
        };
        if lease.lease_id != lease_id {
            return;
        }
        if lease.state == SourceState::Releasing {
            // `docs/03-architecture.md` («Смена владения и роли lease»): the owner
            // «дожидается `Released`/`CaptureReady` (то есть Local)», so a source that was
            // asked to end its capture may be answered by either message. Treating the
            // capture-ready confirmation as the release is what lets a hand-off finish.
            self.on_released(lease_id, out);
            return;
        }
        if lease.state != SourceState::Preparing {
            return;
        }
        if !lease.capture_ready {
            // First `BeginCapture` answered: ask the receiver to prepare.
            let (peer, topology_revision, position, owner_serial) = (
                lease.peer,
                lease.topology_revision,
                lease.entry_position.clone(),
                self.serial(),
            );
            if let Some(lease) = self.lease.source_mut() {
                lease.capture_ready = true;
            }
            out.push(Output::Send {
                peer,
                message: Message::EnterPrepare {
                    lease_id,
                    topology_revision,
                    position,
                    owner_serial,
                },
            });
            return;
        }
        if !lease.remote_capture_requested {
            return;
        }
        // Second `BeginCapture` (the Remote one) answered: commit.
        let peer = lease.peer;
        let unanswered: Vec<_> = lease.unanswered_challenges.clone();
        if let Some(lease) = self.lease.source_mut() {
            lease.state = SourceState::Remote;
            lease.commit_ms = Some(now_ms);
        }
        out.push(Output::CancelTimer(Timer::PrepareTimeout));
        out.push(Output::Send {
            peer,
            message: Message::EnterCommit { lease_id },
        });
        for nonce in unanswered {
            out.push(Output::Send {
                peer,
                message: Message::LeaseRenew { lease_id, nonce },
            });
        }
        out.push(Output::ArmTimer {
            timer: Timer::SourceLeaseWatchdog,
            deadline_ms: self.timing.lease_deadline_ms(now_ms),
        });
    }

    fn on_agent_ready(&mut self, now_ms: u64, lease_id: LeaseId, out: &mut Vec<Output>) {
        let Some(lease) = self.lease.receiver() else {
            return;
        };
        if lease.lease_id != lease_id || lease.state != ReceiverState::Prepared {
            return;
        }
        let source = lease.source;
        // «Idle --> Prepared: EnterPrepare valid, adapter ready, send EnterAck and first
        // challenge»
        out.push(Output::Send {
            peer: source,
            message: Message::EnterAck {
                lease_id,
                accepted: true,
                reason: None,
            },
        });
        self.issue_challenge(now_ms, out);
        out.push(Output::ArmTimer {
            timer: Timer::PrepareTimeout,
            deadline_ms: now_ms.saturating_add(self.timing.prepare_timeout_ms),
        });
    }

    /// Issues one challenge and returns when the next one is due.
    fn issue_challenge(&mut self, now_ms: u64, out: &mut Vec<Output>) -> u64 {
        let Some(lease) = self.lease.receiver_mut() else {
            return now_ms;
        };
        let lease_id = lease.lease_id;
        let source = lease.source;
        let (nonce, _) = lease.issue_challenge(&mut self.ids, now_ms);
        out.push(Output::Send {
            peer: source,
            message: Message::LeaseChallenge {
                lease_id,
                nonce,
                ttl_ms: u32::try_from(self.timing.lease_ttl_ms).unwrap_or(u32::MAX),
            },
        });
        now_ms.saturating_add(self.timing.challenge_interval_ms())
    }

    fn on_released(&mut self, lease_id: LeaseId, out: &mut Vec<Output>) {
        let matches = match &self.lease {
            LeaseSlot::Source(lease) => lease.lease_id == lease_id,
            LeaseSlot::Receiver(lease) => lease.lease_id == lease_id,
            LeaseSlot::Idle => false,
        };
        if !matches {
            return;
        }
        let owe_leave_ack = match &self.lease {
            LeaseSlot::Receiver(lease) => {
                lease.pending_leave.is_none() && lease.state == ReceiverState::Releasing
            }
            _ => false,
        };
        if owe_leave_ack {
            if let LeaseSlot::Receiver(lease) = &self.lease {
                let (peer, lease_id) = (lease.source, lease.lease_id);
                out.push(Output::Send {
                    peer,
                    message: Message::LeaveAck { lease_id },
                });
            }
        }
        self.burned.insert(lease_id);
        self.lease = LeaseSlot::Idle;
        out.push(Output::CancelTimer(Timer::PrepareTimeout));
        out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
        out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
        out.push(Output::CancelTimer(Timer::ReceiverChallenge));
        out.push(Output::CancelTimer(Timer::RepeatGuard));
        out.push(Output::CancelTimer(Timer::ReleaseGuard));
    }

    /// The receiver's agent reported the outcome of one batch (IPC mapping table:
    /// «`InjectResult` → `InjectAck`»).
    fn on_agent_inject_result(
        &mut self,
        lease_id: LeaseId,
        input_batch_seq: u64,
        applied_count: u32,
        actual_position: Option<Position>,
        status: InjectStatus,
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.receiver() else {
            return;
        };
        if lease.lease_id != lease_id {
            return;
        }
        let source = lease.source;
        out.push(Output::Send {
            peer: source,
            message: Message::InjectAck {
                lease_id,
                input_batch_seq,
                applied_count,
                actual_position,
                status,
            },
        });
        if !status.aborts_lease() {
            return;
        }
        // «Active --> Expired: inject failed or ledger mismatch or IPC lost»; the abort
        // is terminal for this `lease_id` and is reported to the source immediately, so
        // both sides converge without waiting for a watchdog.
        out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
        if let Some(lease) = self.lease.receiver_mut() {
            lease.state = ReceiverState::Expired;
        }
        self.burned.insert(lease_id);
        out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
        out.push(Output::CancelTimer(Timer::ReceiverChallenge));
        out.push(Output::CancelTimer(Timer::RepeatGuard));
        out.push(Output::Notice(Notice::ReceiverAborted {
            lease_id,
            code: ErrorCode::InjectionFailed,
        }));
    }

    /// The source received the receiver's `InjectAck` for one batch.
    fn on_inject_ack(
        &mut self,
        from: NodeId,
        lease_id: LeaseId,
        input_batch_seq: u64,
        actual_position: Option<Position>,
        status: InjectStatus,
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.source_mut() else {
            return;
        };
        if lease.peer != from || lease.lease_id != lease_id {
            return;
        }
        lease.last_applied_batch_seq = input_batch_seq;
        if actual_position.is_some() {
            lease.actual_position = actual_position;
        }
        if !status.aborts_lease() {
            return;
        }
        // The receiver gave up on the lease («`PARTIAL`/`FAILED` отменяют lease»), so
        // this side restores local control at once instead of waiting for the
        // watchdog. No `Leave` is sent: there is no `LeaveReason` for "the receiver
        // aborted injection" and the receiver's own lease is already dead.
        out.push(Output::Agent(AgentCommand::EndCapture {
            lease_id,
            replay_buffered_locally: false,
        }));
        out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
        self.lease = LeaseSlot::Idle;
        self.burned.insert(lease_id);
        out.push(Output::Notice(Notice::InjectionAborted {
            lease_id,
            code: ErrorCode::InjectionFailed,
        }));
    }

    fn on_local_input_detected(&mut self, lease_id: LeaseId, out: &mut Vec<Output>) {
        let Some(lease) = self.lease.receiver() else {
            return;
        };
        if lease.lease_id != lease_id || lease.state != ReceiverState::Active {
            return;
        }
        let source = lease.source;
        // «Takeover: receiver в Active при физическом вводе сначала освобождает
        // session-owned ledger, затем отправляет `Leave{RECEIVER_LOCAL_INPUT}`».
        out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
        out.push(Output::Send {
            peer: source,
            message: Message::Leave {
                lease_id,
                reason: LeaveReason::ReceiverLocalInput,
            },
        });
        out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
        out.push(Output::CancelTimer(Timer::ReceiverChallenge));
        out.push(Output::CancelTimer(Timer::RepeatGuard));
        if let Some(lease) = self.lease.receiver_mut() {
            lease.state = ReceiverState::Releasing;
            lease.pending_leave = Some(LeaveReason::ReceiverLocalInput);
        }
        out.push(Output::Notice(Notice::Takeover { lease_id }));
    }

    fn on_agent_deadline_expired(&mut self, lease_id: LeaseId, out: &mut Vec<Output>) {
        match &self.lease {
            LeaseSlot::Source(lease) if lease.lease_id == lease_id => {
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
                out.push(Output::Notice(Notice::LeaseExpired { lease_id }));
            }
            LeaseSlot::Receiver(lease) if lease.lease_id == lease_id => {
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Expired;
                }
                self.burned.insert(lease_id);
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                out.push(Output::Notice(Notice::ReceiverAborted {
                    lease_id,
                    code: ErrorCode::LeaseExpired,
                }));
            }
            _ => {}
        }
    }

    fn on_link_down(&mut self, now_ms: u64, peer: NodeId, out: &mut Vec<Output>) {
        if let Some(entry) = self.peers.get_mut(&peer) {
            entry.online = false;
        }
        let peer_is_lease_peer = self.lease_peer() == Some(peer);
        if !peer_is_lease_peer {
            return;
        }
        match &self.lease {
            LeaseSlot::Source(lease) if lease.state != SourceState::Releasing => {
                let lease_id = lease.lease_id;
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                if let Some(lease) = self.lease.source_mut() {
                    lease.state = SourceState::Releasing;
                    lease.channel_gone = true;
                }
                // «Переход на другого receiver разрешается после `LeaveAck` либо после
                // закрытия прежнего канала и консервативного ожидания 600 ms».
                out.push(Output::ArmTimer {
                    timer: Timer::ReleaseGuard,
                    deadline_ms: now_ms.saturating_add(self.timing.claim_grant_timeout_ms),
                });
            }
            LeaseSlot::Receiver(lease) if lease.state != ReceiverState::Releasing => {
                let lease_id = lease.lease_id;
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Releasing;
                    lease.channel_gone = true;
                }
            }
            _ => {}
        }
    }

    fn on_net(&mut self, now_ms: u64, from: NodeId, message: Message, out: &mut Vec<Output>) {
        match message {
            Message::Shutdown { reason } => {
                out.push(Output::Notice(Notice::PeerShutdown { peer: from }));
                let _ = reason;
                self.on_link_down(now_ms, from, out);
            }
            Message::OwnerClaim { serial, reason } => {
                self.on_owner_claim(now_ms, from, serial, reason, out);
            }
            Message::OwnerState {
                serial,
                owner,
                granted,
            } => self.on_owner_state(now_ms, from, serial, owner, granted, out),
            Message::ProtocolError { code, .. } => self.on_protocol_error(now_ms, from, code, out),
            Message::EnterPrepare {
                lease_id,
                topology_revision,
                position,
                owner_serial,
            } => self.on_enter_prepare(
                now_ms,
                Frame { from, lease_id },
                topology_revision,
                position,
                owner_serial,
                out,
            ),
            Message::EnterAck {
                lease_id,
                accepted,
                reason,
            } => self.on_enter_ack(now_ms, from, lease_id, accepted, reason, out),
            Message::EnterCommit { lease_id } => self.on_enter_commit(now_ms, from, lease_id, out),
            Message::LeaseChallenge {
                lease_id,
                nonce,
                ttl_ms,
            } => self.on_lease_challenge(from, lease_id, nonce, ttl_ms, out),
            Message::LeaseStatus {
                lease_id,
                nonce,
                accepted,
                last_input_batch_seq,
            } => self.on_lease_status(
                now_ms,
                Frame { from, lease_id },
                nonce,
                accepted,
                last_input_batch_seq,
                out,
            ),
            Message::InputBatch {
                lease_id,
                input_batch_seq,
                events,
            } => self.on_input_batch(now_ms, from, lease_id, input_batch_seq, events, out),
            Message::InjectAck {
                lease_id,
                input_batch_seq,
                applied_count: _,
                actual_position,
                status,
            } => self.on_inject_ack(
                from,
                lease_id,
                input_batch_seq,
                actual_position,
                status,
                out,
            ),
            Message::Leave { lease_id, reason } => {
                self.on_leave(now_ms, from, lease_id, reason, out);
            }
            Message::LeaveAck { lease_id } => self.on_leave_ack(from, lease_id, out),
            Message::LeaseRenew { lease_id, nonce } => {
                self.on_lease_renew(now_ms, from, lease_id, nonce, out);
            }
        }
    }

    // ---------------------------------------------------------------- receiver

    fn on_enter_prepare(
        &mut self,
        now_ms: u64,
        frame: Frame,
        topology_revision: u64,
        position: Position,
        owner_serial: u64,
        out: &mut Vec<Output>,
    ) {
        let Frame { from, lease_id } = frame;
        let _ = position;
        if !self.lease.is_idle() {
            // «новый lease без освобождения старого отклоняется»
            self.reject_prepare(from, lease_id, ErrorCode::LeaseMismatch, out);
            return;
        }
        if self.burned.contains(&lease_id) {
            self.reject_prepare(from, lease_id, ErrorCode::LeaseMismatch, out);
            return;
        }
        if let Err(code) = self.ownership.accepts_lease_from(owner_serial, &from) {
            self.reject_prepare(from, lease_id, code, out);
            return;
        }
        if topology_revision != self.topology_revision {
            self.reject_prepare(from, lease_id, ErrorCode::TopologyChanged, out);
            return;
        }
        self.lease = LeaseSlot::Receiver(Box::new(ReceiverLease::new(
            lease_id,
            from,
            topology_revision,
            now_ms,
        )));
        out.push(Output::Agent(AgentCommand::PrepareInject { lease_id }));
    }

    fn reject_prepare(
        &mut self,
        from: NodeId,
        lease_id: LeaseId,
        code: ErrorCode,
        out: &mut Vec<Output>,
    ) {
        out.push(Output::Send {
            peer: from,
            message: Message::EnterAck {
                lease_id,
                accepted: false,
                reason: Some(code),
            },
        });
        out.push(Output::Notice(Notice::Rejected { peer: from, code }));
    }

    fn on_enter_commit(
        &mut self,
        now_ms: u64,
        from: NodeId,
        lease_id: LeaseId,
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.receiver() else {
            self.mismatch(from, out);
            return;
        };
        if lease.source != from || lease.lease_id != lease_id {
            self.mismatch(from, out);
            return;
        }
        match lease.state {
            ReceiverState::Prepared => {
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::Agent(AgentCommand::ArmDeadline {
                    lease_id,
                    remaining_ms: self.timing.lease_ttl_ms,
                }));
                out.push(Output::ArmTimer {
                    timer: Timer::ReceiverLeaseDeadline,
                    deadline_ms: self.timing.lease_deadline_ms(now_ms),
                });
                out.push(Output::ArmTimer {
                    timer: Timer::ReceiverChallenge,
                    deadline_ms: now_ms.saturating_add(self.timing.challenge_interval_ms()),
                });
                out.push(Output::ArmTimer {
                    timer: Timer::RepeatGuard,
                    deadline_ms: now_ms.saturating_add(self.timing.repeat_guard_ms),
                });
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Active;
                    lease.deadline_ms = self.timing.lease_deadline_ms(now_ms);
                    lease.note_activity(now_ms);
                }
            }
            ReceiverState::Active => {
                // «Повтор `EnterCommit` с тем же ID и корректной новой `seq` отвечает
                // статусом без сброса watchdog».
                out.push(Output::Send {
                    peer: from,
                    message: Message::EnterAck {
                        lease_id,
                        accepted: true,
                        reason: None,
                    },
                });
            }
            ReceiverState::Releasing | ReceiverState::Expired => {
                // Terminal for this lease id: no revival.
                out.push(Output::Send {
                    peer: from,
                    message: Message::EnterAck {
                        lease_id,
                        accepted: false,
                        reason: Some(ErrorCode::LeaseExpired),
                    },
                });
            }
        }
    }

    fn on_lease_renew(
        &mut self,
        now_ms: u64,
        from: NodeId,
        lease_id: LeaseId,
        nonce: [u8; 16],
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.receiver_mut() else {
            self.mismatch(from, out);
            return;
        };
        if lease.source != from || lease.lease_id != lease_id {
            self.mismatch(from, out);
            return;
        }
        let last_input_batch_seq = lease.last_input_batch_seq;
        match lease.apply_renew(&nonce, self.timing.lease_ttl_ms) {
            Some(deadline) => {
                lease.deadline_ms = deadline;
                lease.note_activity(now_ms);
                out.push(Output::Send {
                    peer: from,
                    message: Message::LeaseStatus {
                        lease_id,
                        nonce,
                        accepted: true,
                        last_input_batch_seq,
                    },
                });
                out.push(Output::ArmTimer {
                    timer: Timer::ReceiverLeaseDeadline,
                    deadline_ms: deadline,
                });
                out.push(Output::ArmTimer {
                    timer: Timer::RepeatGuard,
                    deadline_ms: now_ms.saturating_add(self.timing.repeat_guard_ms),
                });
            }
            None => {
                // Unknown/stale nonce, or the lease is no longer active: the deadline is
                // untouched, so a late renew cannot revive an expired lease.
                out.push(Output::Send {
                    peer: from,
                    message: Message::LeaseStatus {
                        lease_id,
                        nonce,
                        accepted: false,
                        last_input_batch_seq,
                    },
                });
            }
        }
    }

    fn on_input_batch(
        &mut self,
        now_ms: u64,
        from: NodeId,
        lease_id: LeaseId,
        input_batch_seq: u64,
        events: Vec<input_model::InputEvent>,
        out: &mut Vec<Output>,
    ) {
        let reject = |out: &mut Vec<Output>, code: ErrorCode| {
            out.push(Output::Send {
                peer: from,
                message: Message::InjectAck {
                    lease_id,
                    input_batch_seq,
                    applied_count: 0,
                    actual_position: None,
                    status: InjectStatus::Failed,
                },
            });
            out.push(Output::Notice(Notice::Rejected { peer: from, code }));
        };
        let Some(lease) = self.lease.receiver_mut() else {
            self.mismatch(from, out);
            return;
        };
        if lease.source != from || lease.lease_id != lease_id {
            self.mismatch(from, out);
            return;
        }
        if !lease.accepts_input() {
            return reject(out, ErrorCode::LeaseExpired);
        }
        if events.is_empty() || events.len() > 128 {
            return reject(out, ErrorCode::InvalidFrame);
        }
        if input_batch_seq <= lease.last_input_batch_seq {
            return reject(out, ErrorCode::InvalidSequence);
        }
        lease.last_input_batch_seq = input_batch_seq;
        lease.note_activity(now_ms);
        out.push(Output::ArmTimer {
            timer: Timer::RepeatGuard,
            deadline_ms: now_ms.saturating_add(self.timing.repeat_guard_ms),
        });
        out.push(Output::Agent(AgentCommand::Inject {
            lease_id,
            input_batch_seq,
            events,
        }));
    }

    fn on_leave(
        &mut self,
        _now_ms: u64,
        from: NodeId,
        lease_id: LeaseId,
        reason: LeaveReason,
        out: &mut Vec<Output>,
    ) {
        match &self.lease {
            LeaseSlot::Source(lease) if lease.peer == from && lease.lease_id == lease_id => {
                // The receiver ended it (takeover) or it duplicates our own Leave:
                // «Source переходит в Local и отвечает LeaveAck».
                out.push(Output::Agent(AgentCommand::EndCapture {
                    lease_id,
                    replay_buffered_locally: false,
                }));
                out.push(Output::Send {
                    peer: from,
                    message: Message::LeaveAck { lease_id },
                });
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::SourceLeaseWatchdog));
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
                self.lease = LeaseSlot::Idle;
                self.burned.insert(lease_id);
                if reason == LeaveReason::ReceiverLocalInput {
                    out.push(Output::Notice(Notice::Takeover { lease_id }));
                }
            }
            LeaseSlot::Receiver(lease) if lease.source == from && lease.lease_id == lease_id => {
                if lease.state == ReceiverState::Releasing {
                    // We already released (takeover); just acknowledge.
                    out.push(Output::Send {
                        peer: from,
                        message: Message::LeaveAck { lease_id },
                    });
                    self.burned.insert(lease_id);
                    self.lease = LeaseSlot::Idle;
                    return;
                }
                // «RC->>RA: ReleaseSession L; RA-->>RC: Released; RC-->>SC: LeaveAck L»:
                // the ack goes out when the agent confirms the release.
                out.push(Output::Agent(AgentCommand::ReleaseSession { lease_id }));
                out.push(Output::CancelTimer(Timer::PrepareTimeout));
                out.push(Output::CancelTimer(Timer::ReceiverLeaseDeadline));
                out.push(Output::CancelTimer(Timer::ReceiverChallenge));
                out.push(Output::CancelTimer(Timer::RepeatGuard));
                if let Some(lease) = self.lease.receiver_mut() {
                    lease.state = ReceiverState::Releasing;
                    lease.pending_leave = None;
                }
            }
            _ => self.mismatch(from, out),
        }
    }

    fn on_leave_ack(&mut self, from: NodeId, lease_id: LeaseId, out: &mut Vec<Output>) {
        match &self.lease {
            LeaseSlot::Source(lease)
                if lease.peer == from
                    && lease.lease_id == lease_id
                    && lease.state == SourceState::Releasing =>
            {
                self.burned.insert(lease_id);
                self.lease = LeaseSlot::Idle;
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
            }
            LeaseSlot::Receiver(lease) if lease.source == from && lease.lease_id == lease_id => {
                self.burned.insert(lease_id);
                self.lease = LeaseSlot::Idle;
                out.push(Output::CancelTimer(Timer::ReleaseGuard));
            }
            // A `LeaveAck` may arrive after we already released locally: benign.
            LeaseSlot::Idle => {}
            _ => self.mismatch(from, out),
        }
    }

    // ------------------------------------------------------------------- source

    fn on_enter_ack(
        &mut self,
        now_ms: u64,
        from: NodeId,
        lease_id: LeaseId,
        accepted: bool,
        reason: Option<ErrorCode>,
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.source() else {
            self.mismatch(from, out);
            return;
        };
        if lease.peer != from || lease.lease_id != lease_id || lease.state != SourceState::Preparing
        {
            self.mismatch(from, out);
            return;
        }
        if !accepted {
            let code = reason.unwrap_or(ErrorCode::UnsupportedSession);
            out.push(Output::Agent(AgentCommand::EndCapture {
                lease_id,
                replay_buffered_locally: true,
            }));
            out.push(Output::CancelTimer(Timer::PrepareTimeout));
            self.lease = LeaseSlot::Idle;
            self.burned.insert(lease_id);
            out.push(Output::Notice(Notice::Rejected { peer: from, code }));
            return;
        }
        let peer = lease.peer;
        let capture_ready = lease.capture_ready;
        if !capture_ready {
            if let Some(lease) = self.lease.source_mut() {
                lease.enter_ack_received = true;
            }
            return;
        }
        // «SC->>SA: BeginCapture REMOTE L» right after the ack.
        if let Some(lease) = self.lease.source_mut() {
            lease.remote_capture_requested = true;
        }
        out.push(Output::Agent(AgentCommand::BeginCapture {
            kind: CaptureKind::Remote,
            lease_id,
            self_timeout_ms: self.timing.lease_ttl_ms.min(1000),
        }));
        let _ = (now_ms, peer);
    }

    fn on_lease_challenge(
        &mut self,
        from: NodeId,
        lease_id: LeaseId,
        nonce: [u8; 16],
        ttl_ms: u32,
        out: &mut Vec<Output>,
    ) {
        let Some(lease) = self.lease.source_mut() else {
            self.mismatch(from, out);
            return;
        };
        if lease.peer != from || lease.lease_id != lease_id {
            self.mismatch(from, out);
            return;
        }
        let _ = ttl_ms;
        match lease.state {
            SourceState::Preparing => {
                // The first challenge arrives before `EnterCommit`; renew it then.
                let _ = lease.remember_challenge(nonce);
            }
            SourceState::Remote => {
                if lease.remember_challenge(nonce) {
                    out.push(Output::Send {
                        peer: from,
                        message: Message::LeaseRenew { lease_id, nonce },
                    });
                }
            }
            SourceState::Releasing | SourceState::Done => {}
        }
    }

    fn on_lease_status(
        &mut self,
        now_ms: u64,
        frame: Frame,
        nonce: Nonce,
        accepted: bool,
        last_input_batch_seq: u64,
        out: &mut Vec<Output>,
    ) {
        let Frame { from, lease_id } = frame;
        let Some(lease) = self.lease.source_mut() else {
            self.mismatch(from, out);
            return;
        };
        if lease.peer != from || lease.lease_id != lease_id || lease.state != SourceState::Remote {
            return;
        }
        lease.last_applied_batch_seq = last_input_batch_seq;
        if accepted && lease.take_challenge(&nonce) {
            // «Source при отсутствии `LeaseStatus` за `lease_ttl` закрывает канал»: an
            // *applied* status for a challenge we actually answered re-arms the
            // watchdog; a replayed status for an unknown nonce does not.
            out.push(Output::ArmTimer {
                timer: Timer::SourceLeaseWatchdog,
                deadline_ms: self.timing.lease_deadline_ms(now_ms),
            });
        }
    }

    // ------------------------------------------------------------------ helpers

    fn refuse(&mut self, trigger: SwitchTrigger, reason: TransitionRefusal, out: &mut Vec<Output>) {
        out.push(Output::Notice(Notice::SwitchRefused { trigger, reason }));
    }

    fn mismatch(&mut self, peer: NodeId, out: &mut Vec<Output>) {
        out.push(Output::Notice(Notice::LeaseMismatch { peer }));
    }

    fn lease_peer(&self) -> Option<NodeId> {
        match &self.lease {
            LeaseSlot::Idle => None,
            LeaseSlot::Source(lease) => Some(lease.peer),
            LeaseSlot::Receiver(lease) => Some(lease.source),
        }
    }

    fn peer_is_receiver_ready(&self, peer: &NodeId) -> bool {
        self.peers
            .get(peer)
            .and_then(|entry| entry.hello)
            .map(|hello| hello.capabilities.receiver_ready())
            .unwrap_or(false)
    }

    fn emit_focus(&mut self, out: &mut Vec<Output>) {
        let focus = self.focus();
        if focus != self.last_focus {
            self.last_focus = focus;
            out.push(Output::Notice(Notice::Focus(focus)));
        }
    }

    fn emit_ownership(&mut self, out: &mut Vec<Output>) {
        let state = self.ownership.state();
        if state != self.last_ownership {
            self.last_ownership = state;
            out.push(Output::Notice(Notice::Ownership(state)));
        }
    }
}
