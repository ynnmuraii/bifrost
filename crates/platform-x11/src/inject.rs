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
use x11rb::connection::{
    Connection as _, DiscardMode, RequestConnection as _, RequestKind, SequenceNumber,
};
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
/// A partial failure reports the proven event prefix and an error. Later queued
/// events may also have landed; never replay the batch. The held ledger tracks
/// individually accepted releases and conservative downs independently.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InjectResult {
    /// Accepted event prefix for injection; proven individual ups for release_all.
    ///
    /// A `Scroll` whose 120-unit remainder has not yet reached a whole notch is
    /// consumed (the remainder is retained for the next event) without sending a
    /// request, and still counts here — nothing is lost either way.
    pub applied_count: usize,
    /// Set for request, flush or synchronization failure. Events after the proven
    /// prefix may already have landed; this is not permission to replay them.
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
/// * every possibly delivered press stays present until a checked release,
/// * queueing an up is not delivery proof: request or transport failure keeps
///   the previous hold, while uncertain downs remain conservative,
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

#[derive(Clone, Copy)]
enum LedgerEdge {
    Key(u8, KeyState),
    Button(u8, KeyState),
}

impl LedgerEdge {
    fn is_down(self) -> bool {
        match self {
            Self::Key(_, state) | Self::Button(_, state) => state.is_down(),
        }
    }

    fn note(self, ledger: &mut PressedLedger) {
        match self {
            Self::Key(key, state) => ledger.note_key(key, state),
            Self::Button(button, state) => ledger.note_button(button, state),
        }
    }
}

