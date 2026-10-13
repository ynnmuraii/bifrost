//! The sans-IO vocabulary: inputs into and outputs out of [`crate::FocusRouter`].
//!
//! Time is never read here: every call takes `now_ms` from a monotonic clock the
//! caller owns, and every deadline is expressed as an absolute `now_ms`-based value.
//! Randomness is never generated either: [`Identifiers`] is the single injected
//! source of `lease_id`/nonce bytes, so a deterministic implementation makes the
//! whole machine replayable (which is what makes KVM-022 property-testable).

use std::fmt;

use geometry::{NodeId, Position};
use input_model::{CaptureKind, HidKey, InputEvent, MouseButton};

use crate::wire::{
    ClaimReason, ErrorCode, InjectStatus, LeaseId, LeaveReason, Message, Nonce, PeerHello,
};

/// Focus state of the local node (`docs/03-architecture.md`, «Фокус и восстановление»
/// and the two `stateDiagram-v2` blocks of `docs/05-protocol.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusState {
    /// Input is local; this node holds no lease.
    Local,
    /// A transition is being negotiated (temporary capture, `EnterPrepare` sent).
    Preparing,
    /// The lease is active: this node is either the source or the receiver.
    Remote,
    /// The lease is being torn down gracefully (waiting for `LeaveAck`).
    Releasing,
    /// The input agent is gone (`AgentLost`); input is local but no transition may
    /// start until the agent is back.
    Blocked,
}

impl FocusState {
    /// The state name used in logs and in the GUI status.
    pub const fn as_str(self) -> &'static str {
        match self {
            FocusState::Local => "Local",
            FocusState::Preparing => "Preparing",
            FocusState::Remote => "Remote",
            FocusState::Releasing => "Releasing",
            FocusState::Blocked => "Blocked",
        }
    }
}

impl fmt::Display for FocusState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ownership state of the local node (`docs/05-protocol.md`, ownership diagram).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnershipState {
    /// A member of the workspace that is not the owner.
    Follower,
    /// A claim is in flight, waiting for the current owner to hand off (or for the
    /// claim timeout, after which we self-grant).
    Claiming,
    /// The only node allowed to open leases and author the layout.
    Owner,
    /// A higher `(serial, node_id)` was observed; the local leases are being released
    /// and `Leave{OWNERSHIP_LOST}` is being announced before returning to `Follower`.
    Deposed,
}

impl OwnershipState {
    /// The state name used in logs and in the GUI status.
    pub const fn as_str(self) -> &'static str {
        match self {
            OwnershipState::Follower => "Follower",
            OwnershipState::Claiming => "Claiming",
            OwnershipState::Owner => "Owner",
            OwnershipState::Deposed => "Deposed",
        }
    }
}

impl fmt::Display for OwnershipState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which physical trigger asked for a switch. It means "enter" while local and
/// "return" while remote (`docs/03-architecture.md`: «return edge, emergency, …»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwitchTrigger {
    /// The pointer reached a link edge.
    Edge,
    /// `input.switch_hotkey` (not available on a Wayland source).
    Hotkey,
}

impl SwitchTrigger {
    /// The `LeaveReason` this trigger produces when it ends a lease.
    pub const fn leave_reason(self) -> LeaveReason {
        match self {
            SwitchTrigger::Edge => LeaveReason::ReturnEdge,
            SwitchTrigger::Hotkey => LeaveReason::Hotkey,
        }
    }
}

/// A local command or a locally detected trigger.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalInput {
    /// An edge or hotkey asks to switch. The caller resolves the target member and
    /// the entry position with `geometry::transition` (edge → link → target endpoint,
    /// entry inset already applied), because this crate must not duplicate the
    /// geometry model.
    SwitchRequest {
        /// Which trigger fired.
        trigger: SwitchTrigger,
        /// The member whose display will receive the input.
        target: NodeId,
        /// Entry position on the target display (display-local logical units).
        entry_position: Position,
        /// The applied topology revision the caller believes in.
        topology_revision: u64,
    },
    /// The emergency combination fired locally. Always wins, in every state.
    Emergency,
    /// The user (GUI or claim hotkey) asks for ownership.
    Claim {
        /// Why the claim is being made.
        reason: ClaimReason,
    },
    /// The user abandons ownership (GUI).
    Abandon,
    /// logind reported suspend or lock; the agent already released the capture.
    SuspendOrLock,
    /// logind reported resume or unlock.
    Resume,
    /// The applied topology revision changed.
    TopologyChanged {
        /// The revision that is now applied.
        revision: u64,
    },
    /// The local display set changed (`DisplaysChanged`).
    DisplaysChanged,
}

