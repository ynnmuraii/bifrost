//! LAN Workspace input model: the normalized input event carried between platform
//! adapters and the mapping tables shared by every Linux backend.
//!
//! This crate is pure: it has no OS dependency, no I/O and no time. It exists so
//! that [`hid_usage_to_evdev`] (USB HID usage -> Linux evdev/input-event code, see
//! ADR-006 and KVM-020) is defined exactly once and used by both `platform-wayland`
//! (EIS keyboard injection, which speaks evdev codes) and `platform-x11`
//! (evdev -> X11 keycode, see `platform-x11::keymap`).
//!
//! The table below is a faithful transcription of `hid_keyboard[]` in
//! `drivers/hid/hid-input.c` (Linux), which the kernel uses directly as the
//! `input_event` code (`map_key_clear(hid_keyboard[usage])`). `0` means the usage is
//! unmapped and `KEY_UNKNOWN` (240) is the kernel sentinel for reserved usages; both
//! are surfaced as-is through [`hid_usage_to_evdev`] so a spike can see the raw value.

#![forbid(unsafe_code)]

/// USB HID usage page for a keyboard (spec: "Keyboard/Keypad", page `0x07`).
pub const HID_USAGE_PAGE_KEYBOARD: u16 = 0x07;
/// USB HID usage page for a mouse (spec: "Generic Desktop", page `0x01`).
pub const HID_USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;

/// `KEY_UNKNOWN`, the kernel sentinel for a reserved HID keyboard usage.
pub const EVDEV_KEY_UNKNOWN: u16 = 240;

/// A physical key identity, layout-independent (ADR-006: the wire carries physical
/// keys, never text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HidUsage {
    /// HID usage page, e.g. [`HID_USAGE_PAGE_KEYBOARD`].
    pub page: u16,
    /// HID usage id inside the page, e.g. `0x04` for `'a'` on the keyboard page.
    pub usage: u16,
}

impl HidUsage {
    /// A usage on the keyboard page (`0x07`).
    pub const fn keyboard(usage: u16) -> Self {
        Self {
            page: HID_USAGE_PAGE_KEYBOARD,
            usage,
        }
    }
}

/// Key/button edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyState {
    /// The key or button went down.
    Down,
    /// The key or button went up.
    Up,
}

impl KeyState {
    /// `true` for [`KeyState::Down`].
    pub const fn is_down(self) -> bool {
        matches!(self, KeyState::Down)
    }
}

/// The five pointer buttons the product maps (KVM DoD: left/right/middle/back/forward,
/// wheel in units of 120).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ButtonKind {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Middle button / wheel click.
    Middle,
    /// Fourth button (browser back).
    Back,
    /// Fifth button (browser forward).
    Forward,
}

impl ButtonKind {
    /// Linux evdev code (`BTN_LEFT`..`BTN_EXTRA`) for this button.
    pub const fn evdev_code(self) -> u16 {
        match self {
            ButtonKind::Left => 0x110,    // BTN_LEFT
            ButtonKind::Right => 0x111,   // BTN_RIGHT
            ButtonKind::Middle => 0x112,  // BTN_MIDDLE
            ButtonKind::Back => 0x113,    // BTN_SIDE
            ButtonKind::Forward => 0x114, // BTN_EXTRA
        }
    }

    /// Maps an evdev `BTN_*` code back to a [`ButtonKind`].
    pub const fn from_evdev_code(code: u16) -> Option<Self> {
        match code {
            0x110 => Some(ButtonKind::Left),
            0x111 => Some(ButtonKind::Right),
            0x112 => Some(ButtonKind::Middle),
            0x113 => Some(ButtonKind::Back),
            0x114 => Some(ButtonKind::Forward),
            _ => None,
        }
    }
}

/// A position in the node's logical coordinate space (see 03-architecture.md,
/// "Logical unit"): Windows DIP, X11 root pixels, Wayland compositor logical space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// Horizontal logical coordinate.
    pub x: f64,
    /// Vertical logical coordinate.
    pub y: f64,
}

impl Position {
    /// Builds a position.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A normalized input event. This is the adapter-facing shape; the wire shape is
/// `LocalEvent` in `docs/ipc.proto`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute pointer position in logical coordinates.
    PointerAbsolute(Position),
    /// A physical key edge, identified by HID usage.
    Key {
        /// The key.
        usage: HidUsage,
        /// The edge.
        state: KeyState,
    },
    /// A pointer button edge.
    Button {
        /// The button.
        button: ButtonKind,
        /// The edge.
        state: KeyState,
    },
    /// Wheel movement in units of 120 (`h120`/`v120`), the Windows/`wl_pointer` unit.
    Scroll {
        /// Horizontal wheel units.
        h120: i32,
        /// Vertical wheel units.
        v120: i32,
    },
}

/// USB HID usage -> Linux evdev (`input_event`) code, for keyboard-page usages.
///
/// Returns `Some(0)` never: `0` means "no code" and yields `None`; the kernel
/// sentinel [`EVDEV_KEY_UNKNOWN`] is returned as-is for reserved usages.
/// Usages outside the keyboard page are `None`.
pub const fn hid_usage_to_evdev(page: u16, usage: u16) -> Option<u16> {
    if page != HID_USAGE_PAGE_KEYBOARD || usage > 255 {
        return None;
    }
    let code = HID_KEYBOARD[usage as usize];
    if code == 0 {
        None
    } else {
        Some(code)
    }
}

/// [`hid_usage_to_evdev`] for a [`HidUsage`].
pub const fn hid_usage_to_evdev_code(usage: HidUsage) -> Option<u16> {
    hid_usage_to_evdev(usage.page, usage.usage)
}

/// Linux evdev code -> HID keyboard usage, if the code appears in the table.
///
/// The table is the one-to-one HID mapping, so it is invertible except for the
/// duplicated entries (both `0x31` and `0x32` map to `KEY_BACKSLASH`, and several
/// usages map to `KEY_UNKNOWN`); the first matching usage wins.
pub fn evdev_to_hid_usage(code: u16) -> Option<u16> {
    HID_KEYBOARD
        .iter()
        .position(|&c| c == code && c != 0)
        .map(|i| i as u16)
}

/// Transcribed `hid_keyboard[256]` from `drivers/hid/hid-input.c`.
#[rustfmt::skip]
const HID_KEYBOARD: [u16; 256] = [
      0,  0,  0,  0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
     50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,  2,  3,
      4,  5,  6,  7,  8,  9, 10, 11, 28,  1, 14, 15, 57, 12, 13, 26,
     27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
     65, 66, 67, 68, 87, 88, 99, 70,119,110,102,104,111,107,109,106,
    105,108,103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
     72, 73, 82, 83, 86,127,116,117,183,184,185,186,187,188,189,190,
    191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
    115,114,240,240,240,121,240, 89, 93,124, 92, 94, 95,240,240,240,
    122,123, 90, 91, 85,240,240,240,240,240,240,240,111,240,240,240,
    240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,
    240,240,240,240,240,240,179,180,240,240,240,240,240,240,240,240,
    240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,240,
    240,240,240,240,240,240,240,240,111,240,240,240,240,240,240,240,
     29, 42, 56,125, 97, 54,100,126,164,166,165,163,161,115,114,113,
    150,158,159,128,136,177,178,176,142,152,173,140,240,240,240,240,
];
