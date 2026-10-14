//! LAN Workspace input model: the single owner of the normalized input vocabulary
//! and of the mapping tables every backend shares.
//!
//! This crate is pure: no OS dependency, no I/O, no time, no allocation on the hot
//! path. It exists so that `platform-win`, `platform-x11` and `platform-wayland`
//! cannot drift apart (three private copies of the same event enum was the state
//! before this unification).
//!
//! # Contract mapping
//!
//! Names deliberately mirror the wire contracts so the model is the one place that
//! has to be read when a contract changes.
//!
//! | here | `docs/ipc.proto` (agent IPC) | `docs/protocol.proto` (wire) |
//! |---|---|---|
//! | [`HidKey`] | `HidKey { usage_page, usage }` | `Key { usage_page, usage, state }` |
//! | [`KeyState`] | `bool down` on `HidKey`/`ButtonEv` | `KeyState { DOWN = 1, UP = 2 }` |
//! | [`MouseButton`] | `ButtonEv.button` (u32) | `ButtonKind { LEFT = 1 .. FORWARD = 5 }` |
//! | [`InputEvent::PointerAbsolute`] | `PointerAbs { display_id, x, y }` | `Position pointer_absolute` |
//! | [`InputEvent::Key`] | `HidKey` | `Key key` |
//! | [`InputEvent::Button`] | `ButtonEv` | `Button button` |
//! | [`InputEvent::Scroll`] | `ScrollEv { h120, v120 }` | `Scroll { horizontal_units120, vertical_units120 }` |
//! | [`InputEvent::PointerRelative`] | — (no wire event) | — (not on the wire) |
//! | [`CaptureKind`] | `CaptureKind { OBSERVE, TEMPORARY, REMOTE }` | — (agent-local) |
//!
//! Two deliberate notes:
//!
//! * `ipc.proto` flattens the key state to a `bool down`; the wire contract and this
//!   type both keep the explicit [`KeyState`], so the flattening happens only at the
//!   local-IPC encoding boundary and inside `platform-win`'s hook glue.
//! * [`InputEvent::PointerRelative`] is a **capture-direction** event only. Windows
//!   capture is built on raw device deltas, so it needs the shape; `docs/03-architecture.md`
//!   is explicit that relative-only *injection* does not exist and is not added, and
//!   `protocol.proto`'s `InputEvent` has no relative member. A source integrates
//!   deltas into an absolute [`Position`] before anything is sent.

#![forbid(unsafe_code)]

pub mod repeat;

pub use repeat::RepeatFilter;

/// USB HID usage page for a keyboard (spec: "Keyboard/Keypad", page `0x07`).
pub const HID_PAGE_KEYBOARD: u16 = 0x07;

/// `KEY_UNKNOWN`, the kernel sentinel for a reserved HID keyboard usage.
pub const EVDEV_KEY_UNKNOWN: u16 = 240;

/// A physical key identity, layout-independent (ADR-006: the wire carries physical
/// keys, never text).
///
/// Mirrors `docs/protocol.proto` `Key.usage_page`/`Key.usage` and
/// `docs/ipc.proto` `HidKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HidKey {
    /// HID usage page, e.g. [`HID_PAGE_KEYBOARD`].
    pub page: u16,
    /// HID usage id inside the page, e.g. `0x04` for `'a'` on the keyboard page.
    pub usage: u16,
}

impl HidKey {
    /// A key on the keyboard page (`0x07`).
    pub const fn keyboard(usage: u16) -> Self {
        Self {
            page: HID_PAGE_KEYBOARD,
            usage,
        }
    }
}

/// A key or button edge.
///
/// Mirrors `docs/protocol.proto` `KeyState { KEY_STATE_DOWN = 1, KEY_STATE_UP = 2 }`;
/// `docs/ipc.proto` carries the same information as a `bool down`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// The `bool down` form used by `docs/ipc.proto`.
    pub const fn is_up(self) -> bool {
        matches!(self, KeyState::Up)
    }

    /// Builds a state from the `bool down` form.
    pub const fn from_down(down: bool) -> Self {
        if down {
            KeyState::Down
        } else {
            KeyState::Up
        }
    }
}

/// The five pointer buttons the product maps.
///
/// The discriminants are the `docs/protocol.proto` `ButtonKind` numbering
/// (`LEFT = 1` .. `FORWARD = 5`), kept explicit so a wire conversion is a cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum MouseButton {
    /// Primary button.
    Left = 1,
    /// Secondary button.
    Right = 2,
    /// Middle button / wheel click.
    Middle = 3,
    /// Fourth button (browser back).
    Back = 4,
    /// Fifth button (browser forward).
    Forward = 5,
}

impl MouseButton {
    /// Linux evdev code (`BTN_LEFT`..`BTN_EXTRA`) for this button.
    pub const fn evdev_code(self) -> u16 {
        match self {
            MouseButton::Left => 0x110,    // BTN_LEFT
            MouseButton::Right => 0x111,   // BTN_RIGHT
            MouseButton::Middle => 0x112,  // BTN_MIDDLE
            MouseButton::Back => 0x113,    // BTN_SIDE
            MouseButton::Forward => 0x114, // BTN_EXTRA
        }
    }

