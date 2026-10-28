//! XTest-based injection (KVM-012).
//!
//! [`X11Injector`] is a thin, honest wrapper around the XTest extension:
//!
//! * keys are pressed/released with `XTestFakeKeyEvent` using the X11 keycode of
//!   the current XKB keymap (ADR-006, see [`crate::keymap`]),
//! * buttons use the core button numbers 1..3/8/9,
//! * wheel movement is one press/release pair per 120-unit notch on buttons
//!   4/5 (vertical) and 6/7 (horizontal), with the sub-notch remainder kept
//!   between calls so a slow stream of `±40` still produces notches,
//! * absolute pointer moves use `XTestFakeMotionEvent` with the root window and
//!   root coordinates.
//!
//! Everything that was actually pressed is recorded in a [`PressedLedger`]. That
//! ledger is the recovery path: [`X11Injector::release_all`] releases exactly the
//! keys and buttons this process pushed, so a transfer that dies halfway (or an
//! emergency abort) never leaves the remote desktop with a stuck key.

use std::collections::HashSet;

use input_model::{InputEvent, KeyState, Position};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keymap::{
    hid_key_to_x11_keycode, mouse_button_to_x11_button, WHEEL_UNITS_PER_NOTCH,
    X11_BUTTON_WHEEL_DOWN, X11_BUTTON_WHEEL_LEFT, X11_BUTTON_WHEEL_RIGHT, X11_BUTTON_WHEEL_UP,
};
use crate::X11Error;

/// Upper bound on wheel notches synthesised for a single `Scroll` event. A peer
/// that asks for thousands of notches in one event would otherwise stall the
/// injector inside a loop; the remainder stays in the accumulator and drains on
/// the following events.
const MAX_NOTCHES_PER_EVENT: i32 = 32;

/// XTest `deviceid` argument meaning "use the default (core) device".
const XTEST_DEFAULT_DEVICE: u8 = 0;

/// The outcome of one injection batch.
///
/// Mirrors the adapter contract: a batch is applied event by event, and a partial
/// failure reports how many events made it *and* the error, so the caller can
/// decide to abort without replaying the whole batch (which would double-apply the
/// events that already landed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InjectResult {
    /// Number of events from the batch that were consumed, in order.
    ///
    /// A `Scroll` whose 120-unit remainder has not yet reached a whole notch is
    /// consumed (the remainder is retained for the next event) without sending a
    /// request, and still counts here — nothing is lost either way.
    pub applied_count: usize,
    /// `Some` when the batch stopped early (or the flush failed); the failing
    /// event and everything after it were not applied.
    pub os_error: Option<String>,
    /// The pointer position the server reports after a batch that contained an
    /// absolute move. `None` when the batch moved nothing, or when the batch
    /// failed before the position could be read back.
    pub actual: Option<Position>,
}

/// Which keys and buttons this process has pushed and not yet released.
///
/// Invariants:
///
/// * a keycode/button is present iff the last successful injection for it was a
///   press,
/// * an entry only leaves the set on a *successful* release (a failed release
///   must stay, or the key stays down on the server forever),
/// * wheel press/release pairs are instantaneous, so a complete notch leaves no
///   trace — but a wheel button whose release failed stays in the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PressedLedger {
    keys: HashSet<u8>,
    buttons: HashSet<u8>,
}

