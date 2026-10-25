//! Outbound classification and motion coalescing: what may be dropped, and what may
//! never be merged across.

use focus_router::{ClaimReason, LeaveReason, Message};
use input_model::{HidKey, InputEvent, KeyState, MouseButton, Position};
use transport::{
    classify, coalesce_motion, is_motion, merge_motion_batches, MessageClass, Outbound,
};

fn abs(x: f64, y: f64) -> InputEvent {
    InputEvent::PointerAbsolute(Position::new(x, y))
}

fn rel(dx: i32, dy: i32) -> InputEvent {
    InputEvent::PointerRelative { dx, dy }
}

fn key(state: KeyState) -> InputEvent {
    InputEvent::Key {
        key: HidKey::keyboard(0x04),
        state,
    }
}

fn button(state: KeyState) -> InputEvent {
    InputEvent::Button {
        button: MouseButton::Left,
        state,
    }
}

fn scroll(v120: i32) -> InputEvent {
    InputEvent::Scroll { h120: 0, v120 }
}

fn batch(events: Vec<InputEvent>) -> Message {
    Message::InputBatch {
        lease_id: [1u8; 16],
        input_batch_seq: 1,
        events,
    }
}

#[test]
fn only_pointer_events_are_motion() {
    assert!(is_motion(&abs(1.0, 2.0)));
    assert!(is_motion(&rel(-3, 4)));

    // Scroll and buttons are discrete edges, not motion: the receiver must see each one.
    assert!(!is_motion(&scroll(120)));
    assert!(!is_motion(&key(KeyState::Down)));
    assert!(!is_motion(&button(KeyState::Up)));
}

#[test]
fn a_batch_is_motion_only_when_every_event_is_motion() {
    assert_eq!(classify(&batch(vec![abs(1.0, 1.0)])), MessageClass::Motion);
    assert_eq!(
        classify(&batch(vec![abs(1.0, 1.0), rel(2, 2)])),
        MessageClass::Motion
    );

    for edge in [key(KeyState::Down), button(KeyState::Up), scroll(120)] {
        assert_eq!(classify(&batch(vec![edge])), MessageClass::Edge);
        assert_eq!(
            classify(&batch(vec![abs(1.0, 1.0), edge])),
            MessageClass::Edge,
            "one edge makes the whole batch guaranteed"
        );
    }

    // An empty batch is not motion, so it can never be dropped.
    assert_eq!(classify(&batch(Vec::new())), MessageClass::Edge);
}

#[test]
fn everything_that_is_not_an_input_batch_is_control() {
    for message in [
        Message::OwnerClaim {
            serial: 1,
            reason: ClaimReason::User,
        },
        Message::Shutdown {
            reason: LeaveReason::Shutdown,
        },
        Message::LeaveAck {
            lease_id: [0u8; 16],
        },
        Message::EnterCommit {
            lease_id: [0u8; 16],
        },
    ] {
        assert_eq!(classify(&message), MessageClass::Control, "{message:?}");
    }
}

#[test]
fn coalescing_never_crosses_an_edge() {
    // A key-up in the middle of a motion run splits it: merging across it would reorder
    // the events the receiver applies.
    let events = vec![
        abs(1.0, 1.0),
        abs(2.0, 2.0),
        key(KeyState::Up),
        abs(3.0, 3.0),
        abs(4.0, 4.0),
    ];
    assert_eq!(
        coalesce_motion(events),
        vec![abs(2.0, 2.0), key(KeyState::Up), abs(4.0, 4.0)]
    );

    // Buttons and scroll split runs the same way.
    let events = vec![rel(1, 1), rel(2, 2), button(KeyState::Down), rel(3, 3)];
    assert_eq!(
        coalesce_motion(events),
        vec![rel(3, 3), button(KeyState::Down), rel(3, 3)]
    );

    let events = vec![rel(1, 1), scroll(120), rel(2, 2)];
    assert_eq!(
        coalesce_motion(events),
        vec![rel(1, 1), scroll(120), rel(2, 2)]
    );
}

