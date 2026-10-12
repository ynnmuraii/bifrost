//! Echo suppression: what keeps a fan-out from turning into a loop.
//!
//! `docs/05-protocol.md`: «Эхо подавляется по origin node и version **на каждом пире**: offer
//! с чужим `origin_node_id` и известной `origin_version` не принимается ни по одному линку,
//! а полученный payload никогда не становится новым локальным offer, так что fan-out не
//! превращается в петлю.» The second half is a property of the source side (see
//! [`crate::source`]); this module is the first half, and it is deliberately per-origin: a
//! node keeps the highest version it has seen *from each origin*, because two nodes can copy
//! independently and their version counters are their own.

use std::collections::BTreeMap;

/// What an incoming offer is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoDecision {
    /// A version this node has not seen from that origin: it may be requested.
    Accept,
    /// The same version again: a re-offer, a retry, or the fan-out coming back.
    Duplicate,
    /// An older version than one already seen from that origin.
    Stale,
}

/// The highest `(origin, version)` this node has accepted, per origin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Echo {
    seen: BTreeMap<[u8; 32], u64>,
}

impl Echo {
    /// A fresh, empty history: a node that has just started has seen nothing.
    pub fn new() -> Self {
        Echo::default()
    }

    /// Judges an incoming offer without recording it.
    pub fn judge(&self, origin: &[u8; 32], version: u64) -> EchoDecision {
        match self.seen.get(origin) {
            None => EchoDecision::Accept,
            Some(highest) if version > *highest => EchoDecision::Accept,
            Some(highest) if version == *highest => EchoDecision::Duplicate,
            Some(_) => EchoDecision::Stale,
        }
    }

    /// Records an accepted offer. Only an accepted one may be recorded, which is what makes
    /// a duplicate a duplicate rather than a new acceptance.
    pub fn accept(&mut self, origin: &[u8; 32], version: u64) {
        let entry = self.seen.entry(*origin).or_insert(0);
        if version > *entry {
            *entry = version;
        }
    }

    /// The highest version seen from `origin`, for diagnostics.
    pub fn highest(&self, origin: &[u8; 32]) -> Option<u64> {
        self.seen.get(origin).copied()
    }

    /// How many origins this node remembers. Bounded by the member count, never by history:
    /// ADR-008 forbids keeping a clipboard history, and this map holds counters, not content.
    pub fn origins(&self) -> usize {
        self.seen.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    #[test]
    fn a_new_origin_is_accepted_and_its_versions_are_monotone() {
        let mut echo = Echo::new();
        assert_eq!(echo.judge(&node(1), 1), EchoDecision::Accept);
        echo.accept(&node(1), 1);
        assert_eq!(echo.judge(&node(1), 1), EchoDecision::Duplicate);
        assert_eq!(echo.judge(&node(1), 2), EchoDecision::Accept);
        echo.accept(&node(1), 2);
        assert_eq!(echo.judge(&node(1), 1), EchoDecision::Stale);
        assert_eq!(echo.highest(&node(1)), Some(2));
    }

    #[test]
    fn versions_are_per_origin_not_global() {
        // Two nodes copy independently; their counters are unrelated, so a low version from
        // a *different* origin is not stale.
        let mut echo = Echo::new();
        echo.accept(&node(1), 9);
        assert_eq!(echo.judge(&node(2), 1), EchoDecision::Accept);
        echo.accept(&node(2), 1);
        assert_eq!(echo.judge(&node(1), 9), EchoDecision::Duplicate);
        assert_eq!(echo.origins(), 2);
    }

    #[test]
    fn only_an_accepted_offer_moves_the_mark() {
        // A refused (stale) offer must not lower or raise anything.
        let mut echo = Echo::new();
        echo.accept(&node(1), 5);
        assert_eq!(echo.judge(&node(1), 4), EchoDecision::Stale);
        assert_eq!(echo.judge(&node(1), 5), EchoDecision::Duplicate);
        assert_eq!(echo.highest(&node(1)), Some(5));
    }

    #[test]
    fn a_three_node_fan_out_settles_without_a_loop() {
        // `AC-28`'s shape: A copies, B and C each see the offer once. Re-offers of the same
        // version — what a loop would look like — are duplicates on every node.
        let mut b = Echo::new();
        let mut c = Echo::new();
        assert_eq!(b.judge(&node(1), 1), EchoDecision::Accept);
        b.accept(&node(1), 1);
        assert_eq!(c.judge(&node(1), 1), EchoDecision::Accept);
        c.accept(&node(1), 1);
        for _ in 0..3 {
            assert_eq!(b.judge(&node(1), 1), EchoDecision::Duplicate);
            assert_eq!(c.judge(&node(1), 1), EchoDecision::Duplicate);
        }
        assert_eq!(b.origins(), 1);
        assert_eq!(c.origins(), 1);
    }
}
