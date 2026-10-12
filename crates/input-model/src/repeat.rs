//! The source-side autorepeat filter: only the first key-down and the up go on the wire.
//!
//! `docs/05-protocol.md`, «Модель событий»: «Source отбрасывает OS-generated repeat events;
//! receiver использует native repeat или один собственный repeat engine. Нельзя
//! одновременно запускать оба». ADR-006 says the same in one line: «Repeat исполняется одним
//! механизмом на receiver; source repeat не дублируется».
//!
//! Why it matters beyond tidiness: the OS of the *source* repeats a held key on its own
//! schedule, with its own delay and rate. If those repeats went on the wire, the receiver
//! would repeat them again (its own engine, or the compositor's on Wayland) and a held key
//! would type at roughly twice the intended rate — and with two independent schedules, in
//! bursts. Dropping them at the source makes the rate the receiver's decision, which is what
//! the capability bits advertise.
//!
//! # Why a pressed set and not a flag
//!
//! A repeat is, by definition, a key-down for a key whose down was already delivered — so on
//! Windows and on the Wayland/EIS capture path the state of the pressed set is the whole
//! detection rule (the platform has no per-event repeat flag). X11 is the exception: a `raw
//! KeyPress` carries `XIKeyRepeat`, so the server says it outright. Both are accepted here:
//! the flag wins where the platform provides it (it is true even when this filter joined the
//! capture mid-hold and has no down in its set), and the pressed-set rule decides otherwise.
//! The up always passes: a key that is up must reach the receiver, or its ledger would keep it
//! pressed.

use std::collections::BTreeSet;

use crate::{HidKey, InputEvent, KeyState};

/// The pressed set and what counts as a repeat.
///
/// One instance per capture session; [`RepeatFilter::clear`] when the capture ends, because
/// the keys held at that moment are released locally and their ups never arrive.
#[derive(Debug, Clone, Default)]
pub struct RepeatFilter {
    pressed: BTreeSet<HidKey>,
}

impl RepeatFilter {
    /// An empty filter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many keys this filter believes are held (for the log).
    #[must_use]
    pub fn pressed(&self) -> usize {
        self.pressed.len()
    }

    /// Feeds one captured event and says whether it is an OS-generated repeat.
    ///
    /// `platform_says_repeat` is X11's `XIKeyRepeat` bit; pass `false` where the platform
    /// cannot know (Windows hooks, EIS capture). Buttons, wheel and pointer events are never
    /// repeats and never change the pressed set.
    pub fn is_repeat(&mut self, event: &InputEvent, platform_says_repeat: bool) -> bool {
        match event {
            InputEvent::Key {
                key,
                state: KeyState::Down,
            } => {
                let first = self.pressed.insert(*key);
                platform_says_repeat || !first
            }
            InputEvent::Key {
                key,
                state: KeyState::Up,
            } => {
                self.pressed.remove(key);
                false
            }
            InputEvent::Button { .. }
            | InputEvent::Scroll { .. }
            | InputEvent::PointerAbsolute(_)
            | InputEvent::PointerRelative { .. } => false,
        }
    }

    /// Forgets the held set: a capture (or a lease) just ended.
    pub fn clear(&mut self) {
        self.pressed.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MouseButton, Position};

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

    /// The rule of the module: the first down is real, every further down of the same key is
    /// the OS repeating it.
    #[test]
    fn only_the_first_down_survives() {
        let mut filter = RepeatFilter::new();
        assert!(
            !filter.is_repeat(&down(0x04), false),
            "the first down is the user's"
        );
        assert!(
            filter.is_repeat(&down(0x04), false),
            "the second is a repeat"
        );
        assert!(filter.is_repeat(&down(0x04), false));
        assert_eq!(filter.pressed(), 1);
        assert!(
            !filter.is_repeat(&up(0x04), false),
            "the up always goes on the wire"
        );
        assert!(
            !filter.is_repeat(&down(0x04), false),
            "a fresh press after the up is real"
        );
        assert_eq!(filter.pressed(), 1);
    }

    /// Two keys held at once are tracked separately, and the repeat of one does not eat the
    /// other's first down.
    #[test]
    fn keys_are_tracked_separately() {
        let mut filter = RepeatFilter::new();
        assert!(!filter.is_repeat(&down(0x04), false));
        assert!(!filter.is_repeat(&down(0x05), false));
        assert!(filter.is_repeat(&down(0x04), false));
        assert!(filter.is_repeat(&down(0x05), false));
        assert_eq!(filter.pressed(), 2);
        assert!(!filter.is_repeat(&up(0x04), false));
        assert!(
            !filter.is_repeat(&down(0x04), false),
            "re-pressed after its own up"
        );
        assert_eq!(filter.pressed(), 2);
    }

    /// X11 states it outright, and that is trusted even when the filter has no down for the
    /// key (a capture that joined while the user was already holding it).
    #[test]
    fn the_platform_flag_wins() {
        let mut filter = RepeatFilter::new();
        assert!(
            filter.is_repeat(&down(0x04), true),
            "the server says repeat"
        );
        assert!(
            !filter.is_repeat(&down(0x05), false),
            "and the flag does not leak to the next key"
        );
        assert_eq!(
            filter.pressed(),
            2,
            "a repeat still counts as held, so its up passes"
        );
    }

    /// Pointer, wheel and button events are never repeats and never touch the pressed set.
    #[test]
    fn only_keys_are_filtered() {
        let mut filter = RepeatFilter::new();
        assert!(!filter.is_repeat(&down(0x04), false));
        assert!(!filter.is_repeat(
            &InputEvent::Button {
                button: MouseButton::Left,
                state: KeyState::Down,
            },
            false
        ));
        assert!(!filter.is_repeat(&InputEvent::PointerAbsolute(Position::new(1.0, 2.0)), false));
        assert!(!filter.is_repeat(&InputEvent::Scroll { h120: 0, v120: 120 }, false));
        assert_eq!(filter.pressed(), 1);
    }

    /// A capture that ends forgets what was held: those ups happen locally and never reach
    /// this filter, and a stale set would eat the next real press.
    #[test]
    fn clear_forgets_the_held_set() {
        let mut filter = RepeatFilter::new();
        assert!(!filter.is_repeat(&down(0x04), false));
        filter.clear();
        assert_eq!(filter.pressed(), 0);
        assert!(
            !filter.is_repeat(&down(0x04), false),
            "the next down is a real one again"
        );
    }
}