/// A local agent (input-agent IPC) event.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The agent answered `BeginCapture`.
    CaptureReady {
        /// The lease the capture belongs to.
        lease_id: LeaseId,
    },
    /// The agent answered `PrepareInject` (`Ready` in the sequence diagram).
    Ready {
        /// The lease the agent prepared injection for.
        lease_id: LeaseId,
    },
    /// The agent restarted and is ready in `NORMAL` mode (leaves `Blocked`).
    AgentReady,
    /// The agent released the capture and its session-owned state.
    Released {
        /// The released lease.
        lease_id: LeaseId,
    },
    /// The outcome of one injected batch.
    InjectResult {
        /// The lease that was injected.
        lease_id: LeaseId,
        /// Echo of the batch sequence.
        input_batch_seq: u64,
        /// Applied event count (`applied_count` in `ipc.proto`).
        applied_count: u32,
        /// Where the local cursor ended up, when the agent knows.
        actual_position: Option<Position>,
        /// Outcome; `Partial`/`Failed` abort the lease.
        status: InjectStatus,
    },
    /// The receiver's agent saw physical (not injected) input during an active lease.
    LocalInputDetected {
        /// The lease that must end.
        lease_id: LeaseId,
    },
    /// The agent's deadline watchdog expired for this lease.
    DeadlineExpired {
        /// The expired lease.
        lease_id: LeaseId,
    },
    /// A conservative snapshot of the agent's pressed ledger.
    ///
    /// Core keeps this set to refuse a normal transition while anything is held
    /// («Нормальный переход только с пустым physical pressed set»); the emergency path
    /// ignores it. It is also what `ReleaseKeys` is built from after an agent restart.
    PressedState {
        /// Keys currently held.
        keys: Vec<HidKey>,
        /// Buttons currently held.
        buttons: Vec<MouseButton>,
    },
    /// The agent is gone and will be restarted in `RECOVERY` mode.
    AgentLost,
    /// The agent reported an error for a lease (or globally, with `lease_id = None`).
    Error {
        /// The lease the error belongs to, when it is lease-scoped.
        lease_id: Option<LeaseId>,
        /// The error code.
        code: ErrorCode,
    },
}

/// Everything that can happen to the router.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A network message arrived on the link to `from`.
    Net {
        /// The peer that sent it.
        from: NodeId,
        /// The message.
        message: Message,
    },
    /// The `Hello` gossip of a peer (its `can_own` and its last known owner).
    Hello {
        /// The peer.
        peer: NodeId,
        /// Its gossip fields.
        hello: PeerHello,
    },
    /// A link went down. The router releases anything scoped to that peer.
    LinkDown {
        /// The peer whose link is gone.
        peer: NodeId,
    },
    /// A local agent event.
    Agent(AgentEvent),
    /// A local command or detected trigger.
    Local(LocalInput),
    /// A timer armed by the router expired.
    TimerExpired(Timer),
}

/// Timers the router asks the caller to arm. At most one of each kind is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Timer {
    /// Allocation-free lease report, every 1000 ms while Remote.
    LeaseDiagnostic,
    /// `prepare_timeout_ms` for the temporary source capture / the receiver's
    /// `Prepared` lease.
    PrepareTimeout,
    /// Source side: no `LeaseStatus` for `lease_ttl_ms` → restore local control.
    SourceLeaseWatchdog,
    /// Receiver side: the active lease deadline.
    ReceiverLeaseDeadline,
    /// Receiver side: issue the next `LeaseChallenge`.
    ReceiverChallenge,
    /// Receiver side: stop autorepeat after `repeat_guard_ms` without input or renew.
    RepeatGuard,
    /// The claimant's `claim_grant_timeout_ms` before self-granting.
    ClaimGrant,
    /// Source side: conservative wait after the peer's channel closed.
    ReleaseGuard,
}

