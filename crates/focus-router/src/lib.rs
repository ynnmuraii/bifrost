//! Bifrost focus router: the focus/lease FSM and ownership (KVM-022, KVM-032,
//! KVM-071 core).
//!
//! A sans-IO, deterministic, pure crate. It reads no clock (every call takes a
//! monotonic `now_ms`), opens no socket, spawns nothing, and generates no randomness
//! (the `lease_id`/nonce source is injected through [`Identifiers`]). Everything it
//! does is `handle(now_ms, Input) -> Vec<Output>`.
//!
//! # Shape
//!
//! Three machines in one router ([`router::FocusRouter`]):
//!
//! * **ownership** ([`ownership::Ownership`]) — the `(serial, node_id)` pair, claims,
//!   hand-off, self-grant, deposition;
//! * **source lease** ([`lease::SourceLease`]) — this node owns the lease and drives a
//!   receiver (`docs/03-architecture.md`, first `stateDiagram-v2`);
//! * **receiver lease** ([`lease::ReceiverLease`]) — a peer owns the lease and injects
//!   into this node (`docs/05-protocol.md`, second `stateDiagram-v2`).
//!
//! They are composed because the docs relate them (`Ownership` + the *lease role*
//! decide the IPC command: «при уступке или потере владения IPC-команда зависит от
//! роли: source возвращает локальный захват через `EndCapture`, receiver освобождает
//! session-owned состояние через `ReleaseSession`»), and because only the owner may
//! open a lease.
//!
//! # Invariants, and how the types make them hard to break
//!
//! 1. **Never two active leases on one node.** The two lease roles are the two
//!    variants of [`lease::LeaseSlot`] and there is exactly one slot, so there is
//!    nowhere to put a second lease; nothing in the router can create one without
//!    emptying the slot first.
//! 2. **Expiry is irreversible.** A lease that expires (deadline, `prepare_timeout`,
//!    injection failure, disconnect) leaves the slot and its `lease_id` goes into
//!    the bounded 1024-ID `FocusRouter::burned` replay history; prepares for a
//!    recently burned id and commits outside the current prepared lease are refused
//!    and [`lease::ReceiverLease::apply_renew`] returns `None` unless the lease is
//!    still `Active`, so a late `LeaseRenew` cannot revive it — it only draws an
//!    `accepted = false` `LeaseStatus`. The *source* side re-arms its watchdog only
//!    for an applied status of a nonce it actually answered.
//! 3. **A receiver accepts `EnterPrepare` only from the current owner, by
//!    `(serial, node_id)`.** `on_enter_prepare` has exactly one acceptance path and it
//!    goes through [`Ownership::accepts_lease_from`], which compares the pair and
//!    distinguishes `STALE_OWNER_SERIAL` (older serial) from `NOT_OWNER`.
//! 4. **A normal transition needs an empty physical pressed set; emergency always
//!    wins.** `on_switch` refuses to leave `Local` while
//!    [`router::PressedSet`] is non-empty; `on_emergency` is dispatched before any
//!    check and tears down whatever lease exists, in every state.
//! 5. **Release before grant.** A pending ownership change
//!    (`FocusRouter::pending_ownership`) is published only from `settle_ownership`,
//!    whose precondition is `lease.is_idle()`; the field is private and nothing else
//!    emits `OwnerState{granted}`.
//! 6. **Startup claim is `max_seen + 1`.** `Ownership::next_claim_serial` is the only
//!    source of a claim serial (`1` when `max_seen = 0`), and a self-grant is only
//!    published from the `ClaimGrant` timer and only while the claim still wins the
//!    ordering (`claim_still_wins`).
//! 7. **A return edge frees the source only when the receiver has released, and every
//!    release is bounded.** `on_switch` marks the source
//!    `SourceLease::awaiting_leave_ack`, and the slot then leaves it only on the peer's
//!    `LeaveAck` — restoring the local capture, the agent's own
//!    `Released`/`CaptureReady`, is not the peer's release («Переход на другого receiver
//!    разрешается после `LeaveAck`», `docs/05-protocol.md`). Opening a lease earlier is
//!    what a receiver answers with `LEASE_MISMATCH` (S14). The ack can be lost (a link
//!    replaced between the `Leave` and the ack is how CI hit this), so *every* entry into
//!    `Releasing` arms the `ReleaseGuard`: «…либо после закрытия прежнего канала и
//!    консервативного ожидания 600 ms» is the backstop, and it is the only thing that
//!    ends a release whose peer message never arrives. The ownership paths (S25/S26/S30)
//!    release without a `Leave` and keep their documented `Released`/`CaptureReady` wait.
//!
//! # Times, and what is *not* here
//!
//! `prepare_timeout_ms`, `lease_ttl_ms` (with `challenge = min(lease_ttl / 5, 100 ms)`) and
//! `claim_grant_timeout_ms` are the whole configurable timing surface
//! ([`config::Timing`], bounds checked). `repeat_guard_ms` is **not** configurable: it is
//! the protocol limit [`config::Timing::REPEAT_GUARD_MS`] (150 ms). The agent's own
//! `core_silence_ms` watchdog is deliberately not modelled: it belongs to the agent's IPC
//! layer.
//!
//! Resolved doc ambiguities are noted at the code that had to choose (search for
//! "ambiguity" below and in [`router`]).
//!
//! # Seams for the test author
//!
//! * **Pure decisions**: [`wire::order`] (the `(serial, node_id)` ordering),
//!   [`Ownership::accepts_lease_from`], [`Ownership::accepts_claim_from`],
//!   [`Ownership::next_claim_serial`], [`Ownership::is_newer_than_current`],
//!   [`Ownership::may_grant`], [`Timing::challenge_interval_ms`],
//!   [`Timing::lease_deadline_ms`], [`Timing::validate`],
//!   [`ReceiverLease::apply_renew`] (deadline anchored to the *issue* time),
//!   [`SourceLease::remember_challenge`]/[`SourceLease::take_challenge`] (bounded,
//!   one-use), [`config`] bounds.
//! * **Replay**: drive [`FocusRouter`] with `SequentialIds` and a `Vec<Output>` sink;
//!   every scenario below is expressible as `(now_ms, Input)` pairs.
//! * **Wire-number mirror**: `ErrorCode::as_u32`, `LeaveReason::as_u32`,
//!   `ClaimReason::as_u32`, `InjectStatus::as_u32` must match
//!   `docs/protocol.proto` — a cheap, high-value test.
//!
//! # Scenario table (property tests / integration tests)
//!
//! | # | Scenario | Expected |
//! |---|---|---|
//! | S1 | `Local` + switch, empty ledger, owner, capable target | `BeginCapture(Temporary)` + `PrepareTimeout` armed; `Preparing` |
//! | S2 | S1 + capture ready + `EnterAck{accepted}` + capture ready | `BeginCapture(Remote)`, then `EnterCommit` + `LeaseRenew(first nonce)`; `Remote`; watchdog armed |
//! | S3 | `Preparing` + `prepare_timeout` | `EndCapture{replay = true}`, `Local` |
//! | S4 | `Preparing` + `EnterAck{accepted=false, UNSUPPORTED_SESSION}` | `EndCapture{replay = true}`, `Local`, no lease left |
//! | S5 | `Preparing` + agent `CaptureFailed` | `EndCapture{replay = true}`, `Local` |
//! | S6 | switch request while a key is held | refused (`PressedInputHeld`), still `Local` |
//! | S7 | `Remote` + switch (edge/hotkey) | `EndCapture` + `Leave{ReturnEdge/Hotkey}`, `Releasing` |
//! | S8 | `Remote` + `LeaveAck` | `Local`; the old `lease_id` is rejected in the recent replay window |
//! | S9 | `Remote` + no `LeaseStatus` for `lease_ttl` | `EndCapture` + `CloseLink{LEASE_EXPIRED}`, `Local` |
//! | S10 | `Remote` + `InjectAck{FAILED}` | `EndCapture`, `Local` at once (`InjectionAborted`) |
//! | S11 | `Remote` + emergency | `EndCapture` + `Leave{Emergency}`, `Local` immediately |
//! | S12 | `Idle` + valid `EnterPrepare` from the owner | `PrepareInject`, `Prepared`, `EnterAck{accepted}` + first `LeaseChallenge` after the agent is ready |
//! | S13 | `EnterPrepare` from a non-owner / an older serial / a wrong revision | `EnterAck{accepted=false}` with `NOT_OWNER` / `STALE_OWNER_SERIAL` / `TOPOLOGY_CHANGED`; slot stays `Idle` |
//! | S14 | `EnterPrepare` while another lease is held | `EnterAck{accepted=false, LEASE_MISMATCH}` |
//! | S15 | `Prepared` + `EnterCommit` before `prepare_timeout` | `Active`, `ArmDeadline(lease_ttl)`, challenge + repeat-guard timers armed |
//! | S16 | `Prepared` + `prepare_timeout` | `ReleaseSession`, id burned, `LeaseExpired`; a late `EnterCommit` is refused |
//! | S17 | `Active` + valid `LeaseRenew` | deadline = `issue_time + ttl` (not `now + ttl`), `LeaseStatus{accepted}` |
//! | S18 | `Active` + replayed/stale nonce | `LeaseStatus{accepted=false}`, deadline unchanged |
//! | S19 | `Active` + deadline | `ReleaseSession`, `Expired`, `LeaseExpired`; a late renew/renew/commit cannot revive |
//! | S20 | `Active` + `InputBatch` with an older/equal seq, empty events, >128 events | `InjectAck{FAILED}` + `INVALID_SEQUENCE` / `INVALID_FRAME`; nothing injected |
//! | S21 | `Active` + `InputBatch` then `InjectResult{OK}` | `InjectAck{OK}`; `last_input_batch_seq` advanced |
//! | S22 | `Active` + agent `LocalInputDetected` | `ReleaseSession` then `Leave{RECEIVER_LOCAL_INPUT}`; `Releasing` |
//! | S23 | `Active` + `Leave` from the source | `ReleaseSession`; `LeaveAck` only after the agent confirms `Released` |
//! | S24 | `Active` + no input/renew for `repeat_guard_ms` | `RepeatGuardElapsed` once |
//! | S25 | hand-off: owner + higher `OwnerClaim` | `EndCapture`/`ReleaseSession` first, `OwnerState{granted}` only after `Released`/`CaptureReady` |
//! | S26 | owner + higher `OwnerClaim` while a lease is active | the release happens *before* the grant, in that output order |
//! | S27 | claimant + no owner answer within `claim_grant_timeout_ms` | self-grant: `OwnerState{serial, self, granted}` |
//! | S28 | claimant + `OwnerState{granted=false, higher pair}` | claim abandoned, the winner adopted |
//! | S29 | claimant + `ProtocolError{CANNOT_OWN}` / `{STALE_OWNER_SERIAL}` | claim abandoned, no self-grant |
//! | S30 | owner + higher pair via `Hello` gossip or `OwnerState` | deposition: release first, then `Leave{OWNERSHIP_LOST}` to the lease peer |
//! | S31 | `close link` while `Remote` (either role) | release, `Releasing`; the source waits `600 ms` before `Local` |
//! | S32 | `SuspendOrLock` in `Remote` | `EndCapture`/`ReleaseSession` + `Leave{SUSPEND_OR_LOCK}`, `Local` |
//! | S33 | agent lost | release, `Blocked`; `AgentReady` returns to `Local` |
//! | S34 | switch while not the owner / while `Blocked` / with a target that lacks `INJECT` | refused with `NotOwner` / `AgentBlocked` / `ReceiverUnsupported`; nothing sent |
//! | S35 | return edge: the local capture is restored (`Released`/`CaptureReady`) before the peer's `LeaveAck` | the slot stays `Releasing`: a switch now is refused and nothing is sent, and only the `LeaveAck` (or the release guard) frees it |
//! | S36 | a `LeaveAck` that never arrives (a lost frame, a replaced link) | `Local` when the release guard is due — the return edge armed it at the `Leave`, so no release hangs |
//! | S37 | the lease peer's link goes down and is replaced while releasing | the guard is re-armed from the close; the replacement alone does not release (`docs/05-protocol.md`'s conservative wait) |

#![forbid(unsafe_code)]

pub mod config;
pub mod diagnostics;
pub mod flow;
pub mod lease;
pub mod ownership;
pub mod router;
pub mod wire;

pub use config::{ConfigError, Timing};
pub use diagnostics::{
    LeaseCounters, LeaseDiagnosticSnapshot, LeaseInboundAges, LeaseRole, LeaseTraceRing,
    LeaseTraceSnapshot, LeaseTraceStage,
};
pub use flow::{
    AgentCommand, AgentEvent, FocusState, Identifiers, Input, LocalInput, Notice, Output,
    OwnershipState, SequentialIds, SwitchTrigger, Timer, TransitionRefusal,
};
pub use lease::{
    LeaseSlot, ReceiverLease, ReceiverState, SourceLease, SourceState, MAX_CHALLENGES,
};
pub use ownership::{Ownership, PendingClaim, PendingGrant};
pub use router::{FocusRouter, PressedSet};
pub use wire::{
    order, Capabilities, ClaimReason, ErrorCode, InjectStatus, LeaseId, LeaveReason, Message,
    Nonce, PeerHello,
};
