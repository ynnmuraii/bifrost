use input_model::{HidKey, InputEvent, KeyState, MouseButton, Position};
use platform_win::inject_plan::{account, plan, Record, VirtualDesktop};

const DESKTOP: VirtualDesktop = VirtualDesktop {
    left: -1920,
    top: -1080,
    width: 3840,
    height: 2160,
};

fn key(usage: u16, state: KeyState) -> InputEvent {
    InputEvent::Key {
        key: HidKey::keyboard(usage),
        state,
    }
}

#[test]
fn normalization_reaches_edges_clamps_and_handles_extreme_coordinates() {
    assert_eq!(DESKTOP.normalize(-1920, -1080), (0, 0));
    assert_eq!(DESKTOP.normalize(1919, 1079), (65535, 65535));
    assert_eq!(DESKTOP.normalize(-1921, 1080), (0, 65535));
    assert_eq!(DESKTOP.normalize(1920, -1081), (65535, 0));
    assert_eq!(DESKTOP.normalize(i32::MIN, i32::MAX), (0, 65535));
    assert_eq!(DESKTOP.normalize(i32::MAX, i32::MIN), (65535, 0));
    let extreme = VirtualDesktop {
        left: i32::MIN,
        top: i32::MAX,
        width: i32::MAX,
        height: 2,
    };
    assert_eq!(extreme.normalize(i32::MAX, i32::MIN), (65535, 0));
    let small = VirtualDesktop {
        left: -1,
        top: -2,
        width: 3,
        height: 5,
    };
    assert_eq!(small.normalize(0, 0), (32767, 32767));
}

#[test]
fn degenerate_extents_map_to_center_independently_per_axis() {
    for extent in [i32::MIN, -1, 0, 1] {
        let width = VirtualDesktop {
            width: extent,
            ..DESKTOP
        };
        let height = VirtualDesktop {
            height: extent,
            ..DESKTOP
        };
        for coordinate in [i32::MIN, -1, 0, 1, i32::MAX] {
            assert_eq!(width.normalize(coordinate, DESKTOP.top), (32767, 0));
            assert_eq!(height.normalize(DESKTOP.left, coordinate), (0, 32767));
        }
    }
}

#[test]
fn normalization_is_monotonic_across_negative_origin_desktop() {
    let mut previous = (0, 0);
    for offset in -2..=3842 {
        let current = DESKTOP.normalize(DESKTOP.left + offset, DESKTOP.top + offset);
        assert!(current.0 >= previous.0 && current.1 >= previous.1);
        previous = current;
    }
}

#[test]
fn planning_preserves_key_edges_buttons_motion_and_unmapped_slots() {
    let mut events = vec![
        key(4, KeyState::Down),
        key(4, KeyState::Up),
        key(0xE4, KeyState::Down),
        key(0xE4, KeyState::Up),
    ];
    let mut expected = vec![
        Record::Key {
            scan: 0x1E,
            extended: false,
            up: false,
        },
        Record::Key {
            scan: 0x1E,
            extended: false,
            up: true,
        },
        Record::Key {
            scan: 0x1D,
            extended: true,
            up: false,
        },
        Record::Key {
            scan: 0x1D,
            extended: true,
            up: true,
        },
    ];
    for button in [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ] {
        for state in [KeyState::Down, KeyState::Up] {
            events.push(InputEvent::Button { button, state });
            expected.push(Record::Button {
                button,
                up: state == KeyState::Up,
            });
        }
    }
    events.extend([
        InputEvent::PointerRelative {
            dx: i32::MIN,
            dy: i32::MAX,
        },
        InputEvent::PointerAbsolute(Position::new(1919.0, -1080.0)),
        key(0x48, KeyState::Down),
        InputEvent::Key {
            key: HidKey {
                page: 0x0C,
                usage: 4,
            },
            state: KeyState::Up,
        },
    ]);
    expected.extend([
        Record::MoveRel {
            dx: i32::MIN,
            dy: i32::MAX,
        },
        Record::MoveAbsNorm { x: 65535, y: 0 },
    ]);
    let planned = plan(&events, DESKTOP);
    assert_eq!(planned.records, expected);
    assert_eq!(planned.record_count(), 16);
    assert_eq!(planned.unmapped_count(), 2);
    assert_eq!(planned.motion_count(), 2);
    assert_eq!(planned.owner, (0..16).collect::<Vec<_>>());
    assert_eq!(&planned.per_event[..16], &[1; 16]);
    assert_eq!(&planned.per_event[16..], &[0, 0]);
    assert_eq!(planned.per_event.len(), events.len());
}