/// Why a switch request was refused locally.
///
/// These refusals never become a wire message: a transition that cannot start is
/// never started, so the peer sees nothing (`docs/05-protocol.md`: the capability
/// check happens *before* `EnterPrepare`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionRefusal {
    /// Only the owner opens leases («Начать переход может только текущий owner»).
    NotOwner,
    /// Something is physically held: «Нормальный переход только с пустым physical
    /// pressed set»; «Переход при удерживаемой клавише или кнопке откладывается».
    PressedInputHeld,
    /// This node cannot offer an emergency hotkey while
    /// `input.require_emergency_hotkey = true` (AC-03).
    SourceCapabilityMissing,
    /// The target did not declare the receiver capabilities (`INJECT` +
    /// `ABSOLUTE_POINTER`).
    ReceiverUnsupported,
    /// The input agent is gone (`Blocked`); no transition may start.
    AgentBlocked,
}

impl TransitionRefusal {
    /// The name used in logs and GUI text.
    pub const fn as_str(self) -> &'static str {
        match self {
            TransitionRefusal::NotOwner => "not the owner",
            TransitionRefusal::PressedInputHeld => "physical input is held",
            TransitionRefusal::SourceCapabilityMissing => "no emergency hotkey capability",
            TransitionRefusal::ReceiverUnsupported => "target cannot receive absolute input",
            TransitionRefusal::AgentBlocked => "input agent is unavailable",
        }
    }
}

/// Something worth surfacing to the GUI/status, without protocol meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// The focus state changed.
    Focus(FocusState),
    /// The ownership state changed.
    Ownership(OwnershipState),
    /// A local switch request was refused, and why.
    SwitchRefused {
        /// The trigger that was refused.
        trigger: SwitchTrigger,
        /// The reason.
        reason: TransitionRefusal,
    },
    /// A message was refused with `code`.
    Rejected {
        /// The peer whose message was refused.
        peer: NodeId,
        /// The refusal.
        code: ErrorCode,
    },
    /// A lease deadline expired; the id is burned.
    LeaseExpired {
        /// The expired lease.
        lease_id: LeaseId,
    },
    /// This node was deposed by a higher `(serial, node_id)`.
    OwnershipLost {
        /// The winning serial.
        serial: u64,
        /// The winning owner.
        owner: NodeId,
    },
    /// A claim of ours was refused or lost the ordering.
    ClaimRefused {
        /// The refusal, when a peer sent one.
        code: Option<ErrorCode>,
    },
    /// The receiver gave up on a lease (a partial/failed injection) and the source
    /// restored local control at once.
    InjectionAborted {
        /// The aborted lease.
        lease_id: LeaseId,
        /// The code the receiver reported.
        code: ErrorCode,
    },
    /// The receiver gave up on a lease locally (injection failure or the deadline).
    ReceiverAborted {
        /// The aborted lease.
        lease_id: LeaseId,
        /// The code to surface.
        code: ErrorCode,
    },
    /// The receiver's repeat guard elapsed: the receiver must stop autorepeat
    /// (AC-14).
    RepeatGuardElapsed {
        /// The lease whose repeat must stop.
        lease_id: LeaseId,
    },
    /// The receiver saw physical input and is taking the machine back.
    Takeover {
        /// The lease that is ending.
        lease_id: LeaseId,
    },
    /// A lease message arrived for a lease we do not hold (or hold as the wrong role).
    LeaseMismatch {
        /// The peer that sent it.
        peer: NodeId,
        /// The lease id the message carried.
        received: LeaseId,
        /// The lease id this node holds, when it holds one (it is idle otherwise).
        expected: Option<LeaseId>,
    },
    /// The peer took its link down deliberately (`Shutdown`).
    PeerShutdown {
        /// The peer.
        peer: NodeId,
    },
}

