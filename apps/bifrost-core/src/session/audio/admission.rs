//! Per-trusted-identity admission, deliberately independent of parent/child generations.
use geometry::NodeId;
use std::collections::{btree_map::Entry, BTreeMap};

const MAX_IDENTITIES: usize = 64;
const BURST: u64 = 12;
const REFILL_MS: u64 = 5_000;
const MIN_START_MS: u64 = 1_000;

struct Budget {
    tokens: u64,
    refilled_at: u64,
    last_start: Option<u64>,
    last_refusal: Option<(u64, [u8; 16])>,
}
impl Budget {
    fn available(&mut self, now: u64) -> bool {
        let Some(elapsed) = now.checked_sub(self.refilled_at) else {
            return false;
        };
        let replenished = elapsed / REFILL_MS;
        if replenished != 0 {
            self.tokens = self.tokens.saturating_add(replenished).min(BURST);
            self.refilled_at = now - elapsed % REFILL_MS;
        }
        self.tokens != 0
            && self.last_start.is_none_or(|last| {
                now.checked_sub(last)
                    .is_some_and(|elapsed| elapsed >= MIN_START_MS)
            })
    }
}

#[derive(Default)]
pub(super) struct Starts {
    identities: BTreeMap<NodeId, Budget>,
}
impl Starts {
    fn budget(&mut self, peer: NodeId, now: u64) -> Option<&mut Budget> {
        let room = self.identities.len() < MAX_IDENTITIES;
        match self.identities.entry(peer) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(entry) if room => Some(entry.insert(Budget {
                tokens: BURST,
                refilled_at: now,
                last_start: None,
                last_refusal: None,
            })),
            Entry::Vacant(_) => None,
        }
    }
    /// Twelve initial starts, one replenished per five seconds, at most one/second.
    /// Call only for a currently trusted identity; reconnecting cannot reset this state.
    pub(super) fn available(&mut self, peer: NodeId, now: u64) -> bool {
        self.budget(peer, now)
            .is_some_and(|budget| budget.available(now))
    }
    pub(super) fn admit(&mut self, peer: NodeId, now: u64) -> bool {
        let Some(budget) = self.budget(peer, now) else {
            return false;
        };
        if !budget.available(now) {
            return false;
        }
        budget.tokens -= 1;
        budget.last_start = Some(now);
        true
    }
    /// A denied request gets at most one typed refusal/second; duplicates are silent.
    pub(super) fn refuse(&mut self, peer: NodeId, request: [u8; 16], now: u64) -> bool {
        let Some(budget) = self.budget(peer, now) else {
            return false;
        };
        if budget.last_refusal.is_some_and(|(at, previous)| {
            previous == request
                || now
                    .checked_sub(at)
                    .is_none_or(|elapsed| elapsed < MIN_START_MS)
        }) {
            return false;
        }
        budget.last_refusal = Some((now, request));
        true
    }
    /// Drop only revoked identities, never merely disconnected parents.
    pub(super) fn retain(&mut self, mut trusted: impl FnMut(&NodeId) -> bool) {
        self.identities.retain(|peer, _| trusted(peer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PEER: NodeId = NodeId::new([1; 32]);

    #[test]
    fn reconnects_and_child_generations_cannot_reset_identity_budget() {
        let mut starts = Starts::default();
        for now in 0..14 {
            assert!(starts.admit(PEER, now * 1_000));
            starts.retain(|_| true); // Parent replacements retain the trusted identity.
        }
        assert!(!starts.available(PEER, 14_000));
        assert!(!starts.admit(PEER, 14_000));
        assert!(starts.admit(NodeId::new([2; 32]), 14_000));
        assert!(starts.admit(PEER, 15_000));
        assert!(!starts.admit(PEER, 15_001));
        assert!(!starts.admit(PEER, 1));
        starts.retain(|_| false);
        assert!(starts.admit(PEER, 16_000)); // A new explicit trust grant is a new authority.
    }

    #[test]
    fn refusal_deduplication_and_rate_are_bounded_without_native_work() {
        let mut starts = Starts::default();
        assert!(starts.refuse(PEER, [1; 16], 0));
        assert!(!starts.refuse(PEER, [1; 16], 1_000));
        assert!(!starts.refuse(PEER, [2; 16], 999));
        assert!(starts.refuse(PEER, [2; 16], 1_000));
        assert!(!starts.refuse(PEER, [3; 16], 1));
    }

    #[test]
    fn identity_cache_is_bounded_and_revocation_releases_capacity() {
        let mut starts = Starts::default();
        for id in 0..MAX_IDENTITIES {
            let mut bytes = [0; 32];
            bytes[..8].copy_from_slice(&(id as u64).to_be_bytes());
            assert!(starts.admit(NodeId::new(bytes), 0));
        }
        let extra = NodeId::new([255; 32]);
        assert!(!starts.available(extra, 0));
        starts.retain(|peer| *peer == PEER);
        assert!(starts.admit(extra, 0));
    }
}
