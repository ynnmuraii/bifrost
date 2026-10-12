//! Software autorepeat on the receiver, and the guard that stops it (KVM-036, AC-14).
//!
//! ADR-006 and `docs/05-protocol.md` fix the rules this module implements: repeat is
//! performed by *one* mechanism on the receiver (the source drops its own OS repeats —
//! `input_model::RepeatFilter`), the mode comes from `[input].repeat_mode`, and it stops
//! within `repeat_guard_ms` (150 ms) after the last input or renew.
//!
//! # Which engine a backend uses
//!
//! * **native** — the platform repeats a held *injected* key by itself. That is X11
//!   (`XTest`-held keys repeat while `XAutoRepeat` is on) and Wayland (the compositor or the
//!   application repeats, from `wl_keyboard.repeat_info`). There is no software engine then,
//!   and the guard is a *release*: letting the held keys up is the only way to stop a repeat
//!   the OS owns.
//! * **software** — the agent repeats the key itself, at the OS rate and delay. This is
//!   Windows, and it is not a preference: measured on Windows 10.0.26200, a key injected with
//!   `SendInput` and held without an up produces **no** further down of its own (the hook
//!   saw exactly one, `platform-win` test `an_injected_key_does_or_does_not_autorepeat`), so
//!   `native` there would mean a held key that never repeats. The guard then stops the
//!   engine.
//!
//! # What the engine does with the event stream
//!
//! It repeats the **single most recent held key** — the key whose down arrived last and has
//! not been released. That is what the OS does (one repeat slot, whichever key was pressed
//! last), and it keeps the injected stream small: one repeat per interval, not one per held
//! key.
//!
//! The clock is a parameter, never `Instant::now()`, so the schedule is testable and the
//! session's monotonic `now_ms` (the same clock the watchdog uses) drives it in production.

use input_model::{HidKey, InputEvent, KeyState};

/// The OS's autorepeat delay and interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepeatTiming {
    /// How long a key is held before the first repeat.
    pub delay_ms: u64,
    /// The gap between repeats after that.
    pub interval_ms: u64,
}

impl Default for RepeatTiming {
    /// The X11 and Windows defaults for a keyboard nobody has reconfigured
    /// (`xset r rate` shows `660 25`-ish; Windows ships 500 ms / ~31 Hz).
    fn default() -> Self {
        Self {
            delay_ms: 500,
            interval_ms: 30,
        }
    }
}

impl RepeatTiming {
    /// Turns a Windows `SPI_GETKEYBOARDDELAY`/`SPI_GETKEYBOARDSPEED` pair into a schedule.
    ///
    /// Only a Windows backend has those indices to convert, and the mapping is unit-tested on
    /// every host, so the function is simply unused where nothing calls it.
    ///
    /// Windows expresses the delay as an index 0..=3 of 250 ms steps, and the speed as an
    /// index 0..=31 that maps linearly onto 2.5..30 repeats per second (the widely used
    /// reading of the `SystemParametersInfo` documentation).
    #[must_use]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn from_windows(delay_index: u32, speed: u32) -> Self {
        let delay_ms = 250 * u64::from(delay_index.min(3) + 1);
        let per_second = 2.5 + (f64::from(speed.min(31)) / 31.0) * 27.5;
        let interval_ms = (1000.0 / per_second).round().max(1.0) as u64;
        Self {
            delay_ms,
            interval_ms,
        }
    }
}

/// What the engine is currently repeating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    key: HidKey,
    /// When the key went down on this machine: the first repeat is `delay_ms` later.
    down_ms: u64,
    /// When the last repeat was emitted; `down_ms` until the first one.
    last_ms: u64,
}

/// The software repeat engine.
#[derive(Debug, Clone, Default)]
pub struct SoftwareRepeat {
    timing: RepeatTiming,
    slot: Option<Slot>,
    repeats: u64,
    stopped: bool,
}

impl SoftwareRepeat {
    /// An engine with the backend's timing.
    #[must_use]
    pub fn new(timing: RepeatTiming) -> Self {
        Self {
            timing,
            slot: None,
            repeats: 0,
            stopped: false,
        }
    }

