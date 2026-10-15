//! Windows-specific part of the input vocabulary.
//!
//! The normalised, platform-independent vocabulary — keys (`HidKey`), buttons
//! (`MouseButton`), the event enum (`InputEvent`), key/button edges (`KeyState`),
//! positions (`Position`) and the capture mode (`CaptureKind`), together with
//! their `ipc.proto` / `protocol.proto` mapping tables — now lives in the shared
//! `input-model` crate. Every backend (Windows, X11, Wayland) names those types
//! directly from `input_model`, so the three copies can no longer drift apart.
//!
//! What stays in this module is the genuinely Windows-specific piece: the
//! emergency combination, expressed in virtual-key space.

/// The emergency combination, `input.emergency_hotkey` (default
/// `Ctrl+Alt+Shift+F12`).
///
/// Virtual-key codes, not HID usages: the combination must be recognised even
/// when the event is about to be swallowed, and modifiers are layout-independent
/// as VKs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EmergencyCombo {
    /// Virtual key of the non-modifier key.
    pub vk: u16,
    /// Require a Ctrl (either side).
    pub ctrl: bool,
    /// Require an Alt (either side).
    pub alt: bool,
    /// Require a Shift (either side).
    pub shift: bool,
}

impl Default for EmergencyCombo {
    fn default() -> Self {
        // VK_F12 = 0x7B, matching config.example.toml `input.emergency_hotkey`.
        Self {
            vk: 0x7B,
            ctrl: true,
            alt: true,
            shift: true,
        }
    }
}

/// Four action-indexed combinations, published to the hook with one atomic store.
/// Index zero is the autonomous emergency release; an absent optional key is disabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkeys(pub u64);

impl Hotkeys {
    pub fn new(combinations: [Option<EmergencyCombo>; 4]) -> Self {
        let mut packed = 0;
        for (index, combo) in combinations.into_iter().enumerate() {
            if let Some(combo) = combo {
                let value = u64::from(combo.vk)
                    | (u64::from(combo.ctrl) << 8)
                    | (u64::from(combo.alt) << 9)
                    | (u64::from(combo.shift) << 10)
                    | (1 << 12);
                packed |= value << (index * 16);
            }
        }
        Self(packed)
    }

    pub fn contains_key(self, vk: u16) -> bool {
        (0..4).any(|index| {
            let combo = (self.0 >> (index * 16)) & 0xffff;
            combo & (1 << 12) != 0 && combo & 0xff == u64::from(vk)
        })
    }

    pub fn matching(
        self,
        vk: u16,
        ctrl: bool,
        alt: bool,
        shift: bool,
        super_key: bool,
    ) -> Option<u8> {
        let modifiers = (u64::from(ctrl) << 8)
            | (u64::from(alt) << 9)
            | (u64::from(shift) << 10)
            | (u64::from(super_key) << 11);
        (0..4).find(|index| {
            let combo = (self.0 >> (usize::from(*index) * 16)) & 0xffff;
            combo & (1 << 12) != 0 && (combo & 0xfff) == (u64::from(vk) | modifiers)
        })
    }
}

/// Returns (keep swallowing this key, emit action) for one physical key edge.
pub fn hotkey_transition(held: bool, down: bool, matched: Option<u8>) -> (bool, Option<u8>) {
    if held {
        (down, None)
    } else if down {
        (matched.is_some(), matched)
    } else {
        (false, None)
    }
}

#[cfg(test)]
mod hotkey_tests {
    use super::*;

    #[test]
    fn exact_matching_and_atomic_replacement() {
        let first = Hotkeys::new([Some(EmergencyCombo::default()), None, None, None]);
        assert_eq!(first.matching(0x7b, true, true, true, false), Some(0));
        assert_eq!(first.matching(0x7b, true, true, false, false), None);
        let next = Hotkeys::new([
            Some(EmergencyCombo::default()),
            Some(EmergencyCombo {
                vk: 0x27,
                ctrl: false,
                alt: false,
                shift: false,
            }),
            None,
            None,
        ]);
        assert_eq!(first.matching(0x27, false, false, false, false), None);
        assert_eq!(next.matching(0x27, false, false, false, false), Some(1));
        assert_eq!(next.matching(0x27, true, false, false, false), None);
        let (held, action) = hotkey_transition(false, true, Some(1));
        assert!(held);
        assert_eq!(action, Some(1));
        // Repeats and key-up stay swallowed even after a live update disables the chord.
        assert_eq!(hotkey_transition(held, true, None), (true, None));
        assert_eq!(hotkey_transition(held, false, None), (false, None));
        assert_eq!(hotkey_transition(false, false, Some(1)), (false, None));
    }
}