    /// Maps an evdev `BTN_*` code back to a [`MouseButton`].
    pub const fn from_evdev_code(code: u16) -> Option<Self> {
        match code {
            0x110 => Some(MouseButton::Left),
            0x111 => Some(MouseButton::Right),
            0x112 => Some(MouseButton::Middle),
            0x113 => Some(MouseButton::Back),
            0x114 => Some(MouseButton::Forward),
            _ => None,
        }
    }

    /// Side-button index for the two side buttons (`Back` = 1, `Forward` = 2), or
    /// `None` for the three normal buttons.
    pub const fn side_index(self) -> Option<u32> {
        match self {
            MouseButton::Back => Some(1),
            MouseButton::Forward => Some(2),
            MouseButton::Left | MouseButton::Right | MouseButton::Middle => None,
        }
    }
}

/// A position in the node's logical coordinate space (see 03-architecture.md,
/// "Logical unit"): Windows DIP, X11 root pixels divided by the configured scale,
/// Wayland compositor logical space.
///
/// `f64` because the Wayland branch needs it (fractional scaling, normalized
/// regions) and Windows coordinates are integers that `f64` represents exactly.
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

/// A normalized input event, the adapter-facing shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute pointer position in logical coordinates (`PointerAbs` / `Position`).
    PointerAbsolute(Position),
    /// Relative pointer motion in device units.
    ///
    /// Capture direction only: Windows capture is built on raw deltas, and a source
    /// integrates them into a [`InputEvent::PointerAbsolute`] before anything is
    /// sent. There is no relative injection (see the crate docs).
    PointerRelative {
        /// Horizontal delta in device units.
        dx: i32,
        /// Vertical delta in device units.
        dy: i32,
    },
    /// A physical key edge (`Key` / `HidKey`).
    Key {
        /// The key.
        key: HidKey,
        /// The edge.
        state: KeyState,
    },
    /// A pointer button edge (`Button` / `ButtonEv`).
    Button {
        /// The button.
        button: MouseButton,
        /// The edge.
        state: KeyState,
    },
    /// Wheel movement in units of 120 (`Scroll` / `ScrollEv`), the Windows and
    /// `wl_pointer` granularity.
    Scroll {
        /// Horizontal wheel units.
        h120: i32,
        /// Vertical wheel units.
        v120: i32,
    },
}

/// How far the local session is being captured (`docs/ipc.proto` `CaptureKind`,
/// the three modes of `docs/03-architecture.md`).
///
/// Cross-platform on purpose: every backend has to answer the same question (does
/// local input keep working, and is it buffered?), so the enum is owned here even
/// though only the agent drives the transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureKind {
    /// Pass-through: events are reported, the local desktop keeps working.
    #[default]
    Observe,
    /// Observe an uncommitted transfer with a self-timeout. Local input remains
    /// usable; discrete events must not be replayed or forwarded after commit.
    Temporary,
    /// Suppress locally (and hold the cursor) and forward to the agent.
    Remote,
}

impl CaptureKind {
    /// Whether local input is suppressed in this mode.
    pub const fn is_suppressed(self) -> bool {
        matches!(self, CaptureKind::Remote)
    }
}

/// USB HID usage -> Linux evdev (`input_event`) code, for keyboard-page usages.
///
/// Returns `None` for `0` (the kernel's "no code"); the kernel sentinel
/// [`EVDEV_KEY_UNKNOWN`] is returned as-is for reserved usages. Usages outside the
/// keyboard page are `None`.
pub const fn hid_usage_to_evdev(page: u16, usage: u16) -> Option<u16> {
    if page != HID_PAGE_KEYBOARD || usage > 255 {
        return None;
    }
    let code = HID_KEYBOARD[usage as usize];
    if code == 0 {
        None
    } else {
        Some(code)
    }
}

/// [`hid_usage_to_evdev`] for a [`HidKey`].
pub const fn hid_key_to_evdev(key: HidKey) -> Option<u16> {
    hid_usage_to_evdev(key.page, key.usage)
}

/// Linux evdev code -> HID keyboard usage, if the code appears in the table.
///
/// The table is the one-to-one HID mapping, so it is invertible except for the
/// duplicated entries (both usage `0x31` and `0x32` map to `KEY_BACKSLASH`, and
/// several usages map to `KEY_UNKNOWN`); the first matching usage wins.
pub fn evdev_to_hid_usage(code: u16) -> Option<u16> {
    HID_KEYBOARD
        .iter()
        .position(|&c| c == code && c != 0)
        .map(|i| i as u16)
}

/// Transcribed `hid_keyboard[256]` from `drivers/hid/hid-input.c`.
///
/// The kernel uses these values directly as `input_event` codes
/// (`map_key_clear(hid_keyboard[usage])`), and they have been diffed against the
/// upstream source entry by entry.
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

#[cfg(test)]
mod tests {
    use super::{hid_usage_to_evdev, HID_PAGE_KEYBOARD};

    #[test]
    fn hid_l_maps_to_linux_evdev_key_l() {
        assert_eq!(hid_usage_to_evdev(HID_PAGE_KEYBOARD, 0x0F), Some(38));
    }
}
