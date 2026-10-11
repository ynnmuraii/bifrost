use input_model::{HidKey, InputEvent, KeyState, MouseButton, Position};
use platform_win::ledger::PressedLedger;

fn key(key: HidKey, state: KeyState) -> InputEvent {
    InputEvent::Key { key, state }
}

fn button(button: MouseButton, state: KeyState) -> InputEvent {
    InputEvent::Button { button, state }
}

#[test]
fn repeated_edges_are_idempotent_and_other_identities_are_untouched() {
    let a = HidKey::keyboard(4);
    let b = HidKey::keyboard(5);
    let other_page = HidKey {
        page: 0x0C,
        usage: 4,
    };
    let mut ledger = PressedLedger::new();
    assert!(ledger.is_empty());
    ledger.note(&key(a, KeyState::Up));
    ledger.note(&button(MouseButton::Left, KeyState::Up));
    assert!(ledger.is_empty());
    for _ in 0..3 {
        ledger.note(&key(a, KeyState::Down));
        ledger.note(&key(other_page, KeyState::Down));
        ledger.note(&button(MouseButton::Left, KeyState::Down));
    }
    assert_eq!(ledger.key_count(), 2);
    assert_eq!(ledger.button_count(), 1);
    ledger.note(&key(b, KeyState::Up));
    ledger.note(&button(MouseButton::Forward, KeyState::Up));
    assert!(ledger.holds_key(a));
    assert!(ledger.holds_key(other_page));
    assert!(ledger.holds_button(MouseButton::Left));
    for _ in 0..3 {
        ledger.note(&key(a, KeyState::Up));
        ledger.note(&button(MouseButton::Left, KeyState::Up));
    }
    assert!(!ledger.holds_key(a));
    assert!(!ledger.holds_key(b));
    assert!(ledger.holds_key(other_page));
    assert!(!ledger.holds_button(MouseButton::Left));
    assert_eq!(ledger.key_count(), 1);
    assert_eq!(ledger.button_count(), 0);
    ledger.note(&key(other_page, KeyState::Up));
    assert!(ledger.is_empty());
}

#[test]
fn last_edge_controls_each_entry_and_independent_edges_commute() {
    let a = HidKey::keyboard(4);
    let b = HidKey::keyboard(5);
    for (first, last, held) in [
        (KeyState::Down, KeyState::Up, false),
        (KeyState::Up, KeyState::Down, true),
    ] {
        let mut ledger = PressedLedger::new();
        ledger.note(&key(a, first));
        ledger.note(&key(a, last));
        ledger.note(&button(MouseButton::Back, first));
        ledger.note(&button(MouseButton::Back, last));
        assert_eq!(ledger.holds_key(a), held);
        assert_eq!(ledger.holds_button(MouseButton::Back), held);
    }
    let events = [
        key(a, KeyState::Down),
        key(b, KeyState::Up),
        button(MouseButton::Back, KeyState::Down),
    ];
    let mut forward = PressedLedger::new();
    let mut reverse = PressedLedger::new();
    forward.mark_held(b);
    reverse.mark_held(b);
    for event in &events {
        forward.note(event);
    }
    for event in events.iter().rev() {
        reverse.note(event);
    }
    assert_eq!(forward, reverse);
    assert!(forward.holds_key(a));
    assert!(!forward.holds_key(b));
    assert!(forward.holds_button(MouseButton::Back));
}

#[test]
fn pointer_and_scroll_events_never_change_pressed_state() {
    for held in [false, true] {
        let mut ledger = PressedLedger::new();
        if held {
            ledger.mark_held(HidKey::keyboard(4));
            ledger.mark_held_button(MouseButton::Forward);
        }
        let before = ledger.clone();
        for event in [
            InputEvent::PointerRelative {
                dx: i32::MIN,
                dy: i32::MAX,
            },
            InputEvent::PointerAbsolute(Position::new(-200.5, 300.5)),
            InputEvent::Scroll {
                h120: -120,
                v120: 240,
            },
        ] {
            ledger.note(&event);
            assert_eq!(ledger, before);
        }
    }
}

#[test]
fn draining_releases_only_held_entries_in_stable_order_and_allows_rearming() {
    let a = HidKey::keyboard(4);
    let z = HidKey::keyboard(0x1D);
    let media = HidKey {
        page: 0x0C,
        usage: 1,
    };
    let mut ledger = PressedLedger::new();
    for held in [media, z, a, z] {
        ledger.mark_held(held);
    }
    for held in [
        MouseButton::Forward,
        MouseButton::Left,
        MouseButton::Back,
        MouseButton::Left,
    ] {
        ledger.mark_held_button(held);
    }
    ledger.note(&button(MouseButton::Back, KeyState::Up));
    let batch = ledger.take_all();
    assert!(!batch.is_empty());
    assert_eq!(batch.len(), 5);
    assert_eq!(batch.keys, vec![a, z, media]);
    assert_eq!(batch.buttons, vec![MouseButton::Left, MouseButton::Forward]);
    assert_eq!(
        batch.to_events(),
        vec![
            key(a, KeyState::Up),
            key(z, KeyState::Up),
            key(media, KeyState::Up),
            button(MouseButton::Left, KeyState::Up),
            button(MouseButton::Forward, KeyState::Up),
        ]
    );
    assert!(ledger.is_empty());
    assert_eq!(ledger.key_count(), 0);
    assert_eq!(ledger.button_count(), 0);
    let empty = ledger.take_all();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(empty.to_events().is_empty());

    // A caller can restore just the entries whose release failed.
    ledger.mark_held(z);
    ledger.mark_held_button(MouseButton::Forward);
    assert_eq!(
        ledger.take_all().to_events(),
        vec![
            key(z, KeyState::Up),
            button(MouseButton::Forward, KeyState::Up)
        ]
    );
}