/// Commands the router sends to the local input agent (mirrors `docs/ipc.proto`).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    /// `BeginCapture`.
    BeginCapture {
        /// `Observe`, `Temporary` or `Remote`.
        kind: CaptureKind,
        /// The lease this capture belongs to.
        lease_id: LeaseId,
        /// The agent ends the capture itself after this much time (`<= 1000`).
        self_timeout_ms: u64,
    },
    /// `EndCapture`.
    EndCapture {
        /// The lease being ended.
        lease_id: LeaseId,
        /// Whether the agent replays the buffered events locally (a cancelled
        /// transition must not lose a press).
        replay_buffered_locally: bool,
    },
    /// `PrepareInject`.
    PrepareInject {
        /// The lease to prepare injection for.
        lease_id: LeaseId,
    },
    /// `Inject`.
    Inject {
        /// The lease to inject for.
        lease_id: LeaseId,
        /// The batch sequence, echoed back in `InjectResult`.
        input_batch_seq: u64,
        /// The events.
        events: Vec<InputEvent>,
    },
    /// `ArmDeadline`, with the remaining time computed from the lease deadline.
    ArmDeadline {
        /// The lease whose deadline is being armed.
        lease_id: LeaseId,
        /// Milliseconds left, `1..=1000`.
        remaining_ms: u64,
    },
    /// `ReleaseSession`: release everything this lease injected.
    ReleaseSession {
        /// The lease to release.
        lease_id: LeaseId,
    },
}

/// An output of the router.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Per-lease FSM counts and inbound ages; final reports precede teardown.
    LeaseDiagnostic(crate::diagnostics::LeaseDiagnosticSnapshot),
    /// Send a message to one peer.
    Send {
        /// The peer.
        peer: NodeId,
        /// The message.
        message: Message,
    },
    /// Send the same message to every member (`OwnerClaim`/`OwnerState` go to all
    /// links).
    Broadcast(Message),
    /// Send a command to the local input agent.
    Agent(AgentCommand),
    /// Arm `timer` to fire at `deadline_ms`.
    ArmTimer {
        /// Which timer.
        timer: Timer,
        /// Absolute monotonic deadline.
        deadline_ms: u64,
    },
    /// Cancel a previously armed timer (idempotent).
    CancelTimer(Timer),
    /// Close the link to `peer`. The transport reconnects in `Local`; input queues
    /// are never replayed.
    CloseLink {
        /// The peer.
        peer: NodeId,
        /// The reason, for the log.
        code: ErrorCode,
    },
    /// Informational; never a protocol action.
    Notice(Notice),
}

/// Supplies the random identifiers of protocol v0.3.
///
/// This is the only injected effect besides time. Production plugs a CSPRNG; tests
/// and replays plug [`SequentialIds`]. Neither `lease_id` nor a nonce may ever repeat
/// (`docs/05-protocol.md`: «`lease_id` — random bytes16, never reused», «Nonce
/// одноразовый»).
pub trait Identifiers {
    /// A fresh `lease_id`.
    fn lease_id(&mut self) -> LeaseId;
    /// A fresh challenge nonce.
    fn nonce(&mut self) -> Nonce;
}

/// Deterministic [`Identifiers`]: a counter in the first 8 bytes, zero elsewhere.
///
/// For tests and for replaying a recorded session; not for production (it is not
/// random and repeats after `2^64` calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequentialIds {
    counter: u64,
}

impl SequentialIds {
    /// Starts the counter at `seed`.
    pub const fn new(seed: u64) -> Self {
        Self { counter: seed }
    }

    fn next_bytes(&mut self) -> [u8; 16] {
        let value = self.counter;
        self.counter = self.counter.wrapping_add(1);
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&value.to_be_bytes());
        bytes
    }
}

impl Default for SequentialIds {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Identifiers for SequentialIds {
    fn lease_id(&mut self) -> LeaseId {
        self.next_bytes()
    }

    fn nonce(&mut self) -> Nonce {
        self.next_bytes()
    }
}
