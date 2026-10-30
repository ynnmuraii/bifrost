use super::{capture_runs, freeze_capture_run, InputEvent};
use protocol::ipc::{self, local_event::Ev};

fn motion(x: f64, timestamp: u64) -> ipc::LocalEvent {
    ipc::LocalEvent {
        monotonic_us: timestamp,
        ev: Some(Ev::Pointer(ipc::PointerAbs {
            display_id: "source".into(),
            x,
            y: 10.0,
        })),
    }
}

#[test]
fn click_position_and_capture_time_are_frozen_before_later_motion_in_the_same_ipc_batch() {
    let click = ipc::LocalEvent {
        monotonic_us: 12,
        ev: Some(Ev::Button(ipc::ButtonEv {
            button: protocol::wire::ButtonKind::Left as u32,
            down: true,
        })),
    };
    let events = [motion(1.0, 10), motion(2.0, 11), click, motion(3.0, 13)];
    let mut runs = capture_runs(&events);
    let first = runs.next().unwrap();
    let final_run = runs.next().unwrap();
    assert!(runs.next().is_none());
    assert_eq!(first.len(), 3);
    assert_eq!(final_run.len(), 1);
    // These are the target positions after tracking each contiguous motion prefix.
    let before_click = geometry::Position::new("target", 20.0, 30.0);
    let after_click = geometry::Position::new("target", 40.0, 30.0);
    let frozen = freeze_capture_run(first, Some(&before_click));
    let final_frozen = freeze_capture_run(final_run, Some(&after_click));
    assert_eq!(frozen[0].1, Some(10));
    assert_eq!(frozen[1].1, Some(11));
    assert_eq!(frozen[2].1, Some(12));
    assert!(matches!(frozen[1].0, InputEvent::PointerAbsolute(point) if point.x == 20.0));
    assert!(matches!(frozen[2].0, InputEvent::Button { .. }));
    assert!(matches!(final_frozen[0].0, InputEvent::PointerAbsolute(point) if point.x == 40.0));
    assert_eq!(final_frozen[0].1, Some(13));
}

#[test]
fn invalid_events_do_not_shift_capture_metadata_and_zero_is_unknown() {
    let events = [
        ipc::LocalEvent {
            monotonic_us: 99,
            ev: None,
        },
        motion(1.0, 0),
        motion(2.0, 321),
    ];
    let frozen = freeze_capture_run(&events, None);
    assert_eq!(frozen.len(), 2);
    assert_eq!(frozen[0].1, None);
    assert_eq!(frozen[1].1, Some(321));
    assert!(matches!(frozen[1].0, InputEvent::PointerAbsolute(point) if point.x == 2.0));
}

#[test]
fn wheel_key_and_button_are_all_discrete_capture_run_barriers() {
    let discrete = [
        Ev::Scroll(ipc::ScrollEv { h120: 0, v120: 120 }),
        Ev::Key(ipc::HidKey {
            usage_page: 7,
            usage: 4,
            down: false,
        }),
        Ev::Button(ipc::ButtonEv {
            button: 1,
            down: false,
        }),
    ];
    for event in discrete {
        let events = [
            motion(1.0, 1),
            ipc::LocalEvent {
                monotonic_us: 2,
                ev: Some(event),
            },
            motion(3.0, 3),
        ];
        let runs: Vec<_> = capture_runs(&events).collect();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].len(), 2);
        assert_eq!(runs[1].len(), 1);
    }
}