impl PressedLedger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a key edge.
    pub fn note_key(&mut self, keycode: u8, state: KeyState) {
        if state.is_down() {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
    }

    /// Records a button edge.
    pub fn note_button(&mut self, button: u8, state: KeyState) {
        if state.is_down() {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }

    /// Keycodes currently held down, ascending. Deterministic, unlike the set.
    pub fn keys(&self) -> Vec<u8> {
        let mut keys: Vec<u8> = self.keys.iter().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Button numbers currently held down, ascending.
    pub fn buttons(&self) -> Vec<u8> {
        let mut buttons: Vec<u8> = self.buttons.iter().copied().collect();
        buttons.sort_unstable();
        buttons
    }

    /// `true` when nothing is held down.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// Total number of held keys and buttons.
    pub fn len(&self) -> usize {
        self.keys.len() + self.buttons.len()
    }

    /// Forgets every entry without touching the server. Only for tests and for
    /// callers that released the input by other means.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
    }
}

/// XTest-backed input injector.
pub struct X11Injector {
    conn: RustConnection,
    root: Window,
    root_width: u16,
    root_height: u16,
    xtest_version: (u8, u16),
    ledger: PressedLedger,
    scroll_remainder_h: i32,
    scroll_remainder_v: i32,
}

impl X11Injector {
    /// Connects to the display named by `$DISPLAY` and verifies that XTest is
    /// available.
    ///
    /// Fails when there is no usable display or when the server does not offer the
    /// XTest extension, since injection is then impossible.
    pub fn new() -> Result<Self, X11Error> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        let geometry = conn.get_geometry(root)?.reply()?;
        let version = conn.xtest_get_version(2, 2)?.reply()?;
        Ok(Self {
            conn,
            root,
            root_width: geometry.width,
            root_height: geometry.height,
            xtest_version: (version.major_version, version.minor_version),
            ledger: PressedLedger::new(),
            scroll_remainder_h: 0,
            scroll_remainder_v: 0,
        })
    }

    /// The XTest version advertised by the server (`major`, `minor`).
    pub fn xtest_version(&self) -> (u8, u16) {
        self.xtest_version
    }

    /// The root window used for absolute motion.
    pub fn root(&self) -> Window {
        self.root
    }

    /// Root window size in pixels.
    pub fn root_size(&self) -> (u16, u16) {
        (self.root_width, self.root_height)
    }

    /// The pressed-state ledger.
    pub fn ledger(&self) -> &PressedLedger {
        &self.ledger
    }

    /// Keycodes currently held down, ascending.
    pub fn pressed_keys(&self) -> Vec<u8> {
        self.ledger.keys()
    }

    /// Buttons currently held down, ascending.
    pub fn pressed_buttons(&self) -> Vec<u8> {
        self.ledger.buttons()
    }

    /// `true` when nothing this injector pushed is still held down.
    pub fn is_empty(&self) -> bool {
        self.ledger.is_empty()
    }

    /// Sends every buffered request to the server.
    pub fn flush(&mut self) -> Result<(), X11Error> {
        self.conn.flush()?;
        Ok(())
    }

    /// The current pointer position, read back from the server.
    pub fn pointer_position(&self) -> Result<Position, X11Error> {
        let reply = self.conn.query_pointer(self.root)?.reply()?;
        Ok(Position::new(
            f64::from(reply.root_x),
            f64::from(reply.root_y),
        ))
    }

    /// Applies a batch of normalized events in order.
    ///
    /// Stops at the first event that could not be pushed and reports it; events
    /// before it are already on the wire and are *not* rolled back or replayed.
    pub fn inject(&mut self, events: &[InputEvent]) -> InjectResult {
        if let Err(error) = self.refresh_geometry() {
            return InjectResult {
                applied_count: 0,
                os_error: Some(error.to_string()),
                actual: None,
            };
        }
        let mut applied_count = 0usize;
        let mut os_error = None;
        let mut moved = false;
        for event in events {
            if matches!(event, InputEvent::PointerAbsolute(_)) {
                moved = true;
            }
            match self.apply(event) {
                Ok(()) => applied_count += 1,
                Err(err) => {
                    os_error = Some(err.to_string());
                    break;
                }
            }
        }
        if let Err(err) = self.flush() {
            os_error.get_or_insert_with(|| err.to_string());
        }
        let actual = if moved && os_error.is_none() {
            self.pointer_position().ok()
        } else {
            None
        };
        InjectResult {
            applied_count,
            os_error,
            actual,
        }
    }

    /// ConfigureNotify keeps the clamp current without a round trip per input batch.
    fn refresh_geometry(&mut self) -> Result<(), X11Error> {
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                x11rb::protocol::Event::ConfigureNotify(configure)
                    if configure.window == self.root =>
                {
                    self.root_width = configure.width;
                    self.root_height = configure.height;
                }
                x11rb::protocol::Event::Error(error) => {
                    return Err(x11rb::errors::ReplyError::X11Error(error).into());
                }
                // This private connection selects only root STRUCTURE_NOTIFY; capture
                // and clipboard own separate connections, so no input/selection is lost.
                _ => {}
            }
        }
        Ok(())
    }

    /// Releases every key and button still held down by this injector.
    ///
    /// This is the watchdog/recovery path and must be callable at any time,
    /// including right after a failed batch. Every entry is attempted even if one
    /// release fails (the first error is reported); entries whose release failed
    /// stay in the ledger so a later attempt can retry them.
    pub fn release_all(&mut self) -> InjectResult {
        // A fraction of a notch left over from one lease must not scroll in the next one.
        self.scroll_remainder_h = 0;
        self.scroll_remainder_v = 0;
        let keys = self.ledger.keys();
        let buttons = self.ledger.buttons();
        let mut applied_count = 0usize;
        let mut os_error = None;
        for keycode in keys {
            match self.fake_key(keycode, KeyState::Up) {
                Ok(()) => applied_count += 1,
                Err(err) => {
                    os_error.get_or_insert_with(|| err.to_string());
                }
            }
        }
        for button in buttons {
            match self.fake_button(button, KeyState::Up) {
                Ok(()) => applied_count += 1,
                Err(err) => {
                    os_error.get_or_insert_with(|| err.to_string());
                }
            }
        }
        if let Err(err) = self.flush() {
            os_error.get_or_insert_with(|| err.to_string());
        }
        InjectResult {
            applied_count,
            os_error,
            actual: None,
        }
    }

    /// Applies exactly one event, updating the ledger on success.
    fn apply(&mut self, event: &InputEvent) -> Result<(), X11Error> {
        match *event {
            InputEvent::PointerAbsolute(position) => self.fake_absolute_motion(position),
            InputEvent::Key { key, state } => {
                let keycode = hid_key_to_x11_keycode(key).ok_or_else(|| {
                    X11Error::Internal(format!(
                        "no X11 keycode for HID usage 0x{:02X} on page 0x{:02X}",
                        key.usage, key.page
                    ))
                })?;
                self.fake_key(keycode, state)
            }
            InputEvent::Button { button, state } => {
                self.fake_button(mouse_button_to_x11_button(button), state)
            }
            InputEvent::Scroll { h120, v120 } => self.fake_scroll(h120, v120),
            InputEvent::PointerRelative { .. } => Err(X11Error::RelativePointerUnsupported),
        }
    }

    fn fake_absolute_motion(&self, position: Position) -> Result<(), X11Error> {
        let x = clamp_coordinate(position.x, self.root_width);
        let y = clamp_coordinate(position.y, self.root_height);
        self.conn.xtest_fake_input(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            x11rb::CURRENT_TIME,
            self.root,
            x,
            y,
            XTEST_DEFAULT_DEVICE,
        )?;
        Ok(())
    }

    fn fake_key(&mut self, keycode: u8, state: KeyState) -> Result<(), X11Error> {
        let type_ = if state.is_down() {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.conn.xtest_fake_input(
            type_,
            keycode,
            x11rb::CURRENT_TIME,
            x11rb::NONE,
            0,
            0,
            XTEST_DEFAULT_DEVICE,
        )?;
        self.ledger.note_key(keycode, state);
        Ok(())
    }

    fn fake_button(&mut self, button: u8, state: KeyState) -> Result<(), X11Error> {
        let type_ = if state.is_down() {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.conn.xtest_fake_input(
            type_,
            button,
            x11rb::CURRENT_TIME,
            x11rb::NONE,
            0,
            0,
            XTEST_DEFAULT_DEVICE,
        )?;
        self.ledger.note_button(button, state);
        Ok(())
    }

    /// Turns 120-unit wheel deltas into press/release pairs, keeping the
    /// sub-notch remainder for the next call.
    fn fake_scroll(&mut self, h120: i32, v120: i32) -> Result<(), X11Error> {
        self.scroll_remainder_h = self.scroll_remainder_h.saturating_add(h120);
        self.scroll_remainder_v = self.scroll_remainder_v.saturating_add(v120);
        let horizontal = take_notches(&mut self.scroll_remainder_h);
        let vertical = take_notches(&mut self.scroll_remainder_v);
        let (h_button, v_button) = (
            if horizontal >= 0 {
                X11_BUTTON_WHEEL_RIGHT
            } else {
                X11_BUTTON_WHEEL_LEFT
            },
            if vertical >= 0 {
                X11_BUTTON_WHEEL_UP
            } else {
                X11_BUTTON_WHEEL_DOWN
            },
        );
        for _ in 0..vertical.unsigned_abs() {
            self.wheel_notch(v_button)?;
        }
        for _ in 0..horizontal.unsigned_abs() {
            self.wheel_notch(h_button)?;
        }
        Ok(())
    }

    fn wheel_notch(&mut self, button: u8) -> Result<(), X11Error> {
        self.fake_button(button, KeyState::Down)?;
        self.fake_button(button, KeyState::Up)
    }
}

/// Consumes whole 120-unit notches out of an accumulator, leaving the remainder
/// (and anything beyond [`MAX_NOTCHES_PER_EVENT`]) in place.
pub(crate) fn take_notches(remainder: &mut i32) -> i32 {
    let notches =
        (*remainder / WHEEL_UNITS_PER_NOTCH).clamp(-MAX_NOTCHES_PER_EVENT, MAX_NOTCHES_PER_EVENT);
    *remainder -= notches * WHEEL_UNITS_PER_NOTCH;
    notches
}

/// Clamps a logical coordinate into the root window and to the XTest `i16` range.
///
/// `NaN` becomes 0 and infinities saturate to an edge, so the injected coordinate
/// is always a valid absolute position inside the root; the server would clamp
/// anyway, but a defined value is what [`InjectResult::actual`] is compared with.
pub(crate) fn clamp_coordinate(value: f64, extent: u16) -> i16 {
    let max = f64::from(extent.saturating_sub(1).min(i16::MAX as u16));
    if value.is_nan() {
        return 0;
    }
    value.clamp(0.0, max).round() as i16
}

#[cfg(test)]
mod tests {
    use super::{clamp_coordinate, take_notches};

    #[test]
    fn root_resize_updates_the_existing_injectors_clamp_without_input() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{
            ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
            EventMask, WindowClass,
        };
        let Ok((conn, screen)) = x11rb::connect(None) else {
            eprintln!("requires an isolated Xvfb display");
            return;
        };
        let window = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            conn.setup().roots[screen].root,
            0,
            0,
            320,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .unwrap()
        .check()
        .unwrap();
        // A private window stands in for root: no desktop resize or synthetic input.
        conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )
        .unwrap()
        .check()
        .unwrap();
        let mut injector = super::X11Injector {
            conn,
            root: window,
            root_width: 320,
            root_height: 200,
            xtest_version: (2, 2),
            ledger: super::PressedLedger::new(),
            scroll_remainder_h: 0,
            scroll_remainder_v: 0,
        };
        for (width, height) in [(800, 600), (240, 160)] {
            injector
                .conn
                .configure_window(
                    window,
                    &ConfigureWindowAux::new().width(width).height(height),
                )
                .unwrap()
                .check()
                .unwrap();
            assert!(injector.inject(&[]).os_error.is_none());
            assert_eq!(injector.root_size(), (width as u16, height as u16));
            assert_eq!(
                clamp_coordinate(799.0, injector.root_size().0),
                width as i16 - 1
            );
        }
        injector
            .conn
            .destroy_window(window)
            .unwrap()
            .check()
            .unwrap();
    }

    #[test]
    fn geometry_refresh_reports_unchecked_request_errors_without_input() {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        let Ok(mut injector) = super::X11Injector::new() else {
            eprintln!("requires an isolated Xvfb display");
            return;
        };
        let nonexistent_window = injector.conn.generate_id().unwrap();
        // An unchecked request puts its asynchronous protocol error in the event queue.
        injector.conn.destroy_window(nonexistent_window).unwrap();
        injector.conn.get_input_focus().unwrap().reply().unwrap();
        let result = injector.inject(&[]);
        assert_eq!(result.applied_count, 0);
        assert!(
            result.os_error.is_some(),
            "protocol errors must not be discarded"
        );
    }

    #[test]
    fn notches_conserve_units_and_bound_work_even_at_integer_limits() {
        for input in [
            i32::MIN,
            -3841,
            -3840,
            -121,
            -120,
            -119,
            0,
            119,
            120,
            121,
            3840,
            3841,
            i32::MAX,
        ] {
            let mut remainder = input;
            let notches = take_notches(&mut remainder);
            assert_eq!(
                i64::from(notches) * 120 + i64::from(remainder),
                i64::from(input)
            );
            assert!(notches.abs() <= 32);
            if notches.abs() < 32 {
                assert!(remainder.abs() < 120);
            }
            if input > 0 {
                assert!(notches >= 0 && remainder >= 0);
            }
            if input < 0 {
                assert!(notches <= 0 && remainder <= 0);
            }
        }
    }

    #[test]
    fn subnotch_streams_and_capped_backlogs_drain_without_losing_units() {
        let mut remainder = 0;
        for (delta, expected_notches, expected_remainder) in [
            (119, 0, 119),
            (1, 1, 0),
            (-119, 0, -119),
            (-2, -1, -1),
            (1, 0, 0),
            (7801, 32, 3961),
            (0, 32, 121),
            (0, 1, 1),
        ] {
            remainder += delta;
            assert_eq!(take_notches(&mut remainder), expected_notches);
            assert_eq!(remainder, expected_remainder);
        }
    }

    #[test]
    fn injected_coordinates_round_and_saturate_to_root_and_protocol_bounds() {
        for extent in [0, 1, 1920, u16::MAX] {
            for value in [
                f64::NAN,
                f64::NEG_INFINITY,
                -0.5,
                0.0,
                0.5,
                1919.9,
                f64::INFINITY,
            ] {
                let actual = clamp_coordinate(value, extent);
                assert!(actual >= 0);
                assert!(u16::try_from(actual).unwrap() <= extent.saturating_sub(1));
            }
        }
        assert_eq!(clamp_coordinate(f64::NAN, 1920), 0);
        assert_eq!(clamp_coordinate(f64::NEG_INFINITY, 1920), 0);
        assert_eq!(clamp_coordinate(f64::INFINITY, 1920), 1919);
        assert_eq!(clamp_coordinate(f64::INFINITY, u16::MAX), i16::MAX);
        assert_eq!(clamp_coordinate(10.49, 1920), 10);
        assert_eq!(clamp_coordinate(10.5, 1920), 11);
    }
}