#[test]
fn absolute_and_relative_runs_are_never_merged_with_each_other() {
    let events = vec![abs(1.0, 1.0), rel(5, 5), abs(2.0, 2.0)];
    assert_eq!(coalesce_motion(events.clone()), events);
}

#[test]
fn absolute_positions_collapse_to_the_last_and_relative_deltas_sum() {
    assert_eq!(
        coalesce_motion(vec![abs(1.0, 1.0), abs(2.0, 2.0), abs(3.0, 4.0)]),
        vec![abs(3.0, 4.0)]
    );
    assert_eq!(
        coalesce_motion(vec![rel(1, 2), rel(3, 4), rel(-1, -1)]),
        vec![rel(3, 5)]
    );
    // A single event is untouched, and an empty list stays empty.
    assert_eq!(coalesce_motion(vec![abs(1.0, 1.0)]), vec![abs(1.0, 1.0)]);
    assert!(coalesce_motion(Vec::new()).is_empty());
}

#[test]
fn relative_sums_saturate_instead_of_wrapping() {
    // Dropping a delta would lose movement, and wrapping would move the cursor the wrong
    // way, so the sum saturates.
    assert_eq!(
        coalesce_motion(vec![rel(i32::MAX, i32::MAX), rel(1, 1)]),
        vec![rel(i32::MAX, i32::MAX)]
    );
    assert_eq!(
        coalesce_motion(vec![rel(i32::MIN, i32::MIN), rel(-1, -1)]),
        vec![rel(i32::MIN, i32::MIN)]
    );
}

#[test]
fn coalescing_preserves_the_order_of_every_non_motion_event() {
    let events = vec![
        key(KeyState::Down),
        abs(1.0, 1.0),
        abs(2.0, 2.0),
        key(KeyState::Up),
        button(KeyState::Down),
        button(KeyState::Up),
        scroll(120),
        rel(1, 1),
        rel(1, 1),
    ];
    assert_eq!(
        coalesce_motion(events),
        vec![
            key(KeyState::Down),
            abs(2.0, 2.0),
            key(KeyState::Up),
            button(KeyState::Down),
            button(KeyState::Up),
            scroll(120),
            rel(2, 2),
        ]
    );
}

#[test]
fn merge_motion_batches_flattens_and_then_coalesces() {
    assert!(merge_motion_batches(Vec::new()).is_empty());
    assert_eq!(
        merge_motion_batches(vec![Vec::new(), vec![abs(1.0, 1.0)]]),
        vec![abs(1.0, 1.0)]
    );
    // Two contiguous absolute batches collapse: the newest position supersedes the older.
    assert_eq!(
        merge_motion_batches(vec![vec![abs(1.0, 1.0)], vec![abs(2.0, 2.0)]]),
        vec![abs(2.0, 2.0)]
    );
    assert_eq!(
        merge_motion_batches(vec![vec![rel(1, 1)], vec![rel(2, 2)]]),
        vec![rel(3, 3)]
    );
    // A batch with an edge keeps the two motions apart.
    assert_eq!(
        merge_motion_batches(vec![
            vec![abs(1.0, 1.0)],
            vec![key(KeyState::Down)],
            vec![abs(2.0, 2.0)],
        ]),
        vec![abs(1.0, 1.0), key(KeyState::Down), abs(2.0, 2.0)]
    );
}

