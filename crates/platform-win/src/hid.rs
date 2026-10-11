//! Scancode set 1 (make codes) ⇄ USB HID keyboard/keypad usage ids.
//!
//! The protocol carries physical key identities (ADR-006), so both directions
//! are needed: capture turns a hook scancode into a usage, injection turns a
//! usage back into a scancode for `KEYEVENTF_SCANCODE`.
//!
//! * Capture: `KBDLLHOOKSTRUCT.scanCode` is the make code, and
//!   `KBDLLHOOKSTRUCT.flags & LLKHF_EXTENDED` says whether it came after the
//!   `E0` prefix. Extended codes are represented here with the `0xE000` bit set
//!   ([`SCANCODE_EXTENDED`]).
//! * Injection: `SendInput` wants the low byte in `wScan` plus
//!   `KEYEVENTF_EXTENDEDKEY` for the same extended set.
//!
//! Scancodes are layout-independent: the same physical key reports the same make
//! code whichever of EN/RU/AltGr is active. The mapping is therefore also the
//! reason a wrong layout on the receiver shows up as wrong *characters* while
//! physical keys stay correct.
//!
//! ## Coverage
//!
//! The full 104/105-key ISO/ANSI set: alphanumerics, function row, the editing
//! cluster, the keypad, the three lock keys, and every `E0`-prefixed extended key
//! (right Ctrl/Alt, keypad Enter and `/`, the arrow cluster, Insert/Delete,
//! Home/End/Page, GUI keys, the Menu key, PrintScreen). Two keys are deliberately
//! absent:
//!
//! * `Pause`/`Break` (HID `0x48`) is a three-byte `E1 1D 45` sequence that a
//!   single scancode field cannot express.
//! * Consumer/media keys are on usage page `0x0C`, not the keyboard page.
//!
//! Both are out of the MVP keyboard contract (AC-07 covers ANSI/ISO, EN/RU,
//! AltGr, numpad) and show up as "unmapped" rather than as a random key.

use crate::event::HID_PAGE_KEYBOARD;

/// Marker bit for `E0`-prefixed scancodes in the 16-bit scancode space used here.
pub const SCANCODE_EXTENDED: u16 = 0xE000;

/// Build the 16-bit scancode representation from the parts a low-level hook
/// reports.
#[must_use]
pub fn scancode_from_hook(scan_code: u32, extended: bool) -> u16 {
    let base = (scan_code & 0xFF) as u16;
    if extended {
        SCANCODE_EXTENDED | base
    } else {
        base
    }
}

/// Split a 16-bit scancode into the `(wScan, extended)` pair `SendInput` wants.
#[must_use]
pub fn scancode_parts(scancode: u16) -> (u16, bool) {
    (scancode & 0xFF, scancode & SCANCODE_EXTENDED != 0)
}

/// HID keyboard-page usage for a scancode, if the key is in the table.
///
/// Binary search over a table sorted by scancode; this is the capture hot path.
#[must_use]
pub fn scancode_to_hid(scancode: u16) -> Option<u16> {
    TABLE
        .binary_search_by_key(&scancode, |&(scan, _)| scan)
        .ok()
        .map(|i| TABLE[i].1)
}

/// Scancode for a HID keyboard-page usage, if the key is in the table.
///
/// Used on the injection path, which sees at most a few events per frame, so the
/// linear scan of the small const table is not worth a second index.
#[must_use]
pub fn hid_to_scancode(usage: u16) -> Option<u16> {
    TABLE
        .iter()
        .find(|&&(_, hid)| hid == usage)
        .map(|&(scan, _)| scan)
}

/// Convenience wrapper for a full [`crate::event::HidKey`].
#[must_use]
pub fn hid_key_to_scancode(key: crate::event::HidKey) -> Option<u16> {
    if key.page == HID_PAGE_KEYBOARD {
        hid_to_scancode(key.usage)
    } else {
        None
    }
}

