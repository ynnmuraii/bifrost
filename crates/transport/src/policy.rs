//! Mesh and liveness policy: the pure decisions of a link manager.
//!
//! Every function here is deterministic and takes only its inputs, so the rules of
//! `docs/05-protocol.md` («одновременный dial», «`Ping` каждую 1 s; после 3 s без `Pong` пир
//! считается офлайн») and `docs/06-development-plan.md` (KVM-070: mesh, dedup, membership)
//! can be read and checked without a socket.

use std::time::Duration;

use geometry::NodeId;

/// Liveness cadence of one link: how often this node sends a `Ping` while the link is idle,
/// and how long a silent peer may go without a `Pong` before it is offline.
///
/// The transport is not the owner of these numbers: `crates/config` carries them as
/// `workspace.link_ping_ms` / `workspace.link_timeout_ms` and validates them
/// (`link_timeout_ms > link_ping_ms >= 1`), and the application passes them in. The
/// [`Default`] is what a caller without a configuration gets (the tests, the demo) and
/// holds the documented `1 s` / `3 s` of `docs/06-development-plan.md`, KVM-070.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkLiveness {
    /// `Ping` period in milliseconds: «`Ping` каждую 1 s по каждому линку».
    pub ping_ms: u64,
    /// No `Pong` within this many milliseconds marks the peer offline: «офлайн через 3 s
    /// без `Pong`».
    pub offline_ms: u64,
}

impl Default for LinkLiveness {
    fn default() -> Self {
        Self {
            ping_ms: 1_000,
            offline_ms: 3_000,
        }
    }
}

/// Reconnect backoff bounds in milliseconds: `network.reconnect_backoff_ms`.
///
/// `crates/config` owns the documented range («backoff 0.5..5 s») and refuses a pair outside
/// it; the transport only uses the two numbers it is handed. [`Default`] is the documented
/// `[500, 5000]` for a caller without a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffBounds {
    /// The first retry delay.
    pub min_ms: u64,
    /// The ceiling every later retry is clamped to.
    pub max_ms: u64,
}

impl Default for BackoffBounds {
    fn default() -> Self {
        Self {
            min_ms: 500,
            max_ms: 5_000,
        }
    }
}

/// Who wins when both peers dialled each other at the same time.
///
/// `docs/03-architecture.md`, ERD `LINK.initiator_node_id`: «при одновременном дозвоне
/// побеждает линк от большего `node_id`». Both sides compute the same answer from the two
/// identities alone, so no negotiation is needed.
pub fn winning_dialer(a: &NodeId, b: &NodeId) -> NodeId {
    if a >= b {
        *a
    } else {
        *b
    }
}

/// Whether a newly accepted/dialled link should replace the existing one to `peer`.
///
/// Returns `true` when the *incoming* link is the one whose initiator is the winning
/// dialer. `existing_initiator` and `incoming_initiator` are the `initiator_node_id`s of the
/// two links (one of them is always us or the peer).
pub fn prefer_incoming(existing_initiator: &NodeId, incoming_initiator: &NodeId) -> bool {
    // The winner is the larger of the two initiators; ties cannot happen for two distinct
    // links to the same peer (one is us, the other is the peer).
    incoming_initiator > existing_initiator
}

/// Membership gate: `docs/05-protocol.md`: «Достигнут предел членов (`N_max = 4`,
/// `max_members ≤ 4`): линк пятого и последующих отклонён».
///
/// `current` counts members already admitted (excluding local).
pub fn admit_member(current: usize, max_members: u8) -> Result<(), MembershipFull> {
    let limit = usize::from(max_members.min(protocol::limits::N_MAX));
    if current >= limit {
        return Err(MembershipFull {
            current,
            max_members: limit,
        });
    }
    Ok(())
}

/// `ERROR_CODE_MEMBERSHIP_FULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("membership is full: {current} members already admitted, max_members = {max_members}")]
pub struct MembershipFull {
    /// Members already admitted.
    pub current: usize,
    /// The configured limit.
    pub max_members: usize,
}

/// Exponential backoff schedule for reconnecting to an unreachable peer.
///
/// `attempt` is the number of failed attempts so far (0 for the first retry). The result
/// stays inside the bounds `crates/config` validated.
pub fn reconnect_delay(attempt: u32, bounds: BackoffBounds) -> Duration {
    // `crates/config` rejects `min > max`; normalising here only keeps a hand-built
    // `BackoffBounds` from making `clamp` panic.
    let low = bounds.min_ms.min(bounds.max_ms);
    let high = bounds.max_ms.max(bounds.min_ms);
    let doubled = low.saturating_mul(1u64 << attempt.min(4));
    Duration::from_millis(doubled.clamp(low, high))
}

/// What a link's liveness state machine wants next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessAction {
    /// Send a `Ping` now (the period elapsed).
    Ping,
    /// The peer is offline: close the link.
    Offline,
    /// Nothing to do.
    Idle,
}

/// Decides the next liveness action from monotonic milliseconds.
///
/// `last_pong_ms` is when the peer last answered (or when the link came up), `last_ping_ms`
/// when we last sent a `Ping` (`None` before the first one). `liveness` is the configured
/// cadence, so nothing here hard-codes a period.
pub fn liveness_action(
    now_ms: u64,
    last_pong_ms: u64,
    last_ping_ms: Option<u64>,
    liveness: LinkLiveness,
) -> LivenessAction {
    let since_pong = now_ms.saturating_sub(last_pong_ms);
    if since_pong >= liveness.offline_ms {
        return LivenessAction::Offline;
    }
    match last_ping_ms {
        Some(last) if now_ms.saturating_sub(last) < liveness.ping_ms => LivenessAction::Idle,
        _ => LivenessAction::Ping,
    }
}

/// How long to sleep before the next liveness check, so the caller can drive the state
/// machine with a single `sleep` instead of a busy loop.
pub fn next_liveness_wakeup(
    now_ms: u64,
    last_pong_ms: u64,
    last_ping_ms: Option<u64>,
    liveness: LinkLiveness,
) -> u64 {
    let ping_due = match last_ping_ms {
        Some(last) => last.saturating_add(liveness.ping_ms),
        None => now_ms,
    };
    let offline_due = last_pong_ms.saturating_add(liveness.offline_ms);
    ping_due.min(offline_due).max(now_ms)
}
