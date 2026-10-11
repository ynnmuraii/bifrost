//! Pure key/button mappings for the X11 backend (ADR-006).
//!
//! ADR-006 fixes the identity of a key as a **HID usage**: layout-independent,
//! never text. The wire carries HID usages; this module turns them into X11
//! keycodes for the *current XKB keymap* and back again.
//!
//! Under the standard XKB `evdev` rules (which every mainstream Linux
//! distribution uses) X11 keycode == Linux evdev code + 8 — see
//! [`X11_KEYCODE_OFFSET`]. That relation is keymap-*independent* because an XKB
//! keymap maps keycode -> keysym(s); it never reassigns keycodes. Consequence: the
//! mapping below stays correct if the user switches from `us` to `de`, while the
//! *characters* produced differ — which is exactly what ADR-006 wants (the remote
//! side decides the layout twice: once when injecting, once when displaying).
//!
//! Everything in this module is a pure function or a constant: no connection, no
//! I/O, no time. It is the main seam available to a unit-test author.
//!
//! Two conventions worth spelling out because they are easy to get wrong:
//!
//! * **Keycodes** are the wire-facing identity of a key. `evdev` codes are the
//!   input-event codes from `linux/input-event-codes.h` (`KEY_A` = 30, `KEY_F12`
//!   = 88). X11 keycodes are `evdev + 8` (`KEY_F12` -> 96).
//! * **X11 button numbers** are *not* evdev `BTN_*` codes. In the core protocol
//!   buttons 1..3 are left/middle/right, 4..7 are the wheel axes
//!   (up/down/left/right), and 8/9 are back/forward. Reusing 4/5 for
//!   back/forward — as some "1..5" tables do — would collide with vertical
//!   scrolling, so [`button_kind_to_x11_button`] uses 1/2/3/8/9.

use input_model::{evdev_to_hid_usage, hid_usage_to_evdev_code, ButtonKind, HidUsage, KeyState};

/// Offset between a Linux evdev code and the X11 keycode of the current XKB
/// keymap under the standard `evdev` rules.
pub const X11_KEYCODE_OFFSET: u16 = 8;

/// One wheel notch in the Windows/`wl_pointer` unit (KVM DoD: 120 units = 1 notch).
pub const WHEEL_UNITS_PER_NOTCH: i32 = 120;

/// X11 button number for [`ButtonKind::Left`].
pub const X11_BUTTON_LEFT: u8 = 1;
/// X11 button number for [`ButtonKind::Middle`].
pub const X11_BUTTON_MIDDLE: u8 = 2;
/// X11 button number for [`ButtonKind::Right`].
pub const X11_BUTTON_RIGHT: u8 = 3;
/// X11 wheel button: vertical scroll up (one notch per press/release pair).
pub const X11_BUTTON_WHEEL_UP: u8 = 4;
/// X11 wheel button: vertical scroll down.
pub const X11_BUTTON_WHEEL_DOWN: u8 = 5;
/// X11 wheel button: horizontal scroll left.
pub const X11_BUTTON_WHEEL_LEFT: u8 = 6;
/// X11 wheel button: horizontal scroll right.
pub const X11_BUTTON_WHEEL_RIGHT: u8 = 7;
/// X11 button number for [`ButtonKind::Back`] (browser back).
pub const X11_BUTTON_BACK: u8 = 8;
/// X11 button number for [`ButtonKind::Forward`] (browser forward).
pub const X11_BUTTON_FORWARD: u8 = 9;

/// HID keyboard-page usage of `F12`.
pub const HID_USAGE_F12: u16 = 0x45;
/// HID keyboard-page usage of left Ctrl.
pub const HID_USAGE_LEFT_CTRL: u16 = 0xE0;
/// HID keyboard-page usage of left Shift.
pub const HID_USAGE_LEFT_SHIFT: u16 = 0xE1;
/// HID keyboard-page usage of left Alt.
pub const HID_USAGE_LEFT_ALT: u16 = 0xE2;
/// HID keyboard-page usage of right Ctrl.
pub const HID_USAGE_RIGHT_CTRL: u16 = 0xE4;
/// HID keyboard-page usage of right Shift.
pub const HID_USAGE_RIGHT_SHIFT: u16 = 0xE5;
/// HID keyboard-page usage of right Alt (AltGr).
pub const HID_USAGE_RIGHT_ALT: u16 = 0xE6;

