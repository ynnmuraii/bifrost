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