    /// How many repeats have been emitted since the last `start` (for the log).
    #[must_use]
    pub const fn repeats(&self) -> u64 {
        self.repeats
    }

    /// The key being repeated, if any (the guard test asserts on this).
    #[cfg(test)]
    #[must_use]
    pub fn holding(&self) -> Option<HidKey> {
        self.slot.map(|slot| slot.key)
    }

    /// A batch went to the receiver: remember which key it holds last.
    ///
    /// `now_ms` is when the batch was applied. A key-down takes the slot over (the OS does
    /// the same with the most recently pressed key); a key-up frees the slot when it is the
    /// one being repeated.
    pub fn applied(&mut self, events: &[InputEvent], now_ms: u64) {
        for event in events {
            match event {
                InputEvent::Key {
                    key,
                    state: KeyState::Down,
                } => {
                    // A new press is new intent: even a guard that just fired must not
                    // swallow it (the user pressed the key again after the hiccup).
                    self.stopped = false;
                    self.slot = Some(Slot {
                        key: *key,
                        down_ms: now_ms,
                        last_ms: now_ms,
                    });
                }
                InputEvent::Key {
                    key,
                    state: KeyState::Up,
                } if self.slot.is_some_and(|slot| slot.key == *key) => {
                    self.slot = None;
                }
                _ => {}
            }
        }
    }

    /// The repeat that is due at `now_ms`, consuming it so the caller injects it once.
    pub fn due(&mut self, now_ms: u64) -> Option<HidKey> {
        if self.stopped {
            return None;
        }
        let slot = self.slot?;
        // The delay belongs to the press, the interval to the repeats: a slot that has not
        // repeated yet waits `delay_ms` from its own down, whatever the counter says.
        let next_at = if slot.last_ms == slot.down_ms {
            slot.down_ms.saturating_add(self.timing.delay_ms)
        } else {
            slot.last_ms.saturating_add(self.timing.interval_ms)
        };
        if now_ms < next_at {
            return None;
        }
        self.slot = Some(Slot {
            last_ms: now_ms,
            ..slot
        });
        self.repeats += 1;
        Some(slot.key)
    }

    /// The guard fired: stop repeating, whatever the held key is.
    ///
    /// Returns the key that was being repeated, so the caller can say so in the log. The
    /// engine stays stopped until the next genuine key-down: a `LeaseRenew` after a hiccup
    /// must not restart a repeat the user cannot see the reason for.
    pub fn stop(&mut self) -> Option<HidKey> {
        let was = self.slot.map(|slot| slot.key);
        self.slot = None;
        self.stopped = true;
        was
    }