#[test]
fn absolute_positions_round_to_nearest_pixel_before_normalization() {
    let desktop = VirtualDesktop {
        left: -1,
        top: -1,
        width: 3,
        height: 3,
    };
    let planned = plan(
        &[
            InputEvent::PointerAbsolute(Position::new(-0.5, 0.5)),
            InputEvent::PointerAbsolute(Position::new(-0.49, 0.49)),
        ],
        desktop,
    );
    assert_eq!(
        planned.records,
        vec![
            Record::MoveAbsNorm { x: 0, y: 65535 },
            Record::MoveAbsNorm { x: 32767, y: 32767 },
        ]
    );
}

#[test]
fn wheel_plans_keep_axis_sign_and_omit_only_zero_axes() {
    for h120 in [i32::MIN, -120, -1, 0, 1, 120, i32::MAX] {
        for v120 in [i32::MIN, -120, -1, 0, 1, 120, i32::MAX] {
            let planned = plan(&[InputEvent::Scroll { h120, v120 }], DESKTOP);
            let count = usize::from(h120 != 0) + usize::from(v120 != 0);
            assert_eq!(planned.record_count(), count);
            assert_eq!(planned.owner, vec![0; count]);
            assert_eq!(planned.per_event, vec![count as u8]);
            assert_eq!(planned.unmapped_count(), usize::from(count == 0));
            assert_eq!(planned.motion_count(), 0);
            for (vertical, delta120) in [(false, h120), (true, v120)] {
                assert_eq!(
                    planned
                        .records
                        .contains(&Record::Wheel { vertical, delta120 }),
                    delta120 != 0
                );
            }
        }
    }
}

#[test]
fn accounting_conserves_events_for_every_accept_count_and_small_batch() {
    // Exhaust every sequence of up to four zero-, one-, and two-record events,
    // including both kinds of zero-record event. No random seed is involved.
    let choices = [
        key(0x48, KeyState::Down),
        InputEvent::Scroll { h120: 0, v120: 0 },
        key(4, KeyState::Down),
        InputEvent::Scroll {
            h120: -120,
            v120: 120,
        },
    ];
    for len in 0..=4u32 {
        for mut encoding in 0..4usize.pow(len) {
            let mut events = Vec::new();
            let mut unmapped = Vec::new();
            for index in 0..len as usize {
                let choice = encoding % 4;
                encoding /= 4;
                events.push(choices[choice]);
                if choice < 2 {
                    unmapped.push(index);
                }
            }
            let planned = plan(&events, DESKTOP);
            assert_eq!(planned.owner.len(), planned.record_count());
            assert_eq!(planned.per_event.len(), events.len());
            for (index, &count) in planned.per_event.iter().enumerate() {
                assert_eq!(
                    planned
                        .owner
                        .iter()
                        .filter(|&&owner| owner == index)
                        .count(),
                    usize::from(count)
                );
            }
            assert_eq!(account(&planned, 0), (0, (0..events.len()).collect()));
            let complete = (events.len() - unmapped.len(), unmapped.clone());
            assert_eq!(account(&planned, planned.record_count()), complete);
            assert_eq!(account(&planned, usize::MAX), complete);
            let mut previous_applied = 0;
            for accepted in 0..=planned.record_count() {
                let (applied, missing) = account(&planned, accepted);
                assert_eq!(applied + missing.len(), events.len());
                assert!(applied >= previous_applied);
                assert!(missing.windows(2).all(|pair| pair[0] < pair[1]));
                assert!(missing.iter().all(|&index| index < events.len()));
                assert!(unmapped.iter().all(|index| missing.contains(index)));
                previous_applied = applied;
            }
        }
    }
}

#[test]
fn partial_wheel_acceptance_leaves_every_later_event_missing() {
    let events = [
        key(4, KeyState::Down),
        InputEvent::Scroll {
            h120: 120,
            v120: -120,
        },
        key(5, KeyState::Down),
        InputEvent::Button {
            button: MouseButton::Left,
            state: KeyState::Down,
        },
    ];
    let planned = plan(&events, DESKTOP);
    assert_eq!(planned.per_event, vec![1, 2, 1, 1]);
    // Only the key and the first wheel axis reached the OS. Neither the other
    // wheel axis nor either later event was accepted.
    assert_eq!(account(&planned, 2), (1, vec![1, 2, 3]));
}
