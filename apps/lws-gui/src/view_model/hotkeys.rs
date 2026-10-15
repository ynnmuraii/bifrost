//! GUI-only shortcut capture. The service remains the sole configuration writer.

use eframe::egui::{Event, Key};
use protocol::ipc;
use ui_text::{text, Key as TextKey, Lang};

/// Stable wire action, catalogue label, and default chord, in display order.
pub const ROWS: [(i32, TextKey, &str); 4] = [
    (1, TextKey::Emergency, "Ctrl+Alt+Shift+F12"),
    (2, TextKey::NextComputer, "Ctrl+Alt+Shift+Right"),
    (3, TextKey::ReturnHere, "Ctrl+Alt+Shift+Left"),
    (4, TextKey::ClaimOwnership, "Ctrl+Alt+Shift+Home"),
];

/// Capture state survives repainting, but never writes a local config file.
#[derive(Clone, Default)]
pub struct Editor {
    pub capturing: Option<i32>,
    pub error: Option<String>,
}

impl Editor {
    /// Consume one non-repeat key press, leaving Escape to cancel capture only.
    pub fn capture(
        &mut self,
        lang: Lang,
        events: &[Event],
        bindings: &[ipc::HotkeyBinding],
    ) -> Option<(i32, String)> {
        let action = self.capturing?;
        for event in events {
            let Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } = event
            else {
                continue;
            };
            if *key == Key::Escape {
                self.capturing = None;
                self.error = None;
                return None;
            }
            let name = match key {
                Key::ArrowRight => "Right",
                Key::ArrowLeft => "Left",
                Key::ArrowUp => "Up",
                Key::ArrowDown => "Down",
                _ => key.name(),
            };
            let mut chord = String::new();
            if modifiers.ctrl {
                chord.push_str("Ctrl+");
            }
            if modifiers.alt {
                chord.push_str("Alt+");
            }
            if modifiers.shift {
                chord.push_str("Shift+");
            }
            if modifiers.mac_cmd {
                chord.push_str("Super+");
            }
            chord.push_str(name);
            match validate(lang, action, &chord, bindings) {
                Ok(()) => {
                    self.capturing = None;
                    self.error = None;
                    return Some((action, chord));
                }
                Err(error) => self.error = Some(error),
            }
            break;
        }
        None
    }
}

/// Validate the entire proposed binding set, including semantic conflicts.
pub fn validate(
    lang: Lang,
    action: i32,
    chord: &str,
    bindings: &[ipc::HotkeyBinding],
) -> Result<(), String> {
    let mut proposed = config::HotkeyBindings::default();
    for binding in bindings {
        if let Some(action) = config_action(binding.action) {
            proposed.set(action, binding.chord.clone());
        }
    }
    let action =
        config_action(action).ok_or_else(|| text(lang, TextKey::InvalidShortcut).to_owned())?;
    proposed.set(action, chord.to_owned());
    proposed.validate().map_err(|error| error.to_string())
}

fn config_action(action: i32) -> Option<config::HotkeyAction> {
    match action {
        1 => Some(config::HotkeyAction::EmergencyRelease),
        2 => Some(config::HotkeyAction::SwitchNext),
        3 => Some(config::HotkeyAction::ReturnLocal),
        4 => Some(config::HotkeyAction::ClaimOwnership),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflicts_are_rejected_and_escape_cancels_without_a_request() {
        assert!(validate(Lang::Ru, 2, ROWS[0].2, &[]).is_err());
        let mut editor = Editor {
            capturing: Some(1),
            error: Some("old".into()),
        };
        let event = Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        };
        assert_eq!(editor.capture(Lang::Ru, &[event], &[]), None);
        assert_eq!(editor.capturing, None);
        assert_eq!(editor.error, None);
    }

    #[test]
    fn capture_normalizes_arrows_and_finishes_after_one_request() {
        let mut editor = Editor {
            capturing: Some(2),
            error: None,
        };
        let event = Event::Key {
            key: Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: eframe::egui::Modifiers {
                ctrl: true,
                alt: true,
                shift: true,
                ..Default::default()
            },
        };
        assert_eq!(
            editor.capture(Lang::Ru, &[event.clone()], &[]),
            Some((2, ROWS[1].2.to_owned()))
        );
        assert_eq!(editor.capture(Lang::Ru, &[event], &[]), None);
        assert_eq!(editor.capturing, None);
    }
}
