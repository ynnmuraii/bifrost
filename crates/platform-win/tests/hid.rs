use std::collections::BTreeSet;

use input_model::HidKey;
use platform_win::hid::{
    hid_key_to_scancode, hid_to_scancode, scancode_from_hook, scancode_parts, scancode_to_hid,
    table_len,
};

#[test]
fn all_scancodes_and_usages_roundtrip_without_aliases() {
    let mut usages = BTreeSet::new();
    for scan in 0..=u16::MAX {
        if let Some(usage) = scancode_to_hid(scan) {
            assert!(usages.insert(usage), "duplicate usage {usage:#x}");
            assert_eq!(hid_to_scancode(usage), Some(scan));
            assert_eq!(hid_key_to_scancode(HidKey::keyboard(usage)), Some(scan));
            let (base, extended) = scancode_parts(scan);
            assert_eq!(scancode_from_hook(u32::from(base), extended), scan);
        }
    }
    let mut scans = BTreeSet::new();
    for usage in 0..=u16::MAX {
        if let Some(scan) = hid_to_scancode(usage) {
            assert!(scans.insert(scan), "duplicate scancode {scan:#x}");
            assert_eq!(scancode_to_hid(scan), Some(usage));
        }
    }
    assert_eq!(table_len(), 104);
    assert_eq!(usages.len(), table_len());
    assert_eq!(scans.len(), table_len());
}

#[test]
fn hook_conversion_preserves_e0_and_discards_high_scan_bits() {
    for low in 0..=255u32 {
        for high in [0, 0x100, 0xE000, 0xFFFF_FF00] {
            for extended in [false, true] {
                let scan = scancode_from_hook(high | low, extended);
                assert_eq!(scan, low as u16 | if extended { 0xE000 } else { 0 });
                assert_eq!(scancode_parts(scan), (low as u16, extended));
            }
        }
    }
}

#[test]
fn unsupported_scancodes_pause_and_non_keyboard_pages_are_unmapped() {
    for scan in [0, 0xFF, 0xE000, 0xE0FF, 0xE11D, u16::MAX] {
        assert_eq!(scancode_to_hid(scan), None);
    }
    for usage in [0, 0x48, 0x100, u16::MAX] {
        assert_eq!(hid_to_scancode(usage), None);
    }
    for page in [0, 1, 6, 8, 0x0C, u16::MAX] {
        for usage in [0x04, 0x45, 0xE4] {
            assert_eq!(hid_key_to_scancode(HidKey { page, usage }), None);
        }
    }
}

#[test]
fn physical_l_scancode_maps_to_hid_l() {
    assert_eq!(scancode_to_hid(0x26), Some(0x0F));
}
