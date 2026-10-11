//! Mesh and liveness policy: the pure decisions of a link manager.
//!
//! Every function here is deterministic and takes only its inputs, so the rules of
//! `docs/05-protocol.md` («одновременный dial», «`Ping` каждую 1 s; после 3 s без `Pong` пир
//! считается офлайн») and `docs/06-development-plan.md` (KVM-070: mesh, dedup, membership)
//! can be read and checked without a socket.

use std::time::Duration;

use geometry::NodeId;

/// `Ping` period: «`Ping` каждую 1 s» (`docs/06-development-plan.md`, KVM-070).
pub const PING_INTERVAL: Duration = Duration::from_secs(1);

/// A peer without a `Pong` for this long is offline: «офлайн через 3 s без `Pong`».
pub const OFFLINE_AFTER: Duration = Duration::from_secs(3);

/// Backoff bounds from the transport diagram: «backoff 0.5..5 s, all configured peers».
pub const BACKOFF_MIN_MS: u64 = 500;
/// See [`BACKOFF_MIN_MS`].
pub const BACKOFF_MAX_MS: u64 = 5_000;

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
/// stays inside `0.5..=5 s`, per the transport diagram.
pub fn reconnect_delay(attempt: u32) -> Duration {
    let doubled = BACKOFF_MIN_MS.saturating_mul(1u64 << attempt.min(4));
    Duration::from_millis(doubled.clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS))
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
/// when we last sent a `Ping` (`None` before the first one).
pub fn liveness_action(
    now_ms: u64,
    last_pong_ms: u64,
    last_ping_ms: Option<u64>,
) -> LivenessAction {
    let since_pong = now_ms.saturating_sub(last_pong_ms);
    if since_pong >= OFFLINE_AFTER.as_millis() as u64 {
        return LivenessAction::Offline;
    }
    match last_ping_ms {
        Some(last) if now_ms.saturating_sub(last) < PING_INTERVAL.as_millis() as u64 => {
            LivenessAction::Idle
        }
        _ => LivenessAction::Ping,
    }
}

/// How long to sleep before the next liveness check, so the caller can drive the state
/// machine with a single `sleep` instead of a busy loop.
pub fn next_liveness_wakeup(now_ms: u64, last_pong_ms: u64, last_ping_ms: Option<u64>) -> u64 {
    let ping_due = match last_ping_ms {
        Some(last) => last.saturating_add(PING_INTERVAL.as_millis() as u64),
        None => now_ms,
    };
    let offline_due = last_pong_ms.saturating_add(OFFLINE_AFTER.as_millis() as u64);
    ping_due.min(offline_due).max(now_ms)
}
