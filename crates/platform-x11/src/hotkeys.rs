//! Pure X11 shortcut key naming and physical-edge matching.

/// X11 core modifier bits (lock modifiers are deliberately ignored).
pub const SHORTCUT_MODIFIERS: u16 = 1 | 4 | 8 | 64;

/// Adds candidate grabs first; a refusal retires only additions, never the live set.
pub fn replace_grabs<E>(
    current: &[(u8, u16)],
    wanted: &[(u8, u16)],
    mut acquire: impl FnMut((u8, u16)) -> Result<(), E>,
    mut release: impl FnMut((u8, u16)),
) -> Result<(), E> {
    let mut added = Vec::new();
    for &grab in wanted {
        if current.contains(&grab) {
            continue;
        }
        if let Err(error) = acquire(grab) {
            for grab in added {
                release(grab);
            }
            return Err(error);
        }
        added.push(grab);
    }
    for &grab in current {
        if !wanted.contains(&grab) {
            release(grab);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub keycode: u8,
    pub modifiers: u16,
}

#[derive(Clone)]
pub struct HotkeyState {
    pub bindings: [Option<Hotkey>; 5],
    pressed: [bool; 256],
    swallowed: [bool; 256],
}

impl Default for HotkeyState {
    fn default() -> Self {
        Self {
            bindings: [None; 5],
            pressed: [false; 256],
            swallowed: [false; 256],
        }
    }
}

impl HotkeyState {
    pub fn seed(&mut self, keys: &[u8; 32]) {
        for (index, pressed) in self.pressed.iter_mut().enumerate() {
            *pressed = keys[index / 8] & (1 << (index % 8)) != 0;
        }
    }

    /// Returns (swallow, newly activated action index). Held triggers remain swallowed on update.
    pub fn edge(&mut self, keycode: u8, down: bool) -> (bool, Option<u8>) {
        let index = usize::from(keycode);
        self.pressed[index] = down;
        if self.swallowed[index] {
            self.swallowed[index] = down;
            return (true, None);
        }
        if !down {
            return (false, None);
        }
        let held = |left, right| self.pressed[left] || self.pressed[right];
        let mask = u16::from(held(50, 62))
            | (u16::from(held(37, 105)) << 2)
            | (u16::from(held(64, 108)) << 3)
            | (u16::from(held(133, 134)) << 6);
        let action = if keycode == 96 && mask == (1 | 4 | 8) {
            Some(0)
        } else {
            self.bindings
                .iter()
                .position(|binding| {
                    binding.is_some_and(|binding| {
                        binding.keycode == keycode && binding.modifiers == mask
                    })
                })
                .map(|index| index as u8)
        };
        if action.is_some() {
            self.swallowed[index] = true;
        }
        (action.is_some(), action)
    }
}

/// Canonical config key name to X11 keysym, resolved through the current server keymap.
pub fn keysym(name: &str) -> Option<u32> {
    if name.len() == 1 {
        return name
            .as_bytes()
            .first()
            .map(|byte| u32::from(byte.to_ascii_lowercase()));
    }
    if let Some(number) = name
        .strip_prefix('F')
        .and_then(|text| text.parse::<u32>().ok())
    {
        if (1..=24).contains(&number) {
            return Some(0xffbe + number - 1);
        }
    }
    Some(match name {
        "Left" => 0xff51,
        "Up" => 0xff52,
        "Right" => 0xff53,
        "Down" => 0xff54,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "Insert" => 0xff63,
        "Delete" => 0xffff,
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "Enter" => 0xff0d,
        "Escape" => 0xff1b,
        "Space" => 0x20,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trigger_repeats_and_release_are_swallowed_across_live_update() {
        let mut state = HotkeyState::default();
        state.bindings[1] = Some(Hotkey {
            keycode: 114,
            modifiers: 4 | 8,
        });
        state.edge(37, true);
        state.edge(64, true);
        assert_eq!(state.edge(114, true), (true, Some(1)));
        state.bindings = [None; 5];
        assert_eq!(state.edge(114, true), (true, None));
        assert_eq!(state.edge(114, false), (true, None));
        assert_eq!(state.edge(114, true), (false, None));
    }
    #[test]
    fn two_modifier_sides_and_exact_modifiers() {
        let mut state = HotkeyState::default();
        state.bindings[0] = Some(Hotkey {
            keycode: 96,
            modifiers: 4,
        });
        state.edge(37, true);
        state.edge(105, true);
        state.edge(37, false);
        assert_eq!(state.edge(96, true), (true, Some(0)));
        state.edge(96, false);
        state.edge(50, true);
        assert_eq!(state.edge(96, true), (false, None));
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    #[test]
    fn refused_candidate_rolls_back_only_new_grabs() {
        let live = [(96, 13)];
        let wanted = [(96, 13), (114, 13), (110, 13)];
        let released = std::cell::RefCell::new(Vec::new());
        let result = replace_grabs(
            &live,
            &wanted,
            |grab| {
                if grab.0 == 110 {
                    Err("conflict")
                } else {
                    Ok(())
                }
            },
            |grab| released.borrow_mut().push(grab),
        );
        assert_eq!(result, Err("conflict"));
        assert_eq!(*released.borrow(), vec![(114, 13)]);
    }
}