/// X11 keycode of `F12` (evdev `KEY_F12` = 88, + 8).
///
/// This is the emergency-combo key of the Linux source: `Ctrl+Alt+Shift+F12`,
/// handled inside capture, without core and without network. Note that some
/// distributions still bind `XF86Switch_VT_12` to this chord's keycode; the spike
/// must check manually that it does not switch virtual terminals.
pub const EMERGENCY_F12_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_F12);

/// X11 keycode of left Ctrl (evdev `KEY_LEFTCTRL` = 29, + 8).
pub const LEFT_CTRL_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_LEFT_CTRL);
/// X11 keycode of left Shift (evdev `KEY_LEFTSHIFT` = 42, + 8).
pub const LEFT_SHIFT_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_LEFT_SHIFT);
/// X11 keycode of left Alt (evdev `KEY_LEFTALT` = 56, + 8).
pub const LEFT_ALT_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_LEFT_ALT);
/// X11 keycode of right Ctrl (evdev `KEY_RIGHTCTRL` = 97, + 8).
pub const RIGHT_CTRL_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_RIGHT_CTRL);
/// X11 keycode of right Shift (evdev `KEY_RIGHTSHIFT` = 54, + 8).
pub const RIGHT_SHIFT_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_RIGHT_SHIFT);
/// X11 keycode of right Alt / AltGr (evdev `KEY_RIGHTALT` = 100, + 8).
pub const RIGHT_ALT_X11_KEYCODE: u8 = keycode_for_hid_usage(HID_USAGE_RIGHT_ALT);

/// Keycodes that mean "Ctrl" on the current XKB keymap.
pub const CTRL_KEYCODES: [u8; 2] = [LEFT_CTRL_X11_KEYCODE, RIGHT_CTRL_X11_KEYCODE];
/// Keycodes that mean "Alt" on the current XKB keymap.
pub const ALT_KEYCODES: [u8; 2] = [LEFT_ALT_X11_KEYCODE, RIGHT_ALT_X11_KEYCODE];
/// Keycodes that mean "Shift" on the current XKB keymap.
pub const SHIFT_KEYCODES: [u8; 2] = [LEFT_SHIFT_X11_KEYCODE, RIGHT_SHIFT_X11_KEYCODE];

/// Compile-time helper: HID keyboard usage -> X11 keycode, panicking on a usage
/// the shared `input-model` table does not map. Only used for the constants above,
/// where a failure is a programming error and must break the build.
const fn keycode_for_hid_usage(usage: u16) -> u8 {
    match hid_usage_to_evdev_code(HidUsage::keyboard(usage)) {
        Some(evdev) => {
            let keycode = evdev + X11_KEYCODE_OFFSET;
            if keycode > u8::MAX as u16 {
                panic!("evdev code does not fit in an X11 keycode");
            }
            keycode as u8
        }
        None => panic!("HID usage must map to an evdev code"),
    }
}

/// Linux evdev code -> X11 keycode of the current XKB keymap (evdev + 8).
///
/// `None` when the result does not fit in the 8-bit X11 keycode space (a code
/// above 247), which no real key has.
pub fn evdev_to_x11_keycode(evdev: u16) -> Option<u8> {
    let keycode = evdev.checked_add(X11_KEYCODE_OFFSET)?;
    u8::try_from(keycode).ok()
}

/// HID usage -> X11 keycode, via the shared evdev table (ADR-006).
///
/// `None` for usages outside the keyboard page, for unmapped usages, and for
/// devices with no key here. A reserved usage mapped to the kernel sentinel
/// `KEY_UNKNOWN` (240) does produce `Some(248)`; that keycode is what the kernel
/// would use, and it is normally unbound in the keymap.
pub fn hid_usage_to_x11_keycode(usage: HidUsage) -> Option<u8> {
    evdev_to_x11_keycode(hid_usage_to_evdev_code(usage)?)
}

/// X11 keycode -> evdev code (keycode - 8).
///
/// `None` for the first eight keycodes, which are not reachable from the evdev
/// rules (the minimum keycode there is 8, i.e. evdev 0 = `KEY_RESERVED`).
pub fn x11_keycode_to_evdev(keycode: u8) -> Option<u16> {
    u16::from(keycode).checked_sub(X11_KEYCODE_OFFSET)
}

/// X11 keycode -> HID usage (the inverse of [`hid_usage_to_x11_keycode`]).
///
/// `None` when the keycode has no evdev equivalent, when no table entry maps to
/// that evdev code, and for `KEY_RESERVED`.
pub fn x11_keycode_to_hid_usage(keycode: u8) -> Option<HidUsage> {
    let evdev = x11_keycode_to_evdev(keycode)?;
    Some(HidUsage::keyboard(evdev_to_hid_usage(evdev)?))
}

