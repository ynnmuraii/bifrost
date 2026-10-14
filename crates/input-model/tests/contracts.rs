use input_model::{
    evdev_to_hid_usage, hid_key_to_evdev, hid_usage_to_evdev, CaptureKind, HidKey, KeyState,
    MouseButton, EVDEV_KEY_UNKNOWN, HID_PAGE_KEYBOARD,
};

// Independent oracle: numeric hid_keyboard[256] entries extracted from Linux v6.12,
// https://raw.githubusercontent.com/torvalds/linux/v6.12/drivers/hid/hid-input.c
// `unk` is expanded to Linux KEY_UNKNOWN (240). No network or file I/O at runtime.
fn linux_keyboard_codes() -> Vec<u16> {
    include_str!("fixtures/linux-v6.12-hid-keyboard.txt")
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|token| !token.is_empty())
        .map(|token| token.parse().expect("numeric Linux keyboard code"))
        .collect()
}

#[test]
fn keyboard_mapping_matches_linux_reference_for_every_usage() {
    let codes = linux_keyboard_codes();
    assert_eq!(codes.len(), 256);
    for (usage, code) in codes.into_iter().enumerate() {
        let expected = (code != 0).then_some(code);
        assert_eq!(hid_usage_to_evdev(0x07, usage as u16), expected);
        assert_eq!(hid_key_to_evdev(HidKey::keyboard(usage as u16)), expected);
    }
}

#[test]
fn inverse_mapping_selects_first_usage_and_rejects_absent_codes() {
    let mut first_usage = std::collections::BTreeMap::new();
    for (usage, code) in linux_keyboard_codes().into_iter().enumerate() {
        if code != 0 {
            first_usage.entry(code).or_insert(usage as u16);
        }
    }
    for code in 0..=u16::MAX {
        let expected = first_usage.get(&code).copied();
        assert_eq!(evdev_to_hid_usage(code), expected, "evdev {code}");
        if let Some(usage) = expected {
            assert_eq!(hid_key_to_evdev(HidKey::keyboard(usage)), Some(code));
        }
    }
    assert_eq!(evdev_to_hid_usage(43), Some(0x31)); // shared by 0x31 and 0x32
    assert_eq!(evdev_to_hid_usage(240), Some(0x82));
}

#[test]
fn unsupported_pages_and_out_of_range_usages_are_unmapped() {
    assert_eq!(HID_PAGE_KEYBOARD, 0x07);
    for page in 0..=u16::MAX {
        if page != HID_PAGE_KEYBOARD {
            for usage in [0, 4, 0x45, 0xE0, 255, 256, u16::MAX] {
                assert_eq!(hid_usage_to_evdev(page, usage), None);
                assert_eq!(hid_key_to_evdev(HidKey { page, usage }), None);
            }
        }
    }
    for usage in 256..=u16::MAX {
        assert_eq!(hid_key_to_evdev(HidKey::keyboard(usage)), None);
        assert_eq!(hid_usage_to_evdev(HID_PAGE_KEYBOARD, usage), None);
    }
}

#[test]
fn no_code_and_reserved_usage_have_distinct_meanings() {
    for usage in 0..=3 {
        assert_eq!(hid_key_to_evdev(HidKey::keyboard(usage)), None);
    }
    assert_eq!(EVDEV_KEY_UNKNOWN, 240);
    for usage in [0x82, 0xA0, 0xFF] {
        assert_eq!(hid_key_to_evdev(HidKey::keyboard(usage)), Some(240));
    }
    assert_eq!(evdev_to_hid_usage(0), None);
}

#[test]
fn buttons_preserve_wire_ids_evdev_codes_and_side_indices() {
    let buttons = [
        (MouseButton::Left, 1, 0x110, None),
        (MouseButton::Right, 2, 0x111, None),
        (MouseButton::Middle, 3, 0x112, None),
        (MouseButton::Back, 4, 0x113, Some(1)),
        (MouseButton::Forward, 5, 0x114, Some(2)),
    ];
    for (button, wire_id, evdev, side) in buttons {
        assert_eq!(button as u8, wire_id);
        assert_eq!(button.evdev_code(), evdev);
        assert_eq!(MouseButton::from_evdev_code(evdev), Some(button));
        assert_eq!(button.side_index(), side);
    }
    for code in 0..=u16::MAX {
        if !(0x110..=0x114).contains(&code) {
            assert_eq!(MouseButton::from_evdev_code(code), None);
        }
    }
}

#[test]
fn key_edges_preserve_ipc_down_boolean() {
    for (down, state) in [(true, KeyState::Down), (false, KeyState::Up)] {
        assert_eq!(KeyState::from_down(down), state);
        assert_eq!(state.is_down(), down);
        assert_eq!(state.is_up(), !down);
    }
}

#[test]
fn capture_defaults_to_observe_and_only_committed_remote_suppresses() {
    assert_eq!(CaptureKind::default(), CaptureKind::Observe);
    assert!(!CaptureKind::Observe.is_suppressed());
    assert!(!CaptureKind::Temporary.is_suppressed());
    assert!(CaptureKind::Remote.is_suppressed());
}