struct PendingRequest {
    sequence: Result<SequenceNumber, String>,
    edge: Option<LedgerEdge>,
    event: usize,
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
    pending: Vec<PendingRequest>,
    pending_event: usize,
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
            pending: Vec::new(),
            pending_event: 0,
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
    /// Queueing stops at the first local error. A single server barrier checks all
    /// queued requests and reports the accepted event prefix; later queued edges
    /// can already have landed and are retained accurately by the release ledger.
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
        for (index, event) in events.iter().enumerate() {
            self.pending_event = index;
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
        let (_, failure) = self.confirm_batch();
        if let Some((index, error)) = failure {
            applied_count = applied_count.min(index);
            os_error.get_or_insert(error);
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
        let mut os_error = None;
        for (index, keycode) in keys.into_iter().enumerate() {
            self.pending_event = index;
            if let Err(error) = self.fake_key(keycode, KeyState::Up) {
                os_error.get_or_insert_with(|| error.to_string());
            }
        }
        let key_count = self.pending.len();
        for (index, button) in buttons.into_iter().enumerate() {
            self.pending_event = key_count + index;
            if let Err(error) = self.fake_button(button, KeyState::Up) {
                os_error.get_or_insert_with(|| error.to_string());
            }
        }
        let (applied_count, failure) = self.confirm_batch();
        if let Some((_, error)) = failure {
            os_error.get_or_insert(error);
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

    fn fake_absolute_motion(&mut self, position: Position) -> Result<(), X11Error> {
        let x = clamp_coordinate(position.x, self.root_width);
        let y = clamp_coordinate(position.y, self.root_height);
        self.queue_input(xproto::MOTION_NOTIFY_EVENT, 0, self.root, x, y, None)
    }

    fn fake_key(&mut self, keycode: u8, state: KeyState) -> Result<(), X11Error> {
        let type_ = if state.is_down() {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.queue_input(
            type_,
            keycode,
            x11rb::NONE,
            0,
            0,
            Some(LedgerEdge::Key(keycode, state)),
        )
    }

    fn fake_button(&mut self, button: u8, state: KeyState) -> Result<(), X11Error> {
        let type_ = if state.is_down() {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.queue_input(
            type_,
            button,
            x11rb::NONE,
            0,
            0,
            Some(LedgerEdge::Button(button, state)),
        )
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

    fn queue_input(
        &mut self,
        type_: u8,
        detail: u8,
        root: Window,
        x: i16,
        y: i16,
        edge: Option<LedgerEdge>,
    ) -> Result<(), X11Error> {
        // A failed write may already have sent the down; only checked ups retire it.
        if let Some(edge) = edge.filter(|edge| edge.is_down()) {
            edge.note(&mut self.ledger);
        }
        let request = self.conn.xtest_fake_input(
            type_,
            detail,
            x11rb::CURRENT_TIME,
            root,
            x,
            y,
            XTEST_DEFAULT_DEVICE,
        );
        match request {
            Ok(cookie) => {
                let sequence = cookie.sequence_number();
                // Keep this checked request registered until the batch barrier below.
                // VoidCookie contains only a borrowed connection and its sequence.
                std::mem::forget(cookie);
                self.pending.push(PendingRequest {
                    sequence: Ok(sequence),
                    edge,
                    event: self.pending_event,
                });
                Ok(())
            }
            Err(error) => {
                self.pending.push(PendingRequest {
                    sequence: Err(error.to_string()),
                    edge,
                    event: self.pending_event,
                });
                Err(error.into())
            }
        }
    }

    fn confirm_batch(&mut self) -> (usize, Option<(usize, String)>) {
        let synchronized = if self.pending.is_empty() {
            self.flush()
        } else {
            // One reply covers all earlier XTest requests; checks below add no round trips.
            (|| -> Result<(), X11Error> {
                self.conn.get_input_focus()?.reply()?;
                Ok(())
            })()
        };
        if let Err(error) = synchronized {
            for request in self.pending.drain(..) {
                if let Ok(sequence) = request.sequence {
                    self.conn.discard_reply(
                        sequence,
                        RequestKind::IsVoid,
                        DiscardMode::DiscardReplyAndError,
                    );
                }
            }
            return (0, Some((0, error.to_string())));
        }
        let mut applied = 0;
        let mut failure = None;
        for request in self.pending.drain(..) {
            let result = request.sequence.and_then(|sequence| {
                self.conn
                    .check_for_error(sequence)
                    .map_err(|error| error.to_string())
            });
            settle_edge(&mut self.ledger, request.edge, result.is_ok());
            match result {
                Ok(()) => applied += 1,
                Err(error) => {
                    failure.get_or_insert((request.event, error));
                }
            }
        }
        (applied, failure)
    }
}

fn settle_edge(ledger: &mut PressedLedger, edge: Option<LedgerEdge>, accepted: bool) {
    if let Some(edge) = edge.filter(|edge| accepted || edge.is_down()) {
        edge.note(ledger);
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
    fn uncertain_ups_and_later_failed_downs_keep_the_release_ledger() {
        use super::{settle_edge, LedgerEdge, PressedLedger};
        use input_model::KeyState::{Down, Up};
        let mut ledger = PressedLedger::new();
        for edge in [LedgerEdge::Key(38, Down), LedgerEdge::Button(1, Down)] {
            settle_edge(&mut ledger, Some(edge), false);
        }
        settle_edge(&mut ledger, Some(LedgerEdge::Key(38, Up)), false);
        settle_edge(&mut ledger, Some(LedgerEdge::Button(1, Up)), false);
        assert_eq!(ledger.keys(), [38]);
        assert_eq!(ledger.buttons(), [1]);
        settle_edge(&mut ledger, Some(LedgerEdge::Key(38, Up)), true);
        settle_edge(&mut ledger, Some(LedgerEdge::Key(38, Down)), false);
        assert_eq!(ledger.keys(), [38]);
        settle_edge(&mut ledger, Some(LedgerEdge::Key(38, Up)), true);
        assert!(ledger.keys().is_empty());
        assert_eq!(ledger.buttons(), [1]);
    }

    #[test]
    fn checked_xtest_failure_retires_only_proven_releases() {
        let Some(_native) = crate::capture::isolated_native_test() else {
            return;
        };
        use input_model::KeyState::{Down, Up};
        let mut injector = super::X11Injector::new().unwrap();
        injector.ledger.note_key(38, Down);
        injector.ledger.note_key(39, Down);
        injector.fake_key(38, Up).unwrap();
        injector.pending_event = 1;
        injector
            .queue_input(
                u8::MAX,
                39,
                x11rb::NONE,
                0,
                0,
                Some(super::LedgerEdge::Key(39, Up)),
            )
            .unwrap();
        let (applied, error) = injector.confirm_batch();
        assert_eq!(applied, 1);
        assert_eq!(error.unwrap().0, 1);
        assert_eq!(injector.pressed_keys(), [39]);
        assert!(injector.inject(&[]).os_error.is_none());
        assert_eq!(injector.pressed_keys(), [39]);
        let released = injector.release_all();
        assert_eq!(released.applied_count, 1);
        assert!(released.os_error.is_none());
        assert!(injector.is_empty());
    }

    #[test]
    fn lost_connection_cannot_turn_queued_ups_into_empty_release_success() {
        let Some(_native) = crate::capture::isolated_native_test() else {
            return;
        };
        use input_model::KeyState::{Down, Up};
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        let mut injector = super::X11Injector::new().unwrap();
        let window = injector.conn.generate_id().unwrap();
        injector
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                injector.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        injector.ledger.note_key(38, Down);
        injector.fake_key(38, Up).unwrap();
        let (killer, _) = x11rb::connect(None).unwrap();
        killer.kill_client(window).unwrap().check().unwrap();
        let (_, failure) = injector.confirm_batch();
        assert!(failure.is_some());
        assert_eq!(injector.pressed_keys(), [38]);
        let released = injector.release_all();
        assert!(released.os_error.is_some());
        assert_eq!(released.applied_count, 0);
        assert_eq!(injector.pressed_keys(), [38]);
    }

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
            pending: Vec::new(),
            pending_event: 0,
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