#[test]
fn outbound_of_routes_motion_to_the_droppable_path() {
    let motion = batch(vec![abs(1.0, 1.0), rel(1, 1)]);
    let outbound = transport::queue::outbound_of(motion.clone());
    assert_eq!(
        outbound,
        Outbound::Motion {
            lease_id: [1u8; 16],
            events: vec![abs(1.0, 1.0), rel(1, 1)],
        }
    );
    assert_eq!(
        outbound.motion_events(),
        Some([abs(1.0, 1.0), rel(1, 1)].as_slice())
    );

    let with_edge = batch(vec![abs(1.0, 1.0), key(KeyState::Up)]);
    let outbound = transport::queue::outbound_of(with_edge.clone());
    assert_eq!(outbound, Outbound::Guaranteed(with_edge));
    assert_eq!(
        outbound.motion_events(),
        None,
        "an edge batch is never droppable"
    );

    let control = Message::OwnerClaim {
        serial: 2,
        reason: ClaimReason::Takeover,
    };
    let outbound = transport::queue::outbound_of(control.clone());
    assert_eq!(outbound, Outbound::Guaranteed(control));
    assert_eq!(outbound.motion_events(), None);
}

/// A wheel turn is the input whose loss leaves no trace on either side, so it may be neither
/// dropped nor merged: every detent the source scrolled must reach the receiver as its own
/// event, on the guaranteed path.
#[test]
fn a_wheel_batch_travels_guaranteed_and_keeps_every_detent() {
    let events = vec![
        scroll(120),
        scroll(120),
        InputEvent::Scroll {
            h120: -120,
            v120: 0,
        },
        scroll(-120),
    ];
    let message = batch(events.clone());
    assert_eq!(
        classify(&message),
        MessageClass::Edge,
        "a wheel-only batch must never ride the droppable motion path"
    );
    let outbound = transport::queue::outbound_of(message);
    match &outbound {
        Outbound::Guaranteed(Message::InputBatch { events: kept, .. }) => {
            assert_eq!(
                kept, &events,
                "every detent travels, in order, and none is merged away"
            );
        }
        other => panic!("a wheel batch must travel guaranteed, got {other:?}"),
    }
    assert_eq!(
        outbound.motion_events(),
        None,
        "the droppable path must never see a wheel"
    );

    // The coalescer is the only place that could merge them, and it must not: two detents in
    // a row are two rotations, not one position that the newest supersedes.
    assert_eq!(coalesce_motion(events.clone()), events);
    assert_eq!(
        coalesce_motion(vec![abs(1.0, 1.0), scroll(120), scroll(120), abs(2.0, 2.0)]),
        vec![abs(1.0, 1.0), scroll(120), scroll(120), abs(2.0, 2.0)],
        "a wheel run splits the motion runs and is never merged across"
    );
}

/// A motion-only `InputBatch` travels on the droppable path and is rebuilt for the wire with
/// the very lease it carried. The receiver validates that lease on every batch, so rebuilding
/// it with the empty id turned every coalesced pointer frame into a `LeaseMismatch` while the
/// key/button batches (guaranteed, lease intact) still arrived.
#[test]
fn a_motion_only_batch_keeps_its_lease_through_the_droppable_path() {
    let lease_id = [7u8; 16];
    let motion = Message::InputBatch {
        lease_id,
        input_batch_seq: 9,
        events: vec![abs(1.0, 1.0), rel(2, 3)],
    };

    let outbound = transport::queue::outbound_of(motion);
    assert_eq!(
        outbound.motion_lease(),
        Some(lease_id),
        "the droppable path must carry the lease, not drop it with the events"
    );

    let rebuilt = transport::queue::motion_batch(
        outbound.motion_lease().expect("motion"),
        outbound.motion_events().expect("motion").to_vec(),
    );
    assert_eq!(
        rebuilt,
        Message::InputBatch {
            lease_id,
            input_batch_seq: 0,
            events: vec![abs(1.0, 1.0), rel(2, 3)],
        },
        "the rebuilt batch must keep the lease its motion belonged to"
    );
}

#[test]
fn updates_never_enter_the_coalescible_motion_path() {
    for item in [
        Outbound::UpdateRequest(protocol::wire::UpdateRequest {
            target_version: "1.2.3".into(),
        }),
        Outbound::UpdateResult(protocol::wire::UpdateResult {
            target_version: "1.2.3".into(),
            status: "downloading".into(),
            detail: String::new(),
        }),
    ] {
        assert!(item.motion_events().is_none());
        assert!(item.motion_lease().is_none());
    }
}
