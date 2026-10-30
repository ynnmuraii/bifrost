//! The receiver-takeover thresholds, as a pure state machine (KVM-035, AC-13).
//!
//! A lease on this node means somebody else drives the pointer here; the machine's own
//! keyboard and mouse are still attached, and the user reaching for them is how they take
//! the desk back. `docs/02-product-requirements.md`, AC-13 is the rule this module
//! implements: «Физическое движение тачпада/мыши ноутбука больше порога или нажатие клавиши
//! в Remote возвращает source в Local ≤150 ms».
//!
//! Three decisions are in here, and nothing else:
//!
//! * **A key or a button press takes over immediately.** There is no useful threshold for
//!   «the user pressed a key»: one press is intent, and waiting for another would type into
//!   the wrong machine. Releases do not count — a release arrives when the user lets go of
//!   something, and a key held through the transition would otherwise fire twice.
//! * **Motion counts only after it accumulates past `[input].takeover_threshold_px`.** A
//!   trackpad reports a pixel or two from a resting palm, a desk bump or a knuckle; the
//!   default 12 logical pixels is comfortably above that and far below a deliberate reach.
//!   The distance is accumulated as Euclidean length, so a slow drag and a flick of the
//!   same length are equally convincing.
//! * **Scroll and absolute re-positioning contribute motion, not triggers.** A wheel turn
//!   is not directional intent to take over (the pointer stays where it is), but the
//!   `PointerAbsolute` deltas a receiver's own injector-free path produces are still
//!   movement and are accumulated from the previous position.
//!
//! What is *not* here is the part that makes it trustworthy: injected events must never
//! reach this module. That filtering is platform work — `platform-win` drops events with the
//! OS injected flag or our `dwExtraInfo` tag in the hook, and `platform-x11` drops raw
//! events whose `deviceid`/`sourceid` is an XTEST slave (`platform_x11::capture`). Both are
//! what makes «a human touched this machine» mean it, and both are why this module can be a
//! threshold and nothing more.

use input_model::{InputEvent, KeyState, Position};

/// What a physical event turned out to be, once it crossed the threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Trigger {
    /// A key went down.
    Key,
    /// A button went down.
    Button,
    /// Motion accumulated past the threshold; the distance it reached.
    Motion {
        /// Euclidean distance from where the accumulation started, in logical units.
        distance_px: f64,
    },
}

impl Trigger {
    /// The short word the log and `docs/03-architecture.md` name the takeover by.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Button => "button",
            Self::Motion { .. } => "motion",
        }
    }
}

/// The accumulation state between events.
///
/// One detector per lease: [`Detector::arm`] starts a fresh accumulation, which is what
/// makes the threshold mean «this much movement *since the pointer was handed over*»
/// rather than a running total over the session.
#[derive(Debug, Clone)]
pub struct Detector {
    threshold_px: f64,
    accumulated_px: f64,
    last_absolute: Option<Position>,
    triggers: u64,
}

impl Detector {
    /// A detector with the configured threshold.
    ///
    /// `threshold_px = 0` is accepted and means «any motion at all», which is what a user who
    /// writes it asked for; the config validator owns the sane default (12).
    #[must_use]
    pub fn new(threshold_px: u32) -> Self {
        Self {
            threshold_px: f64::from(threshold_px),
            accumulated_px: 0.0,
            last_absolute: None,
            triggers: 0,
        }
    }

    /// Forgets the accumulation: a lease just started, or a trigger fired.
    pub fn arm(&mut self) {
        self.accumulated_px = 0.0;
        self.last_absolute = None;
    }

    /// How many triggers this detector has raised (for the log and the smoke).
    #[must_use]
    pub const fn triggers(&self) -> u64 {
        self.triggers
    }

    /// Feeds one *physical* event (the caller has already dropped injected ones).
    ///
    /// Returns the trigger when this event is the one that crossed the line; a detector that
    /// stays silent returns `None`, which is the normal case for every event of a lease.
    pub fn observe(&mut self, event: &InputEvent) -> Option<Trigger> {
        let trigger = match event {
            InputEvent::Key {
                state: KeyState::Down,
                ..
            } => Some(Trigger::Key),
            InputEvent::Button {
                state: KeyState::Down,
                ..
            } => Some(Trigger::Button),
            InputEvent::PointerAbsolute(position) => self.accumulate_absolute(*position),
            InputEvent::PointerRelative { dx, dy } => {
                self.accumulate(f64::from(*dx).hypot(f64::from(*dy)))
            }
            // A wheel turn is not intent to take over the desk: the pointer does not move.
            InputEvent::Scroll { .. } => None,
            // Releases are not intent either (see the module documentation).
            InputEvent::Key {
                state: KeyState::Up,
                ..
            }
            | InputEvent::Button {
                state: KeyState::Up,
                ..
            } => None,
        };
        if trigger.is_some() {
            self.triggers += 1;
            self.arm();
        }
        trigger
    }