/// [`ButtonKind`] -> X11 core button number.
///
/// Left/middle/right are 1/2/3; back/forward are 8/9 because 4..7 are the wheel
/// axes in the core protocol (see the module docs).
pub const fn button_kind_to_x11_button(kind: ButtonKind) -> u8 {
    match kind {
        ButtonKind::Left => X11_BUTTON_LEFT,
        ButtonKind::Middle => X11_BUTTON_MIDDLE,
        ButtonKind::Right => X11_BUTTON_RIGHT,
        ButtonKind::Back => X11_BUTTON_BACK,
        ButtonKind::Forward => X11_BUTTON_FORWARD,
    }
}

/// X11 core button number -> [`ButtonKind`], the inverse of
/// [`button_kind_to_x11_button`].
///
/// `None` for the wheel buttons 4..7 and for buttons the product does not carry.
pub const fn x11_button_to_button_kind(button: u8) -> Option<ButtonKind> {
    match button {
        X11_BUTTON_LEFT => Some(ButtonKind::Left),
        X11_BUTTON_MIDDLE => Some(ButtonKind::Middle),
        X11_BUTTON_RIGHT => Some(ButtonKind::Right),
        X11_BUTTON_BACK => Some(ButtonKind::Back),
        X11_BUTTON_FORWARD => Some(ButtonKind::Forward),
        _ => None,
    }
}

/// X11 wheel button -> the wheel scroll it represents, in 120-unit steps.
///
/// Positive vertical is "up" (button 4), matching the HID/Win32 sign convention
/// where a positive delta scrolls the content up. `None` for non-wheel buttons.
pub const fn wheel_from_x11_button(button: u8) -> Option<(i32, i32)> {
    match button {
        X11_BUTTON_WHEEL_UP => Some((0, WHEEL_UNITS_PER_NOTCH)),
        X11_BUTTON_WHEEL_DOWN => Some((0, -WHEEL_UNITS_PER_NOTCH)),
        X11_BUTTON_WHEEL_LEFT => Some((-WHEEL_UNITS_PER_NOTCH, 0)),
        X11_BUTTON_WHEEL_RIGHT => Some((WHEEL_UNITS_PER_NOTCH, 0)),
        _ => None,
    }
}

/// `true` if `keycode` is Ctrl (left or right).
pub const fn is_ctrl_keycode(keycode: u8) -> bool {
    keycode == LEFT_CTRL_X11_KEYCODE || keycode == RIGHT_CTRL_X11_KEYCODE
}

/// `true` if `keycode` is Alt (left or right; AltGr counts, as XKB puts
/// `Alt_R`/`ISO_Level3_Shift` on the right Alt keycode).
pub const fn is_alt_keycode(keycode: u8) -> bool {
    keycode == LEFT_ALT_X11_KEYCODE || keycode == RIGHT_ALT_X11_KEYCODE
}

/// `true` if `keycode` is Shift (left or right).
pub const fn is_shift_keycode(keycode: u8) -> bool {
    keycode == LEFT_SHIFT_X11_KEYCODE || keycode == RIGHT_SHIFT_X11_KEYCODE
}

/// The modifier state tracked from raw key events, used for the emergency chord.
///
/// This is deliberately only about the three modifiers of `Ctrl+Alt+Shift`: the
/// full adapter keeps a real XKB state mask, capture only needs to know whether it
/// can abort. A modifier goes up as soon as *one* of its keycodes is released —
/// holding both Ctrls and releasing one therefore clears `ctrl`, which is the safe
/// direction for an emergency abort.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModifierState {
    /// Ctrl is held (either side).
    pub ctrl: bool,
    /// Alt/AltGr is held (either side).
    pub alt: bool,
    /// Shift is held (either side).
    pub shift: bool,
}

impl ModifierState {
    /// A state with no modifier held.
    pub const fn new() -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Folds one key edge into the tracked state. Non-modifier keycodes are ignored.
    pub fn observe(&mut self, keycode: u8, state: KeyState) {
        let down = state.is_down();
        if is_ctrl_keycode(keycode) {
            self.ctrl = down;
        } else if is_alt_keycode(keycode) {
            self.alt = down;
        } else if is_shift_keycode(keycode) {
            self.shift = down;
        }
    }

    /// `true` when all three modifiers of the emergency chord are held.
    pub const fn emergency_chord(&self) -> bool {
        self.ctrl && self.alt && self.shift
    }

    /// `true` when no tracked modifier is held.
    pub const fn is_clear(&self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }
}