    /// A new lease (or a new capture): repeats may start again.
    pub fn resume(&mut self) {
        self.slot = None;
        self.stopped = false;
        self.repeats = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(usage: u16) -> InputEvent {
        InputEvent::Key {
            key: HidKey::keyboard(usage),
            state: KeyState::Down,
        }
    }

    fn up(usage: u16) -> InputEvent {
        InputEvent::Key {
            key: HidKey::keyboard(usage),
            state: KeyState::Up,
        }
    }

    fn engine() -> SoftwareRepeat {
        SoftwareRepeat::new(RepeatTiming {
            delay_ms: 500,
            interval_ms: 30,
        })
    }

    /// Nothing repeats before the OS delay, then it repeats every interval — on the injected
    /// clock, so the test is exact instead of approximate.
    #[test]
    fn the_schedule_is_the_os_delay_then_the_os_interval() {
        let mut engine = engine();
        engine.applied(&[down(0x04)], 1000);
        assert_eq!(engine.due(1499), None, "one millisecond before the delay");
        assert_eq!(
            engine.due(1500),
            Some(HidKey::keyboard(0x04)),
            "the first repeat"
        );
        assert_eq!(engine.due(1529), None);
        assert_eq!(engine.due(1530), Some(HidKey::keyboard(0x04)));
        assert_eq!(engine.due(1560), Some(HidKey::keyboard(0x04)));
        assert_eq!(engine.repeats(), 3);
    }

    /// One slot, the most recent key: pressing another key takes the repeat over, exactly as
    /// the OS does.
    #[test]
    fn the_most_recent_key_owns_the_slot() {
        let mut engine = engine();
        engine.applied(&[down(0x04)], 0);
        engine.applied(&[down(0x05)], 100);
        assert_eq!(engine.holding(), Some(HidKey::keyboard(0x05)));
        assert_eq!(
            engine.due(599),
            None,
            "05 repeats 500 ms after its own down"
        );
        assert_eq!(engine.due(600), Some(HidKey::keyboard(0x05)));
    }

    /// Releasing the repeated key frees the slot; releasing another key does not.
    #[test]
    fn an_up_frees_the_slot() {
        let mut engine = engine();
        engine.applied(&[down(0x04), down(0x05)], 0);
        engine.applied(&[up(0x04)], 10);
        assert_eq!(engine.holding(), Some(HidKey::keyboard(0x05)));
        engine.applied(&[up(0x05)], 20);
        assert_eq!(engine.holding(), None);
        assert_eq!(engine.due(10_000), None);
    }

    /// A button or a pointer event in the same batch does not disturb the slot.
    #[test]
    fn only_keys_touch_the_slot() {
        let mut engine = engine();
        engine.applied(&[down(0x04)], 0);
        engine.applied(
            &[InputEvent::Button {
                button: input_model::MouseButton::Left,
                state: KeyState::Down,
            }],
            100,
        );
        assert_eq!(engine.holding(), Some(HidKey::keyboard(0x04)));
        assert_eq!(engine.due(500), Some(HidKey::keyboard(0x04)));
    }

    /// The guard's whole contract: after `stop`, no repeat is due, however long the silence
    /// lasts; a new key-down resumes the schedule.
    #[test]
    fn stop_ends_the_repeat_until_the_next_press() {
        let mut engine = engine();
        engine.applied(&[down(0x04)], 0);
        assert_eq!(engine.due(500), Some(HidKey::keyboard(0x04)));
        assert_eq!(
            engine.stop(),
            Some(HidKey::keyboard(0x04)),
            "the guard names what stopped"
        );
        assert_eq!(engine.due(600), None);
        assert_eq!(engine.due(100_000), None, "silence keeps it stopped");
        engine.applied(&[down(0x04)], 100_100);
        assert_eq!(
            engine.due(100_599),
            None,
            "the delay starts over for the new press"
        );
        assert_eq!(engine.due(100_600), Some(HidKey::keyboard(0x04)));
    }

    /// Windows expresses its repeat settings as two indices; the mapping is what the guard's
    /// budget is measured against (AC-14's ⌈rate × 150 ms⌉).
    #[test]
    fn the_windows_mapping_matches_the_documented_ranges() {
        assert_eq!(
            RepeatTiming::from_windows(0, 0),
            RepeatTiming {
                delay_ms: 250,
                interval_ms: 400
            },
            "the slowest setting: 2.5/s"
        );
        assert_eq!(
            RepeatTiming::from_windows(3, 31),
            RepeatTiming {
                delay_ms: 1000,
                interval_ms: 33
            },
            "the slowest delay and the fastest rate: 30/s"
        );
        // The fast end of the budget: at 30/s, 150 ms allows at most five extra characters.
        assert!((150 / RepeatTiming::from_windows(0, 31).interval_ms) <= 5);
    }

    /// `resume` is what a fresh lease does: the counters and the stopped flag go back.
    #[test]
    fn resume_forgets_a_stopped_engine() {
        let mut engine = engine();
        engine.applied(&[down(0x04)], 0);
        engine.stop();
        engine.resume();
        assert_eq!(engine.repeats(), 0);
        engine.applied(&[down(0x05)], 0);
        assert_eq!(engine.due(500), Some(HidKey::keyboard(0x05)));
    }
}
