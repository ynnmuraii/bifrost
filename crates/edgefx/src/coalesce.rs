//! Core-side rate limit for the edge-effect frames: at most one frame per
//! [`MIN_GAP_MS`] in total (~30 Hz), latest wins, so a ping-pong across the edge is a trickle
//! and never a queue. A held frame is released by [`Coalescer::due`].

use crate::Update;

/// Minimum spacing of two frames (30 Hz).
pub const MIN_GAP_MS: u64 = 33;

#[derive(Debug, Default)]
pub struct Coalescer {
    last_sent: Option<u64>,
    pending: Option<Update>,
}

/// A held `Proximity` must not overwrite a held state change (`Crossed`/`Entered`/`Left`/
/// `Clear`): those decide what the overlay does next.
fn outranks(held: &Update, new: &Update) -> bool {
    matches!(new, Update::Proximity { .. }) && !matches!(held, Update::Proximity { .. })
}

impl Coalescer {
    /// Offers `update` at `now_ms`; returns the frame to send now, if any.
    pub fn push(&mut self, update: Update, now_ms: u64) -> Option<Update> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|held| outranks(held, &update))
        {
            self.pending = Some(update);
        }
        self.due(now_ms)
    }

    /// The held frame, once the spacing allows.
    pub fn due(&mut self, now_ms: u64) -> Option<Update> {
        let ready = self
            .last_sent
            .is_none_or(|last| now_ms.saturating_sub(last) >= MIN_GAP_MS);
        if ready {
            let out = self.pending.take();
            if out.is_some() {
                self.last_sent = Some(now_ms);
            }
            out
        } else {
            None
        }
    }

    /// Whether a frame is waiting (the caller then needs a wake-up at ~[`MIN_GAP_MS`]).
    #[must_use]
    pub const fn waiting(&self) -> bool {
        self.pending.is_some()
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
        let mut gate = Coalescer::default();
        let mut sent = 0;
        for index in 0..20u64 {
            let now = index * 25;
            let update = if index % 2 == 0 {
                Update::Crossed(None)
            } else {
                Update::Entered(target())
            };
            sent += u32::from(gate.push(update, now).is_some());
            sent += u32::from(gate.due(now + 12).is_some());
        }
        assert!(sent <= 16, "sent {sent}");
        assert!(sent >= 1);
    }

    #[test]
    fn the_latest_state_change_wins_and_proximity_never_overwrites_one() {
        let mut gate = Coalescer::default();
        assert!(gate.push(Update::Clear, 0).is_some());
        assert!(gate.push(Update::Crossed(None), 5).is_none());
        assert!(gate
            .push(
                Update::Proximity {
                    target: target(),
                    distance: 3.0
                },
                10
            )
            .is_none());
        assert_eq!(gate.due(40), Some(Update::Crossed(None)));
        assert!(gate.push(Update::Entered(target()), 41).is_none());
        assert_eq!(gate.due(80), Some(Update::Entered(target())));
        assert!(!gate.waiting());
    }
}
