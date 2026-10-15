//! Core-side rate limit for the edge-effect frames: at most one frame per
//! [`MIN_GAP_MS`] in total (~30 Hz), latest wins, so a ping-pong across the edge is a trickle
//! and never a queue. A held frame is released by [`Coalescer::due`]; the caller wakes at
//! [`Coalescer::due_at`].

use crate::Update;

/// Minimum spacing of two frames (30 Hz).
pub const MIN_GAP_MS: u64 = 34;

/// `P` rides along with an [`Update`] (the display id the frame names).
#[derive(Debug)]
pub struct Coalescer<P> {
    last_sent: Option<u64>,
    pending: Option<(Update, P)>,
}

impl<P> Default for Coalescer<P> {
    fn default() -> Self {
        Self {
            last_sent: None,
            pending: None,
        }
    }
}

/// A held state change (`Crossed`/`Entered`/`Left`/`Clear`) is not overwritten by a later
/// `Proximity`: it decides what the overlay does next.
fn outranks(held: &Update, new: &Update) -> bool {
    matches!(new, Update::Proximity { .. }) && !matches!(held, Update::Proximity { .. })
}

impl<P> Coalescer<P> {
    /// Offers a frame at `now_ms`; returns the frame to send now, if any.
    pub fn push(&mut self, update: Update, payload: P, now_ms: u64) -> Option<(Update, P)> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|(held, _)| outranks(held, &update))
        {
            self.pending = Some((update, payload));
        }
        self.due(now_ms)
    }

    /// The held frame, once the spacing allows.
    pub fn due(&mut self, now_ms: u64) -> Option<(Update, P)> {
        let ready = self
            .last_sent
            .is_none_or(|last| now_ms.saturating_sub(last) >= MIN_GAP_MS);
        if !ready {
            return None;
        }
        let out = self.pending.take();
        if out.is_some() {
            self.last_sent = Some(now_ms);
        }
        out
    }

    /// When a held frame may go (`None` when nothing is held).
    #[must_use]
    pub fn due_at(&self) -> Option<u64> {
        self.pending
            .as_ref()
            .map(|_| self.last_sent.map_or(0, |last| last + MIN_GAP_MS))
    }

    /// Whether the spacing allows a new frame right now; a sampler asks this before doing work.
    #[must_use]
    pub fn ready(&self, now_ms: u64) -> bool {
        self.last_sent
            .is_none_or(|last| now_ms.saturating_sub(last) >= MIN_GAP_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Edge, Target};

    fn target() -> Target {
        Target {
            edge: Edge::Right,
            start: 0.0,
            end: 1000.0,
            along: 500.0,
        }
    }

    #[test]
    fn twenty_events_in_half_a_second_are_at_most_thirty_hz() {
        let mut gate = Coalescer::<()>::default();
        let mut sent = 0;
        for index in 0..20u64 {
            let now = index * 25;
            let update = if index % 2 == 0 {
                Update::Crossed(None)
            } else {
                Update::Entered(target())
            };
            sent += u32::from(gate.push(update, (), now).is_some());
            sent += u32::from(gate.due(now + 12).is_some());
        }
        // 500 ms at 30 Hz is at most 16 frames.
        assert!(sent <= 16, "sent {sent}");
        assert!(sent >= 1);
    }

    #[test]
    fn the_latest_state_change_wins_and_proximity_never_overwrites_one() {
        let mut gate = Coalescer::<u8>::default();
        assert!(gate.push(Update::Clear, 0, 0).is_some());
        assert!(gate.push(Update::Crossed(None), 1, 5).is_none());
        assert!(gate
            .push(
                Update::Proximity {
                    target: target(),
                    distance: 3.0
                },
                2,
                10
            )
            .is_none());
        assert_eq!(gate.due_at(), Some(MIN_GAP_MS));
        assert_eq!(gate.due(40), Some((Update::Crossed(None), 1)));
        assert!(gate.push(Update::Entered(target()), 3, 41).is_none());
        assert_eq!(gate.due(80), Some((Update::Entered(target()), 3)));
        assert_eq!(gate.due_at(), None);
    }
}
