#![cfg(target_os = "linux")]

use input_model::{HidKey, KeyState, MouseButton, Position};
use platform_x11::{capture::*, keymap::*, PressedLedger};
use x11rb::protocol::xinput::Fp3232;

#[test]
fn evdev_keycodes_round_trip_across_the_x11_range() {
    for evdev in 1..=247 {
        let keycode = evdev_to_x11_keycode(evdev).unwrap();
        assert_eq!(x11_keycode_to_evdev(keycode), Some(evdev));
    }
    for evdev in 248..=u16::MAX {
        assert_eq!(evdev_to_x11_keycode(evdev), None);
    }
    for keycode in 0..8 {
        assert_eq!(x11_keycode_to_evdev(keycode), None);
        assert_eq!(x11_keycode_to_hid_key(keycode), None);
    }
    assert_eq!(x11_keycode_to_hid_key(8), None);
}

#[test]
fn physical_keys_and_emergency_modifiers_have_stable_codes() {
    for (usage, code) in [
        (0x04, 38),
        (0x45, 96),
        (0xe0, 37),
        (0xe4, 105),
        (0xe1, 50),
        (0xe5, 62),
        (0xe2, 64),
        (0xe6, 108),
    ] {
        assert_eq!(hid_key_to_x11_keycode(HidKey::keyboard(usage)), Some(code));
        assert_eq!(x11_keycode_to_hid_key(code), Some(HidKey::keyboard(usage)));
    }
    for page in [0, 1, 6, 8, 0x0c, u16::MAX] {
        assert_eq!(hid_key_to_x11_keycode(HidKey { page, usage: 0x04 }), None);
    }
    assert_eq!(EMERGENCY_F12_X11_KEYCODE, 96);
    for code in 0..=u8::MAX {
        assert_eq!(is_ctrl_keycode(code), [37, 105].contains(&code));
        assert_eq!(is_alt_keycode(code), [64, 108].contains(&code));
        assert_eq!(is_shift_keycode(code), [50, 62].contains(&code));
    }
}

#[test]
fn mouse_buttons_and_wheel_axes_never_overlap() {
    for (button, code) in [
        (MouseButton::Left, 1),
        (MouseButton::Middle, 2),
        (MouseButton::Right, 3),
        (MouseButton::Back, 8),
        (MouseButton::Forward, 9),
    ] {
        assert_eq!(mouse_button_to_x11_button(button), code);
        assert_eq!(x11_button_to_mouse_button(code), Some(button));
        assert_eq!(wheel_from_x11_button(code), None);
    }
    for (code, delta) in [(4, (0, 120)), (5, (0, -120)), (6, (-120, 0)), (7, (120, 0))] {
        assert_eq!(wheel_from_x11_button(code), Some(delta));
        assert_eq!(x11_button_to_mouse_button(code), None);
    }
    for code in [0, 10, 255] {
        assert_eq!(x11_button_to_mouse_button(code), None);
        assert_eq!(wheel_from_x11_button(code), None);
    }
}

#[test]
fn emergency_chord_requires_all_three_modifiers_on_either_side() {
    for ctrl in [37, 105] {
        for alt in [64, 108] {
            for shift in [50, 62] {
                for mask in 0..8 {
                    let mut state = ModifierState::new();
                    for (bit, code) in [(1, ctrl), (2, alt), (4, shift)] {
                        if mask & bit != 0 {
                            state.observe(code, KeyState::Down);
                        }
                    }
                    assert_eq!(state.emergency_chord(), mask == 7);
                    assert_eq!(state.is_clear(), mask == 0);
                }
            }
        }
    }
}

#[test]
fn releasing_either_modifier_side_clears_it_and_other_keys_are_ignored() {
    for pair in [[37, 105], [64, 108], [50, 62]] {
        for released in pair {
            let mut state = ModifierState::new();
            for code in pair {
                state.observe(code, KeyState::Down);
            }
            state.observe(released, KeyState::Up);
            assert!(state.is_clear());
        }
    }
    for held in [false, true] {
        let mut state = ModifierState {
            ctrl: held,
            alt: held,
            shift: held,
        };
        let before = state;
        for code in 0..=u8::MAX {
            if ![37, 105, 64, 108, 50, 62].contains(&code) {
                state.observe(code, KeyState::Down);
                state.observe(code, KeyState::Up);
                assert_eq!(state, before);
            }
        }
    }
}