/// `(scancode, HID usage)`, sorted by scancode. One row per physical key.
#[rustfmt::skip]
static TABLE: &[(u16, u16)] = &[
    (0x0001, 0x29), // Esc
    (0x0002, 0x1E), // 1
    (0x0003, 0x1F), // 2
    (0x0004, 0x20), // 3
    (0x0005, 0x21), // 4
    (0x0006, 0x22), // 5
    (0x0007, 0x23), // 6
    (0x0008, 0x24), // 7
    (0x0009, 0x25), // 8
    (0x000A, 0x26), // 9
    (0x000B, 0x27), // 0
    (0x000C, 0x2D), // -_
    (0x000D, 0x2E), // =+
    (0x000E, 0x2A), // Backspace
    (0x000F, 0x2B), // Tab
    (0x0010, 0x14), // Q
    (0x0011, 0x1A), // W
    (0x0012, 0x08), // E
    (0x0013, 0x15), // R
    (0x0014, 0x17), // T
    (0x0015, 0x1C), // Y
    (0x0016, 0x18), // U
    (0x0017, 0x0C), // I
    (0x0018, 0x12), // O
    (0x0019, 0x13), // P
    (0x001A, 0x2F), // [{
    (0x001B, 0x30), // ]}
    (0x001C, 0x28), // Enter
    (0x001D, 0xE0), // Left Ctrl
    (0x001E, 0x04), // A
    (0x001F, 0x16), // S
    (0x0020, 0x07), // D
    (0x0021, 0x09), // F
    (0x0022, 0x0A), // G
    (0x0023, 0x0B), // H
    (0x0024, 0x0D), // J
    (0x0025, 0x0E), // K
    (0x0026, 0x0F), // L
    (0x0027, 0x33), // ;:
    (0x0028, 0x34), // '"
    (0x0029, 0x35), // `~
    (0x002A, 0xE1), // Left Shift
    (0x002B, 0x31), // \|
    (0x002C, 0x1D), // Z
    (0x002D, 0x1B), // X
    (0x002E, 0x06), // C
    (0x002F, 0x19), // V
    (0x0030, 0x05), // B
    (0x0031, 0x11), // N
    (0x0032, 0x10), // M
    (0x0033, 0x36), // ,<
    (0x0034, 0x37), // .>
    (0x0035, 0x38), // /?
    (0x0036, 0xE5), // Right Shift
    (0x0037, 0x55), // Keypad *
    (0x0038, 0xE2), // Left Alt
    (0x0039, 0x2C), // Space
    (0x003A, 0x39), // Caps Lock
    (0x003B, 0x3A), // F1
    (0x003C, 0x3B), // F2
    (0x003D, 0x3C), // F3
    (0x003E, 0x3D), // F4
    (0x003F, 0x3E), // F5
    (0x0040, 0x3F), // F6
    (0x0041, 0x40), // F7
    (0x0042, 0x41), // F8
    (0x0043, 0x42), // F9
    (0x0044, 0x43), // F10
    (0x0045, 0x53), // Num Lock
    (0x0046, 0x47), // Scroll Lock
    (0x0047, 0x5F), // Keypad 7
    (0x0048, 0x60), // Keypad 8
    (0x0049, 0x61), // Keypad 9
    (0x004A, 0x56), // Keypad -
    (0x004B, 0x5C), // Keypad 4
    (0x004C, 0x5D), // Keypad 5
    (0x004D, 0x5E), // Keypad 6
    (0x004E, 0x57), // Keypad +
    (0x004F, 0x59), // Keypad 1
    (0x0050, 0x5A), // Keypad 2
    (0x0051, 0x5B), // Keypad 3
    (0x0052, 0x62), // Keypad 0
    (0x0053, 0x63), // Keypad .
    (0x0056, 0x64), // 102nd key (ISO non-US \|, between LShift and Z)
    (0x0057, 0x44), // F11
    (0x0058, 0x45), // F12
    (0xE01C, 0x58), // Keypad Enter
    (0xE01D, 0xE4), // Right Ctrl
    (0xE035, 0x54), // Keypad /
    (0xE037, 0x46), // PrintScreen
    (0xE038, 0xE6), // Right Alt (AltGr)
    (0xE047, 0x4A), // Home
    (0xE048, 0x52), // Up
    (0xE049, 0x4B), // Page Up
    (0xE04B, 0x50), // Left
    (0xE04D, 0x4F), // Right
    (0xE04F, 0x4D), // End
    (0xE050, 0x51), // Down
    (0xE051, 0x4E), // Page Down
    (0xE052, 0x49), // Insert
    (0xE053, 0x4C), // Delete
    (0xE05B, 0xE3), // Left GUI (Win)
    (0xE05C, 0xE7), // Right GUI (Win)
    (0xE05D, 0x65), // Application / Menu
];

/// Number of physical keys described by [`TABLE`].
#[must_use]
pub const fn table_len() -> usize {
    TABLE.len()
}
