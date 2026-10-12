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
/// dialer. `existing_initiator` and `incoming_initiator` are the `initiator_node_id`s of
/// the two links (one of them is always us or the peer).
///
/// This decides **simultaneous formation only** (`docs/05-protocol.md`, «одновременный
/// взаимный дозвон разрешается детерминированно»): two dials that were in flight at the
/// same time, so both sides must pick the same connection without negotiating. It is never
/// the authority for replacing an established link — see [`formation_overlap`], which is
/// what scopes this rule to the race it exists for.
pub fn prefer_incoming(existing_initiator: &NodeId, incoming_initiator: &NodeId) -> bool {
    // The winner is the larger of the two initiators; ties cannot happen for two distinct
    // links to the same peer (one is us, the other is the peer).
    incoming_initiator > existing_initiator
}

/// Whether a link reporting `Up` was still forming when the live link came up.
///
/// `existing_up_at_ns` is when the live link finished forming, `incoming_registered_at_ns`
/// when the newcomer was spawned — both on the manager's monotonic nanosecond clock. `true`
/// means the newcomer was registered *before* the live link was up, so the two dials
/// overlapped in time: the simultaneous-dial race, where [`prefer_incoming`] decides
/// deterministically. Nanosecond precision avoids treating a redial in the same millisecond
/// as a formation race.
///
/// `false` means the newcomer dialled after the live link was already established. That is a
/// late redial — a staggered start, a reconnect that overlapped the old link's last moments,
/// or a dial loop that outlived its link — and it must be refused, not deduplicated: a redial
/// that arrives while a healthy link exists never replaces it, because the replacement tears
/// down the session the established link is carrying. `docs/05-protocol.md` scopes the
/// dedup rule to «одновременный» dialling for exactly this reason.
pub fn formation_overlap(existing_up_at_ns: u128, incoming_registered_at_ns: u128) -> bool {
    incoming_registered_at_ns <= existing_up_at_ns
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The simultaneous-dial rule, pinned as the pure decision it is: the link initiated by
    /// the larger `node_id` wins, on both ends, with no negotiation. The tie goes to the
    /// existing link, which is the two-dials-from-one-node case (`a peer that dials twice`).
    #[test]
    fn the_larger_initiator_wins_a_simultaneous_dial() {
        let smaller = NodeId::new([7u8; 32]);
        let larger = NodeId::new([9u8; 32]);
        assert_eq!(winning_dialer(&smaller, &larger), larger);
        assert_eq!(winning_dialer(&larger, &smaller), larger);
        assert!(prefer_incoming(&smaller, &larger));
        assert!(!prefer_incoming(&larger, &smaller));
        // Two dials from the same node tie, and the existing link keeps its place.
        assert!(!prefer_incoming(&larger, &larger));
    }

    /// The dedup decision is scoped to dials that overlapped in time.
    ///
    /// A newcomer registered while the live link was still forming is the simultaneous race
    /// [`prefer_incoming`] exists for; a newcomer registered after the live link was already
    /// up is a late redial and never decides anything — it is refused. The boundary is the
    /// instant the live link came up, inclusive.
    #[test]
    fn only_a_formation_race_may_decide_the_dedup() {
        // Newcomer registered one nanosecond before the live link came up: the dials
        // overlapped, so the winner is decided by the initiators.
        assert!(formation_overlap(1_000, 999));
        // Registered at the same instant.
        assert!(formation_overlap(1_000, 1_000));
        // Registered after: the live link was formed, so the newcomer is late.
        assert!(!formation_overlap(1_000, 1_001));
    }

    /// The documented deadline, pinned where no scheduler can move it.
    ///
    /// «офлайн через 3 s без `Pong`» is a statement about the link's *own* monotonic clock, and
    /// this is that statement as an assertion: one millisecond before the window is out the
    /// link is still alive (and pinging), at the window it is offline. The integration test in
    /// `tests/mesh.rs` measures the same thing through a real socket, so it can only bound the
    /// *report* generously — the product's deadline is exact here.
    #[test]
    fn the_offline_action_flips_exactly_at_the_documented_window() {
        let liveness = LinkLiveness::default();
        assert_eq!(liveness.ping_ms, 1_000, "documented ping cadence");
        assert_eq!(liveness.offline_ms, 3_000, "documented offline window");

        // A `Pong` a moment ago: alive, and no ping is due yet.
        assert_eq!(
            liveness_action(2_999, 0, Some(2_999), liveness),
            LivenessAction::Idle
        );
        // The same instant, but the last ping is a full period old: the link wants to ping.
        assert_eq!(
            liveness_action(2_999, 0, Some(1_000), liveness),
            LivenessAction::Ping
        );
        // The window is out: offline, whatever the ping bookkeeping says.
        assert_eq!(
            liveness_action(3_000, 0, Some(2_999), liveness),
            LivenessAction::Offline
        );
        assert_eq!(
            liveness_action(9_999, 0, None, liveness),
            LivenessAction::Offline
        );
    }

    /// The wakeup lands on the earlier of the next ping and the offline deadline, and never in
    /// the past: the caller sleeps for `wakeup - now`, and a past instant would mean a busy
    /// loop or a check that never happens.
    #[test]
    fn the_wakeup_is_the_earlier_of_the_next_ping_and_the_deadline() {
        let liveness = LinkLiveness::default();

        // The next ping comes first.
        assert_eq!(next_liveness_wakeup(1_000, 0, Some(1_000), liveness), 2_000);
        // Both are due at the same instant.
        assert_eq!(next_liveness_wakeup(2_900, 0, Some(2_000), liveness), 3_000);
        // The deadline has passed: the caller is told to check now, not in the past.
        assert_eq!(next_liveness_wakeup(3_500, 0, Some(3_000), liveness), 3_500);
        // No ping sent yet: ping now (the first `Ping` is not delayed by a period).
        assert_eq!(next_liveness_wakeup(500, 0, None, liveness), 500);
    }
}