#[test]
fn ledger_tracks_last_recorded_edges_in_sorted_independent_sets() {
    let mut ledger = PressedLedger::new();
    assert!(ledger.is_empty());
    assert_eq!(ledger.len(), 0);
    for code in [105, 8, 37, 8] {
        ledger.note_key(code, KeyState::Down);
    }
    for code in [9, 1, 8, 1] {
        ledger.note_button(code, KeyState::Down);
    }
    assert_eq!(ledger.keys(), [8, 37, 105]);
    assert_eq!(ledger.buttons(), [1, 8, 9]);
    assert_eq!(ledger.len(), 6);
    ledger.note_key(8, KeyState::Up);
    ledger.note_key(8, KeyState::Up);
    ledger.note_button(99, KeyState::Up);
    assert_eq!(ledger.keys(), [37, 105]);
    assert_eq!(ledger.buttons(), [1, 8, 9]);
    ledger.note_key(8, KeyState::Down);
    assert_eq!(ledger.keys(), [8, 37, 105]);
    ledger.clear();
    ledger.clear();
    assert!(ledger.is_empty());
    assert_eq!(ledger.len(), 0);
    assert!(ledger.keys().is_empty() && ledger.buttons().is_empty());
}

#[test]
fn completed_wheel_edges_leave_no_pressed_entry() {
    let mut ledger = PressedLedger::new();
    ledger.note_key(38, KeyState::Down);
    for button in 4..=7 {
        ledger.note_button(button, KeyState::Down);
        assert_eq!(ledger.buttons(), [button]);
        ledger.note_button(button, KeyState::Up);
        assert!(ledger.buttons().is_empty());
    }
    assert_eq!(ledger.keys(), [38]);
    assert_eq!(ledger.len(), 1);
}

#[test]
fn xtest_device_names_are_case_insensitive_byte_substrings() {
    for name in [
        b"Virtual core XTEST pointer".as_slice(),
        b"xtest keyboard",
        b"prefix-XtEsT-suffix",
        b"\xffXTEST",
    ] {
        assert!(name_reports_xtest(name));
    }
    for name in [b"".as_slice(), b"XTES", b"physical keyboard", b"X TEST"] {
        assert!(!name_reports_xtest(name));
    }
}

#[test]
fn fixed_point_motion_preserves_negative_fractions() {
    for (integral, frac, expected) in [
        (0, 0, 0.0),
        (0, 0x8000_0000, 0.5),
        (-1, 0x8000_0000, -0.5),
        (-2, 0xc000_0000, -1.25),
        (i32::MIN, 0, -2147483648.0),
    ] {
        assert_eq!(fp3232_to_f64(Fp3232 { integral, frac }), expected);
    }
}

#[test]
fn sparse_valuators_use_packed_values_across_mask_words() {
    let values = [10, 20, 30, 40].map(|integral| Fp3232 { integral, frac: 0 });
    let mask = [1 | (1 << 31), 1 | (1 << 3)];
    for (index, expected) in [(0, 10.0), (31, 20.0), (32, 30.0), (35, 40.0)] {
        assert_eq!(valuator_value(&mask, &values, index), Some(expected));
    }
    for index in [1, 30, 33, 64, usize::MAX] {
        assert_eq!(valuator_value(&mask, &values, index), None);
    }
    assert_eq!(valuator_value(&mask, &values[..3], 35), None);
    assert_eq!(motion_delta(&[2], &values[..1]), (0.0, 10.0));
    assert_eq!(motion_delta(&[3], &values[..2]), (10.0, 20.0));
    assert_eq!(motion_delta(&[], &[]), (0.0, 0.0));
}

#[test]
fn recenter_threshold_is_strict_on_each_axis() {
    let centre = screen_centre(1919, 1079);
    assert_eq!(centre, Position::new(959.5, 539.5));
    assert_eq!(screen_centre(0, 0), Position::new(0.0, 0.0));
    for offset in [-129.0_f64, -128.0, 0.0, 128.0, 129.0] {
        for position in [
            Position::new(centre.x + offset, centre.y),
            Position::new(centre.x, centre.y + offset),
        ] {
            assert_eq!(
                needs_recenter(position, centre, RECENTER_THRESHOLD_PX),
                offset.abs() > 128.0
            );
        }
    }
}

#[test]
fn captured_positions_clamp_nonfinite_values_and_zero_extents() {
    for extent in [0, 1, 1920, u16::MAX] {
        for value in [
            f64::NAN,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
            0.5,
            1919.0,
            f64::INFINITY,
        ] {
            let actual = clamp_position(Position::new(value, value), extent, extent);
            assert!(actual.x.is_finite() && actual.y.is_finite());
            assert!((0.0..=f64::from(extent.saturating_sub(1))).contains(&actual.x));
            assert_eq!(actual.x, actual.y);
        }
    }
    assert_eq!(
        clamp_position(Position::new(f64::NAN, f64::INFINITY), 100, 50),
        Position::new(0.0, 49.0)
    );
    assert_eq!(
        clamp_position(Position::new(-1.0, 3.25), 100, 50),
        Position::new(0.0, 3.25)
    );
}
