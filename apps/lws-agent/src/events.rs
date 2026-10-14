//! Normalised events → the wire's `LocalEvent` vocabulary.
//!
//! `protocol` depends on `input-model`, so the shared crate cannot own the mapping
//! in either direction; the agent needs this one (capture → `CapturedBatch`) and
//! `lws-core` needs the other (`AgentCommand::Inject` → `protocol::ipc::Inject`).
//! The two live next to the processes that use them and are pure, so both are
//! unit-tested.

use input_model::{InputEvent, KeyState, MouseButton};
use protocol::ipc;

/// One normalised event as `ipc.proto` `LocalEvent`, or `None` when it has no wire
/// form.
///
/// `monotonic_us` is diagnostic: `transport::convert` documents that nothing may
/// derive timing from it.
pub fn to_local_event(event: &InputEvent, monotonic_us: u64) -> Option<ipc::LocalEvent> {
    let ev = match event {
        InputEvent::PointerAbsolute(position) => ipc::local_event::Ev::Pointer(ipc::PointerAbs {
            // The normalised position carries no display id (`input-model`), and the
            // wire's `PointerAbs.display_id` is the receiver's business: it resolves
            // the display from its own zones/geometry. Empty means exactly that
            // (`transport::convert` documents the same for `InputBatch`).
            display_id: String::new(),
            x: position.x,
            y: position.y,
        }),
        // Relative motion is capture-side only and is integrated into an absolute
        // position before it reaches the wire («в протоколе нет и не добавляется»).
        // It is dropped rather than guessed at; the caller counts the drops.
        InputEvent::PointerRelative { .. } => return None,
        InputEvent::Key { key, state } => ipc::local_event::Ev::Key(ipc::HidKey {
            usage_page: u32::from(key.page),
            usage: u32::from(key.usage),
            down: *state == KeyState::Down,
        }),
        InputEvent::Button { button, state } => ipc::local_event::Ev::Button(ipc::ButtonEv {
            button: match button {
                MouseButton::Left => 1,
                MouseButton::Right => 2,
                MouseButton::Middle => 3,
                MouseButton::Back => 4,
                MouseButton::Forward => 5,
            },
            down: *state == KeyState::Down,
        }),
        InputEvent::Scroll { h120, v120 } => ipc::local_event::Ev::Scroll(ipc::ScrollEv {
            h120: *h120,
            v120: *v120,
        }),
    };
    Some(ipc::LocalEvent {
        monotonic_us,
        ev: Some(ev),
    })
}

pub fn edge_counters(counters: input_model::EdgeCounters) -> ipc::EdgeCounters {
    ipc::EdgeCounters {
        keys_down: counters.keys_down,
        keys_up: counters.keys_up,
        buttons_down: counters.buttons_down,
        buttons_up: counters.buttons_up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_model::{HidKey, Position};

    #[test]
    fn key_edges_keep_their_usage_and_state() {
        let down = to_local_event(
            &InputEvent::Key {
                key: HidKey::keyboard(0x04),
                state: KeyState::Down,
            },
            42,
        )
        .expect("a key edge has a wire form");
        assert_eq!(down.monotonic_us, 42);
        match down.ev {
            Some(ipc::local_event::Ev::Key(key)) => {
                assert_eq!(key.usage_page, 7);
                assert_eq!(key.usage, 0x04);
                assert!(key.down);
            }
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn buttons_and_wheel_map_to_the_wire_numbers() {
        let button = to_local_event(
            &InputEvent::Button {
                button: MouseButton::Forward,
                state: KeyState::Up,
            },
            0,
        )
        .expect("a button edge has a wire form");
        match button.ev {
            Some(ipc::local_event::Ev::Button(ev)) => {
                assert_eq!(ev.button, 5);
                assert!(!ev.down);
            }
            other => panic!("expected a button event, got {other:?}"),
        }

        let scroll = to_local_event(
            &InputEvent::Scroll {
                h120: -120,
                v120: 240,
            },
            0,
        )
        .expect("a wheel step has a wire form");
        match scroll.ev {
            Some(ipc::local_event::Ev::Scroll(ev)) => {
                assert_eq!(ev.h120, -120);
                assert_eq!(ev.v120, 240);
            }
            other => panic!("expected a scroll event, got {other:?}"),
        }
    }

    #[test]
    fn relative_motion_never_reaches_the_wire() {
        assert!(
            to_local_event(&InputEvent::PointerRelative { dx: 3, dy: -4 }, 0).is_none(),
            "relative motion is dropped, not guessed at"
        );
        let absolute = to_local_event(&InputEvent::PointerAbsolute(Position::new(1.0, 2.0)), 0)
            .expect("an absolute position has a wire form");
        match absolute.ev {
            Some(ipc::local_event::Ev::Pointer(pointer)) => {
                assert_eq!(pointer.display_id, "");
                assert!((pointer.x - 1.0).abs() < f64::EPSILON);
            }
            other => panic!("expected a pointer event, got {other:?}"),
        }
    }
}
