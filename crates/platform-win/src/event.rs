//! Normalised input events.
//!
//! The vocabulary mirrors `outputs/ipc.proto`:
//!
//! | here                     | ipc.proto        |
//! |--------------------------|------------------|
//! | [`InputEvent::Key`]      | `HidKey`         |
//! | [`InputEvent::Button`]   | `ButtonEv`       |
//! | [`InputEvent::Wheel`]    | `ScrollEv`       |
//! | [`InputEvent::PointerAbs`] | `PointerAbs`   |
//! | [`InputEvent::PointerRel`] | `LocalEvent` (relative pointer) |
//!
//! Keys are *physical*: USB HID usage page + usage, per ADR-006. There is no
//! character, no virtual key and no layout anywhere in this type, which is what
//! makes the crate structurally unable to log typed text.

/// USB HID usage page for the keyboard/keypad collection.
pub const HID_PAGE_KEYBOARD: u16 = 0x07;

/// A physical key, identified the way the wire protocol identifies it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct HidKey {
    /// HID usage page, e.g. [`HID_PAGE_KEYBOARD`].
    pub page: u16,
    /// HID usage id within the page, e.g. `0x04` for the `A` position.
    pub usage: u16,
}

impl HidKey {
    /// A key on the keyboard/keypad page.
    #[must_use]
    pub const fn keyboard(usage: u16) -> Self {
        Self {
            page: HID_PAGE_KEYBOARD,
            usage,
        }
    }
}

/// Mouse buttons, in the normalised numbering used on the wire.
///
/// The concrete ids are the ones the protocol side needs to agree on; KVM-020
/// owns the final `HID`/`evdev`/`X11` triples. Only the *set* handled here is
/// fixed: left, right, middle, back, forward.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
#[repr(u8)]
pub enum MouseButton {
    /// Primary button.
    Left = 1,
    /// Secondary button.
    Right = 2,
    /// Middle button.
    Middle = 3,
    /// First side button.
    Back = 4,
    /// Second side button.
    Forward = 5,
}

impl MouseButton {
    /// Side-button index for the two side buttons (`Back` = 1, `Forward` = 2),
    /// or `None` for the three normal buttons.
    #[must_use]
    pub const fn side_index(self) -> Option<u32> {
        match self {
            Self::Back => Some(1),
            Self::Forward => Some(2),
            Self::Left | Self::Right | Self::Middle => None,
        }
    }
}

/// One normalised input event.
///
/// Coordinates are physical pixels of the Windows virtual desktop (negative
/// values are normal to the left/above the primary monitor). Wheel values are in
/// the hi-res 1/120 units the protocol uses (`ScrollEv { h120, v120 }`), which is
/// also `SendInput`'s native `WHEEL_DELTA` granularity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputEvent {
    /// A physical key transition.
    Key { key: HidKey, down: bool },
    /// A button transition.
    Button { button: MouseButton, down: bool },
    /// Relative pointer motion in device units.
    PointerRel { dx: i32, dy: i32 },
    /// Absolute pointer position on the virtual desktop.
    PointerAbs { x: i32, y: i32 },
    /// Wheel motion, 1/120 detent units; horizontal and vertical independent.
    Wheel { h120: i32, v120: i32 },
}

/// Capture mode of the adapter (`BeginCapture` in `ipc.proto`).
///
/// The architecture document names three modes; TEMPORARY is exactly REMOTE with
/// a self-timeout and a local replay of the buffered events, so the spike only
/// implements the two suppression-relevant ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CaptureMode {
    /// Pass-through: events are reported to the agent but the local desktop
    /// keeps working normally.
    #[default]
    Observe,
    /// Suppress locally (and hold the cursor) and report events to the agent.
    Remote,
}

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