    /// Adds `distance` to the accumulation and reports the crossing.
    fn accumulate(&mut self, distance: f64) -> Option<Trigger> {
        self.accumulated_px += distance;
        if self.accumulated_px >= self.threshold_px {
            return Some(Trigger::Motion {
                distance_px: self.accumulated_px,
            });
        }
        None
    }

    /// Motion towards a new absolute position: the delta from the previous one is what the
    /// user's hand moved.
    fn accumulate_absolute(&mut self, position: Position) -> Option<Trigger> {
        let previous = self.last_absolute.replace(position);
        let Some(previous) = previous else {
            // The first absolute report says where the pointer is, not how far it moved.
            return None;
        };
        let dx = position.x - previous.x;
        let dy = position.y - previous.y;
        self.accumulate(dx.hypot(dy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_model::{HidKey, MouseButton};

    fn key(state: KeyState) -> InputEvent {
        InputEvent::Key {
            key: HidKey::keyboard(0x04),
            state,
        }
    }

    fn button(state: KeyState) -> InputEvent {
        InputEvent::Button {
            button: MouseButton::Left,
            state,
        }
    }

    fn relative(dx: i32, dy: i32) -> InputEvent {
        InputEvent::PointerRelative { dx, dy }
    }

    /// A key press is intent; a release is not (the user letting go is not reaching for the
    /// machine, and a key held across the transition would otherwise fire twice).
    #[test]
    fn a_key_press_takes_over_and_a_release_does_not() {
        let mut detector = Detector::new(12);
        assert_eq!(detector.observe(&key(KeyState::Up)), None);
        assert_eq!(detector.observe(&key(KeyState::Down)), Some(Trigger::Key));
        assert_eq!(detector.triggers(), 1);
    }

    /// A button press is as explicit as a key press.
    #[test]
    fn a_button_press_takes_over() {
        let mut detector = Detector::new(12);
        assert_eq!(detector.observe(&button(KeyState::Up)), None);
        assert_eq!(
            detector.observe(&button(KeyState::Down)),
            Some(Trigger::Button)
        );
    }

    /// Motion is accumulated, and the threshold is the sum of several small moves — that is
    /// the whole point: one report is a palm, twelve pixels are a reach.
    #[test]
    fn motion_takes_over_only_past_the_threshold() {
        let mut detector = Detector::new(12);
        assert_eq!(detector.observe(&relative(3, 4)), None); // 5 px
        assert_eq!(detector.observe(&relative(3, 4)), None); // 10 px
        assert_eq!(
            detector.observe(&relative(3, 4)),
            Some(Trigger::Motion { distance_px: 15.0 })
        );
        // The accumulation starts over, so the next 5 px do not trigger.
        assert_eq!(detector.observe(&relative(3, 4)), None);
    }

    /// A single flick past the threshold triggers on that event, and `arm` (a new lease)
    /// cleans the slate.
    #[test]
    fn a_long_move_triggers_at_once_and_arm_forgets_it() {
        let mut detector = Detector::new(12);
        assert_eq!(
            detector.observe(&relative(0, 40)),
            Some(Trigger::Motion { distance_px: 40.0 })
        );
        detector.arm();
        assert_eq!(detector.observe(&relative(0, 11)), None);
    }

    /// An absolute pointer path contributes its deltas: the first report has no previous
    /// position to compare with and must not fabricate movement.
    #[test]
    fn absolute_positions_contribute_their_deltas() {
        let mut detector = Detector::new(12);
        assert_eq!(
            detector.observe(&InputEvent::PointerAbsolute(Position::new(100.0, 100.0))),
            None
        );
        assert_eq!(
            detector.observe(&InputEvent::PointerAbsolute(Position::new(105.0, 105.0))),
            None,
            "7.07 px is below the threshold"
        );
        match detector.observe(&InputEvent::PointerAbsolute(Position::new(115.0, 105.0))) {
            Some(Trigger::Motion { distance_px }) => assert!(
                (distance_px - 17.07).abs() < 0.01,
                "about 17 px after both legs, got {distance_px}"
            ),
            other => panic!("10 more pixels must cross the threshold, got {other:?}"),
        }
    }

    /// A wheel turn moves nothing, so it never takes the desk over.
    #[test]
    fn scrolling_is_not_a_takeover() {
        let mut detector = Detector::new(12);
        for _ in 0..20 {
            assert_eq!(
                detector.observe(&InputEvent::Scroll { h120: 0, v120: 120 }),
                None
            );
        }
        assert_eq!(detector.triggers(), 0);
    }

    /// A zero threshold means «any motion at all» — the user asked for it explicitly.
    #[test]
    fn a_zero_threshold_triggers_on_any_motion() {
        let mut detector = Detector::new(0);
        assert_eq!(
            detector.observe(&relative(0, 0)),
            Some(Trigger::Motion { distance_px: 0.0 })
        );
    }
}
