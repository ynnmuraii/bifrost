use platform_win::event::EmergencyCombo;

#[test]
fn emergency_default_requires_ctrl_alt_shift_f12() {
    let combo = EmergencyCombo::default();
    assert_eq!(combo.vk, 0x7B);
    assert!(combo.ctrl && combo.alt && combo.shift);
}
