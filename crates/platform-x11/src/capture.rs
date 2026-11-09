//! XInput2 raw capture with an optional pointer/keyboard grab (KVM-013).
//!
//! # Why raw events *and* a grab
//!
//! XI2 raw events are passive: any client that selects them keeps seeing the real
//! device stream while the local session keeps behaving normally. They alone do
//! **not** suppress local input. Suppression is the grab
//! ([`X11Capture::grab`], `owner_events = false`, pointer and keyboard), which is
//! why it is a separate fallible step: if either grab is refused the transfer must
//! be cancelled rather than silently letting local input through.
//!
//! # Injected-loop prevention
//!
//! XTest fake input enters the server through the virtual XTEST slave devices
//! (`Virtual core XTEST pointer` / `Virtual core XTEST keyboard`). At startup the
//! capture enumerates devices with XI2 and remembers the ids whose name contains
//! `XTEST`; every raw event whose `deviceid` **or** `sourceid` is one of them is
//! dropped and counted ([`X11Capture::injected_events_filtered`]). Without that
//! filter, a KVM source that also injects would feed its own synthetic input back
//! into the transfer, and the pointer would run away.
//!
//! # Recenter
//!
//! Raw motion is relative-only and reports the *deltas* the devices produced.
//! Left alone, the local cursor reaches a screen edge and stops moving while the
//! remote still expects motion, so the source "sticks". While grabbed, the
//! capture therefore tracks where the cursor actually is and warps it back to the
//! screen centre once it drifts further than [`RECENTER_THRESHOLD_PX`] from it,
//! counting the warps in [`X11Capture::recenter_count`]. The caller never sees the
//! warp: the reported position is an *integrated* logical position accumulated
//! from the raw deltas (clamped to the root geometry), so the remote cursor keeps
//! moving even though the local one snapped back. Observe-only mode never warps —
//! moving the user's pointer around while merely observing would be hostile.
//!
//! The alternative considered was confining the pointer to a 1x1 `InputOnly`
//! window via `confine_to`, which avoids visible cursor travel but pins the
//! pointer to one pixel and still needs a warp to give the X server back its
//! coordinates; the threshold + warp approach was chosen because it keeps the
//! cursor usable (and visible) for a human sitting at the source.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use input_model::{CaptureKind, InputEvent, KeyState, Position};
use x11rb::connection::Connection as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::inject::PressedLedger;
use crate::keymap::{
    wheel_from_x11_button, x11_button_to_mouse_button, x11_keycode_to_hid_key,
    EMERGENCY_F12_X11_KEYCODE,
};
use crate::X11Error;

/// XI2 `deviceid` meaning "all devices", including the XTEST slaves. Used both to
/// enumerate devices and to select raw events: selecting on all devices is what
/// lets the injected-loop filter be *proven* (the spike sees the XTEST events and
/// drops them) instead of never seeing them at all.
pub const XI_ALL_DEVICES: u16 = 0;

/// Distance from the screen centre at which the cursor is warped back, in pixels.
pub const RECENTER_THRESHOLD_PX: f64 = 128.0;

/// A captured input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapturedEvent {
    /// A normalized event destined for the remote node.
    Input(InputEvent),
    /// The emergency abort combo fired. The caller must stop the transfer.
    ///
    /// The full adapter also has to react to `TopologyChanged` and
    /// `LocalTakeover`; those are served by other mechanisms (RandR monitor
    /// notifications / the control channel), not by the raw event stream, so they
    /// are not variants here.
    Emergency,
    HotkeyPressed(u8, Instant),
}

/// Full x11rb request sequences identify the server-side grab, not dequeue time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GrabEpoch {
    pointer: u64,
    keyboard: u64,
}

impl GrabEpoch {
    fn suppresses(self, sequence: u64, keyboard: bool) -> bool {
        sequence
            >= if keyboard {
                self.keyboard
            } else {
                self.pointer
            }
    }
}

#[derive(Clone, Copy)]
struct RawEdge {
    keyboard: bool,
    detail: u32,
    down: bool,
    deviceid: u16,
    sourceid: u16,
}

fn raw_edge(event: &Event) -> Option<RawEdge> {
    let (keyboard, detail, down, deviceid, sourceid) = match event {
        Event::XinputRawKeyPress(raw) => (true, raw.detail, true, raw.deviceid, raw.sourceid),
        Event::XinputRawKeyRelease(raw) => (true, raw.detail, false, raw.deviceid, raw.sourceid),
        Event::XinputRawButtonPress(raw) => (false, raw.detail, true, raw.deviceid, raw.sourceid),
        Event::XinputRawButtonRelease(raw) => {
            (false, raw.detail, false, raw.deviceid, raw.sourceid)
        }
        _ => return None,
    };
    Some(RawEdge {
        keyboard,
        detail,
        down,
        deviceid,
        sourceid,
    })
}

fn suppressed_between(start: GrabEpoch, end: GrabEpoch, sequence: u64, keyboard: bool) -> bool {
    start.suppresses(sequence, keyboard) && !end.suppresses(sequence, keyboard)
}

/// Tracks only events after known-empty snapshots and before each device's grab.
struct AdmissionCrossing {
    snapshot: GrabEpoch,
    grab: GrabEpoch,
    keys: [bool; 256],
    buttons: [bool; 256],
}

impl AdmissionCrossing {
    fn new(snapshot: GrabEpoch, grab: GrabEpoch) -> Self {
        Self {
            snapshot,
            grab,
            keys: [false; 256],
            buttons: [false; 256],
        }
    }

    fn note(&mut self, sequence: u64, edge: RawEdge) {
        if self.snapshot.suppresses(sequence, edge.keyboard)
            && !self.grab.suppresses(sequence, edge.keyboard)
        {
            let states = if edge.keyboard {
                &mut self.keys
            } else {
                &mut self.buttons
            };
            if let Some(state) = states.get_mut(edge.detail as usize) {
                *state = edge.down;
            }
        }
    }

    fn crossed(&self) -> bool {
        self.keys.iter().chain(&self.buttons).any(|down| *down)
    }
}

fn discrete_event(
    epoch: Option<GrabEpoch>,
    sequence: u64,
    input: InputEvent,
) -> Option<CapturedEvent> {
    if epoch
        .is_some_and(|epoch| !epoch.suppresses(sequence, matches!(input, InputEvent::Key { .. })))
    {
        return None;
    }
    Some(CapturedEvent::Input(input))
}

#[derive(Clone, Copy)]
struct ServerClock {
    sequence: u64,
    time: u32,
    sent_at: Instant,
}

impl ServerClock {
    fn issued_at(self, sequence: u64, time: u32, now: Instant) -> Option<Instant> {
        // Do not calibrate from input receipts. An old/unknown clock sample fails closed.
        if sequence < self.sequence
            || now.checked_duration_since(self.sent_at)? > Duration::from_secs(60)
        {
            return None;
        }
        let elapsed_ms = time.wrapping_sub(self.time);
        if elapsed_ms > i32::MAX as u32 {
            return None;
        }
        // Server timestamps are millisecond ticks; subtract a tick for a lower bound.
        let delta = i64::from(elapsed_ms) - 1;
        let issued = if delta >= 0 {
            self.sent_at
                .checked_add(Duration::from_millis(delta as u64))?
        } else {
            self.sent_at
                .checked_sub(Duration::from_millis(delta.unsigned_abs()))?
        };
        (now.checked_duration_since(issued)? < Duration::from_millis(400)).then_some(issued)
    }
}

fn hotkey_edge(
    state: &mut crate::hotkeys::HotkeyState,
    fence: u64,
    sequence: u64,
    keycode: u8,
    down: bool,
) -> (bool, Option<u8>) {
    if sequence < fence {
        return (false, None);
    }
    state.edge(keycode, down)
}

fn timed_hotkey(
    action: u8,
    clock: Option<ServerClock>,
    sequence: u64,
    time: u32,
    now: Instant,
) -> Option<CapturedEvent> {
    let issued_at = if action == 1 {
        clock?.issued_at(sequence, time, now)?
    } else {
        // Return-local and the other immediate actions do not arm a release latch.
        now
    };
    Some(CapturedEvent::HotkeyPressed(action, issued_at))
}

/// XI2 raw capture, optionally holding the pointer+keyboard grab.
pub struct X11Capture {
    conn: RustConnection,
    root: Window,
    root_width: u16,
    root_height: u16,
    xi2_version: (u16, u16),
    xtest_device_ids: Vec<u16>,
    kind: CaptureKind,
    grab_active: bool,
    recenters: u64,
    filtered_injected: u64,
    emergencies: u64,
    last_event_deviceid: u16,
    last_motion_delta: (f64, f64),
    integrated: Position,
    local: Position,
    ledger: PressedLedger,
    hotkeys: crate::hotkeys::HotkeyState,
    shortcut_grabs: Vec<(u8, u16)>,
    grab_epoch: Option<GrabEpoch>,
    hotkey_fence: u64,
    clock_window: Window,
    clock_atom: xproto::Atom,
    clock: Option<ServerClock>,
    clock_pending: Option<(u64, Instant)>,
    pending: VecDeque<(Event, u64)>,
}

impl X11Capture {
    /// Connects to `$DISPLAY`, negotiates XI2, finds the XTEST virtual devices and
    /// selects the raw events on the root window.
    ///
    /// `kind` records the caller's intent ([`CaptureKind`]) for reporting and for
    /// [`Self::is_suppressed`]; the grab itself is always an explicit
    /// [`Self::grab`] call, so an `Observe` capture never moves the user's pointer
    /// and a `Remote`/`Temporary` capture suppresses nothing until `grab` succeeds.
    pub fn new(kind: CaptureKind) -> Result<Self, X11Error> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let (root, root_width, root_height) = {
            let screen = &conn.setup().roots[screen_num];
            (screen.root, screen.width_in_pixels, screen.height_in_pixels)
        };
        let version = conn.xinput_xi_query_version(2, 0)?.reply()?;
        if version.major_version < 2 {
            return Err(X11Error::Unsupported(format!(
                "XInput2 is required, the server offers {}.{}",
                version.major_version, version.minor_version
            )));
        }
        let xtest_device_ids = enumerate_xtest_devices(&conn)?;
        let clock_window = conn.generate_id().map_err(|error| {
            X11Error::Internal(format!("the server has no window id to give: {error}"))
        })?;
        conn.create_window(
            0,
            clock_window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        let clock_atom = conn
            .intern_atom(false, b"_BIFROST_CAPTURE_CLOCK")?
            .reply()?
            .atom;
        // XISelectEvents takes mask words, not one word per event bit.
        let mask = vec![
            xinput::XIEventMask::RAW_MOTION
                | xinput::XIEventMask::RAW_KEY_PRESS
                | xinput::XIEventMask::RAW_KEY_RELEASE
                | xinput::XIEventMask::RAW_BUTTON_PRESS
                | xinput::XIEventMask::RAW_BUTTON_RELEASE,
        ];
        let subscription = conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: XI_ALL_DEVICES,
                mask,
            }],
        )?;
        let start = query_pointer(&conn, root)?;
        // The pointer reply also serves as the subscription's error barrier.
        subscription.check()?;
        let mut capture = Self {
            conn,
            root,
            root_width,
            root_height,
            xi2_version: (version.major_version, version.minor_version),
            xtest_device_ids,
            kind,
            grab_active: false,
            recenters: 0,
            filtered_injected: 0,
            emergencies: 0,
            last_event_deviceid: 0,
            last_motion_delta: (0.0, 0.0),
            integrated: start,
            local: start,
            ledger: PressedLedger::new(),
            hotkeys: crate::hotkeys::HotkeyState::default(),
            shortcut_grabs: Vec::new(),
            grab_epoch: None,
            hotkey_fence: 0,
            clock_window,
            clock_atom,
            clock: None,
            clock_pending: None,
            pending: VecDeque::new(),
        };
        capture.refresh_clock()?;
        Ok(capture)
    }

    /// Resolves a keysym against the current server keyboard map.
    pub fn hotkey_keycode(&self, keysym: u32) -> Result<u8, X11Error> {
        let setup = self.conn.setup();
        let mapping = self
            .conn
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()?;
        mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .position(|symbols| symbols.contains(&keysym))
            .and_then(|index| u8::try_from(index).ok())
            .map(|index| setup.min_keycode + index)
            .ok_or_else(|| {
                X11Error::Unsupported(format!(
                    "hotkey keysym {keysym:#x} is absent from the keyboard map"
                ))
            })
    }

    /// Acquires every new passive grab before retiring the old set. On refusal, rolls back additions.
    pub fn set_hotkeys(
        &mut self,
        bindings: [Option<crate::hotkeys::Hotkey>; 5],
    ) -> Result<(), X11Error> {
        // Seed and fence only after the replacement is installed on the server.
        let mut wanted = Vec::new();
        for binding in
            bindings
                .iter()
                .flatten()
                .copied()
                .chain(std::iter::once(crate::hotkeys::Hotkey {
                    keycode: EMERGENCY_F12_X11_KEYCODE,
                    modifiers: 1 | 4 | 8,
                }))
        {
            for locks in [0, 2, 16, 2 | 16] {
                let grab = (binding.keycode, binding.modifiers | locks);
                if !wanted.contains(&grab) {
                    wanted.push(grab);
                }
            }
        }
        let installed = crate::hotkeys::replace_grabs(
            &self.shortcut_grabs,
            &wanted,
            |(keycode, modifiers)| -> Result<(), X11Error> {
                self.conn
                    .grab_key(
                        false,
                        self.root,
                        modifiers.into(),
                        keycode,
                        xproto::GrabMode::ASYNC,
                        xproto::GrabMode::ASYNC,
                    )?
                    .check()?;
                Ok(())
            },
            |(keycode, modifiers)| {
                let _ = self
                    .conn
                    .ungrab_key(keycode, self.root, xproto::ModMask::from(modifiers));
            },
        );
        self.conn.flush()?;
        installed?;
        let snapshot = self.conn.query_keymap()?;
        let fence = snapshot.sequence_number();
        let physical = snapshot.reply()?.keys;
        self.hotkey_fence = fence;
        self.hotkeys.seed(&physical);
        self.hotkeys.bindings = bindings;
        self.shortcut_grabs = wanted;
        Ok(())
    }

    /// The XI2 version negotiated with the server (`major`, `minor`).
    pub fn xi2_version(&self) -> (u16, u16) {
        self.xi2_version
    }

    /// The device ids whose name reports them as XTEST virtual devices.
    pub fn xtest_device_ids(&self) -> &[u16] {
        &self.xtest_device_ids
    }

    /// The root window raw events are selected on.
    pub fn root(&self) -> Window {
        self.root
    }

    /// Root window size in pixels.
    pub fn root_size(&self) -> (u16, u16) {
        (self.root_width, self.root_height)
    }

    /// The capture mode the caller asked for.
    ///
    /// [`CaptureKind::is_suppressed`] describes the *intent*; whether input is
    /// actually suppressed is [`Self::is_suppressed`], which is the grab state.
    pub fn capture_kind(&self) -> CaptureKind {
        self.kind
    }

    /// `true` while this capture holds the pointer and keyboard grab.
    pub fn grab_active(&self) -> bool {
        self.grab_active
    }

    /// `true` when local input is currently suppressed — i.e. exactly while the
    /// grab is held. Raw-event observation alone never suppresses anything.
    pub fn is_suppressed(&self) -> bool {
        self.grab_active
    }

    /// Number of centre warps performed by the recenter logic.
    pub fn recenter_count(&self) -> u64 {
        self.recenters
    }

    /// Number of raw events dropped because they came from an XTEST device.
    pub fn injected_events_filtered(&self) -> u64 {
        self.filtered_injected
    }

    /// Number of times the emergency combo fired.
    pub fn emergency_events(&self) -> u64 {
        self.emergencies
    }

    /// `deviceid` of the most recent raw event, filtered or not. The spike prints
    /// it to show which device produced what, and to prove the filter's source.
    pub fn last_event_deviceid(&self) -> u16 {
        self.last_event_deviceid
    }

    /// `(dx, dy)` of the most recent raw motion event, in root pixels.
    pub fn last_motion_delta(&self) -> (f64, f64) {
        self.last_motion_delta
    }

    /// The integrated logical pointer position reported to the remote.
    pub fn integrated_position(&self) -> Position {
        self.integrated
    }

    /// Where the capture believes the local cursor is.
    pub fn local_position(&self) -> Position {
        self.local
    }

    /// The keys and buttons seen held down in the captured stream.
    pub fn ledger(&self) -> &PressedLedger {
        &self.ledger
    }

    /// Server snapshot also includes holds predating this capture connection.
    pub fn physical_pressed(&self) -> Result<input_model::PressedState, X11Error> {
        self.physical_snapshot().map(|(pressed, _)| pressed)
    }

    fn physical_snapshot(&self) -> Result<(input_model::PressedState, GrabEpoch), X11Error> {
        let snapshot = self.conn.query_keymap()?;
        let keyboard_sequence = snapshot.sequence_number();
        let keys = snapshot.reply()?.keys;
        let mut pressed = input_model::PressedState::default();
        for code in 8u16..=255 {
            if keys[usize::from(code / 8)] & (1 << (code % 8)) != 0 {
                let key = x11_keycode_to_hid_key(code as u8).ok_or_else(|| {
                    X11Error::Unsupported("held keyboard identity is not mapped".to_owned())
                })?;
                pressed.keys.insert(key);
            }
        }
        // XI2 device state covers side buttons, which core QueryPointer cannot represent.
        let client_pointer = self
            .conn
            .xinput_xi_get_client_pointer(x11rb::NONE)?
            .reply()?;
        let snapshot = self
            .conn
            .xinput_xi_query_pointer(self.root, client_pointer.deviceid)?;
        let pointer_sequence = snapshot.sequence_number();
        let devices = snapshot.reply()?;
        for (number, button) in [
            (1usize, input_model::MouseButton::Left),
            (2, input_model::MouseButton::Middle),
            (3, input_model::MouseButton::Right),
            (8, input_model::MouseButton::Back),
            (9, input_model::MouseButton::Forward),
        ] {
            if devices
                .buttons
                .get(number / 32)
                .is_some_and(|bits| bits & (1 << (number % 32)) != 0)
            {
                pressed.buttons.insert(button);
            }
        }
        Ok((
            pressed,
            GrabEpoch {
                pointer: pointer_sequence,
                keyboard: keyboard_sequence,
            },
        ))
    }

    /// Reads the real pointer position back from the server.
    pub fn pointer_position(&self) -> Result<Position, X11Error> {
        query_pointer(&self.conn, self.root)
    }

    /// Takes the pointer and keyboard grab with `owner_events = false`.
    ///
    /// Any failure attempts checked release of both grabs and local replay.
    /// Failed rollback/restoration is returned without erasing surviving grab evidence.
    pub fn grab(&mut self) -> Result<(), X11Error> {
        if self.grab_active {
            return Ok(());
        }
        let (pressed, snapshot) = self.physical_snapshot()?;
        if !pressed.is_empty() {
            return Err(X11Error::Internal(
                "physical input is held; capture commit refused".to_owned(),
            ));
        }
        let admission = (|| {
            let epoch = self.take_grabs()?;
            self.grab_epoch = Some(epoch);
            self.grab_active = true;
            let final_pressed = self.physical_pressed()?;
            self.collect_until_barrier()?;
            let mut crossing = AdmissionCrossing::new(snapshot, epoch);
            for (event, sequence) in &self.pending {
                if let Some(edge) = raw_edge(event) {
                    if !self.is_own_injection(edge.deviceid, edge.sourceid) {
                        crossing.note(*sequence, edge);
                    }
                }
            }
            if !final_pressed.is_empty() || crossing.crossed() {
                return Err(X11Error::Internal(
                    "physical input crossed capture admission; commit refused".to_owned(),
                ));
            }
            Ok(())
        })();
        if let Err(error) = admission {
            if let Err(rollback) = self.rollback_admission() {
                let cleanup = self.ungrab();
                return Err(X11Error::Internal(match cleanup {
                    Ok(()) => format!("{error}; local input rollback failed: {rollback}"),
                    Err(cleanup) => format!(
                        "{error}; local input rollback failed: {rollback}; ungrab failed: {cleanup}"
                    ),
                }));
            }
            return Err(error);
        }
        let position = self
            .pointer_position()
            .unwrap_or_else(|_| screen_centre(self.root_width, self.root_height));
        self.integrated = position;
        self.local = position;
        Ok(())
    }

    /// Checks both native releases. Failed cleanup retains grab evidence for retry.
    pub fn ungrab(&mut self) -> Result<(), X11Error> {
        self.drop_grabs().map(|_| ())
    }

    /// Waits for the next captured event.
    ///
    /// Returns `Ok(None)` when `timeout` expires without a capturable event, so a
    /// spike can run for `--seconds` without depending on Ctrl-C. `None` waits
    /// indefinitely.
    pub fn next_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<CapturedEvent>, X11Error> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        self.refresh_clock()?;
        loop {
            let next = match self.pending.pop_front() {
                Some(event) => Some(event),
                None => self.conn.poll_for_event_with_sequence()?,
            };
            if let Some((event, sequence)) = next {
                if let Some(captured) = self.translate(event, sequence) {
                    return Ok(Some(captured));
                }
                continue;
            }
            let sleep = match deadline {
                None => None,
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Ok(None);
                    }
                    Some(deadline - now)
                }
            };
            self.wait_readable(sleep)?;
        }
    }

    fn take_grabs(&mut self) -> Result<GrabEpoch, X11Error> {
        let pointer_cookie = self.conn.grab_pointer(
            false,
            self.root,
            xproto::EventMask::NO_EVENT,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )?;
        let pointer_sequence = pointer_cookie.sequence_number();
        let pointer = pointer_cookie.reply()?;
        if pointer.status != xproto::GrabStatus::SUCCESS {
            return Err(X11Error::Internal(format!(
                "XGrabPointer was refused (status {})",
                u8::from(pointer.status)
            )));
        }
        self.grab_epoch = Some(GrabEpoch {
            pointer: pointer_sequence,
            keyboard: u64::MAX,
        });
        let keyboard_cookie = self.conn.grab_keyboard(
            false,
            self.root,
            x11rb::CURRENT_TIME,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
        )?;
        let keyboard_sequence = keyboard_cookie.sequence_number();
        let keyboard = keyboard_cookie.reply()?;
        if keyboard.status != xproto::GrabStatus::SUCCESS {
            return Err(X11Error::Internal(format!(
                "XGrabKeyboard was refused (status {})",
                u8::from(keyboard.status)
            )));
        }
        self.grab_epoch = Some(GrabEpoch {
            pointer: pointer_sequence,
            keyboard: keyboard_sequence,
        });
        self.conn.flush()?;
        Ok(GrabEpoch {
            pointer: pointer_sequence,
            keyboard: keyboard_sequence,
        })
    }

    fn collect_until_barrier(&mut self) -> Result<(), X11Error> {
        let cookie = self.conn.change_property(
            xproto::PropMode::APPEND,
            self.clock_window,
            self.clock_atom,
            xproto::AtomEnum::STRING,
            8,
            0,
            &[],
        )?;
        let barrier = cookie.sequence_number();
        self.conn.flush()?;
        loop {
            let (event, sequence) = self.conn.wait_for_event_with_sequence()?;
            if matches!(&event, Event::PropertyNotify(property)
                if property.window == self.clock_window && property.atom == self.clock_atom)
                && sequence == barrier
            {
                return Ok(());
            }
            self.pending.push_back((event, sequence));
        }
    }

    fn rollback_admission(&mut self) -> Result<(), X11Error> {
        let Some(start) = self.grab_epoch else {
            return self.ungrab();
        };
        let end = self.drop_grabs()?;
        self.collect_until_barrier()?;
        let mut hotkeys = self.hotkeys.clone();
        let mut error = None;
        for (event, sequence) in &self.pending {
            let edge =
                raw_edge(event).filter(|edge| !self.is_own_injection(edge.deviceid, edge.sourceid));
            let swallowed = edge.is_some_and(|edge| {
                edge.keyboard
                    && hotkey_edge(
                        &mut hotkeys,
                        self.hotkey_fence,
                        *sequence,
                        edge.detail as u8,
                        edge.down,
                    )
                    .0
            });
            let replay = edge.filter(|edge| {
                !swallowed && suppressed_between(start, end, *sequence, edge.keyboard)
            });
            if let Some(edge) = replay {
                let type_ = match (edge.keyboard, edge.down) {
                    (true, true) => xproto::KEY_PRESS_EVENT,
                    (true, false) => xproto::KEY_RELEASE_EVENT,
                    (false, true) => xproto::BUTTON_PRESS_EVENT,
                    (false, false) => xproto::BUTTON_RELEASE_EVENT,
                };
                let result = (|| -> Result<(), X11Error> {
                    let detail = u8::try_from(edge.detail).map_err(|_| {
                        X11Error::Unsupported(
                            "suppressed input identity cannot be replayed locally".to_owned(),
                        )
                    })?;
                    self.conn
                        .xtest_fake_input(type_, detail, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)?
                        .check()?;
                    Ok(())
                })();
                if let Err(replay_error) = result {
                    if error.is_none() {
                        error = Some(replay_error);
                    }
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn drop_grabs(&mut self) -> Result<GrabEpoch, X11Error> {
        // Attempt both requests before checking either result.
        let pointer = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let keyboard = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let synchronized = (|| -> Result<(), X11Error> {
            self.conn.get_input_focus()?.reply()?;
            Ok(())
        })();
        synchronized?;
        let pointer = pointer.map_err(X11Error::from).and_then(|cookie| {
            let sequence = cookie.sequence_number();
            cookie.check()?;
            Ok(sequence)
        });
        let keyboard = keyboard.map_err(X11Error::from).and_then(|cookie| {
            let sequence = cookie.sequence_number();
            cookie.check()?;
            Ok(sequence)
        });
        let epoch = GrabEpoch {
            pointer: pointer?,
            keyboard: keyboard?,
        };
        self.grab_active = false;
        self.grab_epoch = None;
        Ok(epoch)
    }

    /// Blocks until the X connection is readable or `timeout` elapses.
    ///
    /// The fd is polled rather than `wait_for_event`-ing so a spike can shut down
    /// on a deadline. Events already buffered by x11rb are drained by the caller
    /// *before* polling, which is why a readable fd is only ever waited on when
    /// the buffer is empty.
    fn wait_readable(&self, timeout: Option<Duration>) -> Result<(), X11Error> {
        let timespec = timeout.map(to_timespec);
        let mut fds = [rustix::event::PollFd::new(
            self.conn.stream(),
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(&mut fds, timespec.as_ref())?;
        Ok(())
    }

    fn refresh_clock(&mut self) -> Result<(), X11Error> {
        if self.clock_pending.is_some()
            || self
                .clock
                .is_some_and(|clock| clock.sent_at.elapsed() < Duration::from_secs(30))
        {
            return Ok(());
        }
        let sent_at = Instant::now();
        let cookie = self.conn.change_property(
            xproto::PropMode::APPEND,
            self.clock_window,
            self.clock_atom,
            xproto::AtomEnum::STRING,
            8,
            0,
            &[],
        )?;
        self.clock_pending = Some((cookie.sequence_number(), sent_at));
        self.conn.flush()?;
        Ok(())
    }

    fn translate(&mut self, event: Event, sequence: u64) -> Option<CapturedEvent> {
        match event {
            Event::PropertyNotify(property)
                if property.window == self.clock_window && property.atom == self.clock_atom =>
            {
                if let Some((request, sent_at)) = self.clock_pending {
                    if sequence == request {
                        self.clock = Some(ServerClock {
                            sequence,
                            time: property.time,
                            sent_at,
                        });
                        self.clock_pending = None;
                    }
                }
                None
            }
            Event::XinputRawMotion(raw) => self.capture_motion(&raw),
            Event::XinputRawKeyPress(raw) => self.capture_key(&raw, KeyState::Down, sequence),
            Event::XinputRawKeyRelease(raw) => self.capture_key(&raw, KeyState::Up, sequence),
            Event::XinputRawButtonPress(raw) => self.capture_button(&raw, KeyState::Down, sequence),
            Event::XinputRawButtonRelease(raw) => self.capture_button(&raw, KeyState::Up, sequence),
            _ => None,
        }
    }

    fn capture_motion(&mut self, raw: &xinput::RawMotionEvent) -> Option<CapturedEvent> {
        self.last_event_deviceid = raw.deviceid;
        if self.is_own_injection(raw.deviceid, raw.sourceid) {
            self.filtered_injected += 1;
            return None;
        }
        let (dx, dy) = motion_delta(&raw.valuator_mask, &raw.axisvalues);
        self.last_motion_delta = (dx, dy);
        if dx == 0.0 && dy == 0.0 {
            return None;
        }
        self.integrated = clamp_position(
            Position::new(self.integrated.x + dx, self.integrated.y + dy),
            self.root_width,
            self.root_height,
        );
        self.local = Position::new(self.local.x + dx, self.local.y + dy);
        self.recenter_if_needed();
        Some(CapturedEvent::Input(InputEvent::PointerAbsolute(
            self.integrated,
        )))
    }

    fn capture_key(
        &mut self,
        raw: &xinput::RawKeyPressEvent,
        state: KeyState,
        sequence: u64,
    ) -> Option<CapturedEvent> {
        self.last_event_deviceid = raw.deviceid;
        if self.is_own_injection(raw.deviceid, raw.sourceid) {
            self.filtered_injected += 1;
            return None;
        }
        let keycode = u8::try_from(raw.detail).ok()?;
        let (swallow, action) = hotkey_edge(
            &mut self.hotkeys,
            self.hotkey_fence,
            sequence,
            keycode,
            state.is_down(),
        );
        if let Some(action) = action {
            if action == 0 {
                self.emergencies += 1;
                self.fire_emergency();
                return Some(CapturedEvent::Emergency);
            }
            return timed_hotkey(action, self.clock, sequence, raw.time, Instant::now());
        }
        if swallow {
            return None;
        }
        let key = x11_keycode_to_hid_key(keycode)?;
        let event = discrete_event(self.grab_epoch, sequence, InputEvent::Key { key, state })?;
        self.ledger.note_key(keycode, state);
        Some(event)
    }

    fn capture_button(
        &mut self,
        raw: &xinput::RawButtonPressEvent,
        state: KeyState,
        sequence: u64,
    ) -> Option<CapturedEvent> {
        self.last_event_deviceid = raw.deviceid;
        if self.is_own_injection(raw.deviceid, raw.sourceid) {
            self.filtered_injected += 1;
            return None;
        }
        let button = u8::try_from(raw.detail).ok()?;
        if let Some((h120, v120)) = wheel_from_x11_button(button) {
            if !state.is_down() {
                return None;
            }
            return discrete_event(self.grab_epoch, sequence, InputEvent::Scroll { h120, v120 });
        }
        let kind = x11_button_to_mouse_button(button)?;
        let event = discrete_event(
            self.grab_epoch,
            sequence,
            InputEvent::Button {
                button: kind,
                state,
            },
        )?;
        self.ledger.note_button(button, state);
        Some(event)
    }

    /// Warps the cursor back to the screen centre when it drifted too far.
    ///
    /// Only while grabbed: observe-only must not move the user's pointer.
    fn recenter_if_needed(&mut self) {
        if !self.grab_active {
            return;
        }
        let centre = screen_centre(self.root_width, self.root_height);
        if !needs_recenter(self.local, centre, RECENTER_THRESHOLD_PX) {
            return;
        }
        let x = clamp_to_i16(centre.x);
        let y = clamp_to_i16(centre.y);
        if self
            .conn
            .warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x, y)
            .is_ok()
        {
            let _ = self.conn.flush();
            self.local = centre;
            self.recenters += 1;
        }
    }

    /// Emergency abort path: drop the grab and release everything the captured
    /// stream had left down, so the local server does not keep a modifier or key
    /// stuck after we stop forwarding.
    fn fire_emergency(&mut self) {
        self.release_ledger();
        // The emitted emergency drives checked terminal cleanup and retries any failure.
        let _ = self.ungrab();
    }

    /// Pushes a release for every held key and button, best effort, then forgets
    /// them. Injecting through XTest keeps this path identical to the injector's
    /// recovery and stays filterable (these events are dropped by
    /// [`Self::is_own_injection`] like any other synthetic input).
    fn release_ledger(&mut self) {
        for keycode in self.ledger.keys() {
            let _ = self.conn.xtest_fake_input(
                xproto::KEY_RELEASE_EVENT,
                keycode,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            );
        }
        for button in self.ledger.buttons() {
            let _ = self.conn.xtest_fake_input(
                xproto::BUTTON_RELEASE_EVENT,
                button,
                x11rb::CURRENT_TIME,
                x11rb::NONE,
                0,
                0,
                0,
            );
        }
        let _ = self.conn.flush();
        self.ledger.clear();
    }

    /// `true` when this raw event was produced by our own XTest injection.
    fn is_own_injection(&self, deviceid: u16, sourceid: u16) -> bool {
        self.xtest_device_ids.contains(&deviceid) || self.xtest_device_ids.contains(&sourceid)
    }
}

impl Drop for X11Capture {
    /// Graceful teardown: release the grabs so the local desktop becomes usable
    /// again even if the caller forgets.
    ///
    /// This is only the graceful path. If the process dies, `Drop` never runs —
    /// but the kernel closes the X connection, and the X server releases every
    /// grab held by a client when its connection closes, so a crash cannot leave
    /// the pointer frozen.
    fn drop(&mut self) {
        // Closing this client is best effort, not a positive restoration receipt.
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }
}

/// Reads the root-relative pointer position.
fn query_pointer(conn: &RustConnection, root: Window) -> Result<Position, X11Error> {
    let reply = conn.query_pointer(root)?.reply()?;
    Ok(Position::new(
        f64::from(reply.root_x),
        f64::from(reply.root_y),
    ))
}

/// Device ids of the virtual XTEST devices that XTest fake input is attributed to.
fn enumerate_xtest_devices(conn: &RustConnection) -> Result<Vec<u16>, X11Error> {
    let reply = conn.xinput_xi_query_device(XI_ALL_DEVICES)?.reply()?;
    let mut ids: Vec<u16> = reply
        .infos
        .iter()
        .filter(|info| name_reports_xtest(&info.name))
        .map(|info| info.deviceid)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// `true` when a device name marks it as the XTEST virtual device.
///
/// Matched case-insensitively and by substring, because the exact name depends on
/// the server (`Virtual core XTEST pointer` on Xorg, `XTEST pointer` elsewhere).
pub fn name_reports_xtest(name: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"XTEST";
    name.windows(NEEDLE.len())
        .any(|window| window.eq_ignore_ascii_case(NEEDLE))
}

/// XI2 `FP3232` fixed-point value (32.32) as `f64`.
///
/// The pair is a single signed 64-bit quantity, so the naive
/// `integral + frac / 2^32` is also correct for negatives (`integral = -1`,
/// `frac = 0x8000_0000` is `-0.5`).
pub fn fp3232_to_f64(value: xinput::Fp3232) -> f64 {
    f64::from(value.integral) + f64::from(value.frac) / 4_294_967_296.0
}

/// Value of one XI2 valuator, or `None` when the event did not carry it.
///
/// The valuator is selected out of the bit-packed `valuator_mask`; the matching
/// entry in the packed `axisvalues` list is the number of set bits before it.
pub fn valuator_value(mask: &[u32], values: &[xinput::Fp3232], index: usize) -> Option<f64> {
    if mask.get(index / 32)? & (1u32 << (index % 32)) == 0 {
        return None;
    }
    let packed = mask
        .iter()
        .take(index / 32)
        .map(|word| word.count_ones())
        .sum::<u32>()
        + (mask[index / 32] & ((1u32 << (index % 32)) - 1)).count_ones();
    values
        .get(packed as usize)
        .map(|value| fp3232_to_f64(*value))
}

/// The `(dx, dy)` of a raw motion event (valuators 0 and 1).
pub fn motion_delta(mask: &[u32], values: &[xinput::Fp3232]) -> (f64, f64) {
    (
        valuator_value(mask, values, 0).unwrap_or(0.0),
        valuator_value(mask, values, 1).unwrap_or(0.0),
    )
}

/// The centre of a `width` x `height` screen.
pub fn screen_centre(width: u16, height: u16) -> Position {
    Position::new(f64::from(width) / 2.0, f64::from(height) / 2.0)
}

/// `true` when `position` is further than `threshold` from `centre` on either axis.
pub fn needs_recenter(position: Position, centre: Position, threshold: f64) -> bool {
    (position.x - centre.x).abs() > threshold || (position.y - centre.y).abs() > threshold
}

/// Clamps a position into `0..=width-1` / `0..=height-1`.
///
/// Always yields a finite, in-bounds position, so the integrated position can be
/// used directly as an absolute pointer target. `NaN` components become `0` and
/// infinities saturate to the nearest edge.
pub fn clamp_position(position: Position, width: u16, height: u16) -> Position {
    Position::new(
        clamp_axis(position.x, f64::from(width.saturating_sub(1))),
        clamp_axis(position.y, f64::from(height.saturating_sub(1))),
    )
}

/// Clamps one coordinate into `0..=max`, mapping `NaN` to `0`.
fn clamp_axis(value: f64, max: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, max)
    }
}

/// Rounds a coordinate into the `i16` range X11 requests use.
fn clamp_to_i16(value: f64) -> i16 {
    clamp_axis(value, f64::from(i16::MAX)).round() as i16
}

/// Converts a `Duration` into a `poll(2)` timeout.
fn to_timespec(timeout: Duration) -> rustix::event::Timespec {
    rustix::event::Timespec {
        tv_sec: timeout.as_secs() as rustix::event::Secs,
        tv_nsec: u64::from(timeout.subsec_nanos()) as rustix::event::Nsecs,
    }
}

#[cfg(test)]
pub(crate) fn isolated_native_test() -> Option<std::sync::MutexGuard<'static, ()>> {
    if std::env::var("BIFROST_X11_TEST_ISOLATED").as_deref() != Ok("1") {
        eprintln!("requires BIFROST_X11_TEST_ISOLATED=1 under isolated Xvfb");
        return None;
    }
    static OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    Some(OWNER.lock().unwrap_or_else(|error| error.into_inner()))
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use input_model::{HidKey, MouseButton};

    #[test]
    fn raw_keyboard_and_pointer_edges_reach_native_filter_without_forwarding() {
        let Some(_native) = isolated_native_test() else {
            return;
        };
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xtest::ConnectionExt as _;
        struct ResetSynthetic(x11rb::rust_connection::RustConnection);
        impl Drop for ResetSynthetic {
            fn drop(&mut self) {
                let _ = self
                    .0
                    .xtest_fake_input(xproto::KEY_RELEASE_EVENT, 38, 0, 0, 0, 0, 0);
                let _ = self
                    .0
                    .xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, 0, 0, 0, 0, 0);
                let _ = self.0.flush();
            }
        }
        let mut capture = X11Capture::new(CaptureKind::Remote).unwrap();
        let (injector, _) = x11rb::connect(None).unwrap();
        let injector = ResetSynthetic(injector);
        capture.grab().unwrap();
        for (press, release, detail) in [
            (xproto::KEY_PRESS_EVENT, xproto::KEY_RELEASE_EVENT, 38),
            (xproto::BUTTON_PRESS_EVENT, xproto::BUTTON_RELEASE_EVENT, 1),
        ] {
            let before = capture.injected_events_filtered();
            for kind in [press, release] {
                injector
                    .0
                    .xtest_fake_input(kind, detail, 0, 0, 0, 0, 0)
                    .unwrap()
                    .check()
                    .unwrap();
            }
            let deadline = Instant::now() + Duration::from_millis(500);
            while capture.injected_events_filtered() - before < 2 {
                assert!(
                    Instant::now() < deadline,
                    "native raw edge subscription did not deliver"
                );
                assert!(
                    capture.next_event(Some(Duration::ZERO)).unwrap().is_none(),
                    "synthetic input must never be forwarded"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn checked_ungrab_preserves_failed_native_grab_evidence() {
        let Some(_native) = isolated_native_test() else {
            return;
        };
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, WindowClass};
        let mut capture = X11Capture::new(CaptureKind::Remote).unwrap();
        let window = capture.conn.generate_id().unwrap();
        capture
            .conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                capture.root,
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
        capture.grab().unwrap();
        capture.ungrab().unwrap();
        assert!(!capture.grab_active());
        assert!(capture.grab_epoch.is_none());
        capture.grab().unwrap();
        let epoch = capture.grab_epoch;
        let (killer, _) = x11rb::connect(None).unwrap();
        killer.kill_client(window).unwrap().check().unwrap();
        assert!(capture.ungrab().is_err());
        assert!(capture.grab_active());
        assert_eq!(capture.grab_epoch, epoch);
        assert!(capture.ungrab().is_err());
        assert!(capture.grab_active());
    }

    #[test]
    fn complete_local_click_and_key_tap_before_grabs_never_replay_remotely() {
        // The pointer grab is processed first; keys stay local until the keyboard grab.
        let epoch = Some(GrabEpoch {
            pointer: 65_535,
            keyboard: 65_537,
        });
        let key = HidKey::keyboard(0x04);
        let local = [
            (
                65_534,
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::Down,
                },
            ),
            (
                65_534,
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::Up,
                },
            ),
            (
                65_536,
                InputEvent::Key {
                    key,
                    state: KeyState::Down,
                },
            ),
            (
                65_536,
                InputEvent::Key {
                    key,
                    state: KeyState::Up,
                },
            ),
            (65_534, InputEvent::Scroll { h120: 0, v120: 120 }),
        ];
        for (sequence, event) in local {
            assert_eq!(discrete_event(epoch, sequence, event), None);
        }
        for (sequence, input) in [
            (
                65_535,
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::Down,
                },
            ),
            (
                65_535,
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::Up,
                },
            ),
            (
                65_537,
                InputEvent::Key {
                    key,
                    state: KeyState::Down,
                },
            ),
            (
                65_537,
                InputEvent::Key {
                    key,
                    state: KeyState::Up,
                },
            ),
        ] {
            assert_eq!(
                discrete_event(epoch, sequence, input),
                Some(CapturedEvent::Input(input))
            );
            let later = Some(GrabEpoch {
                pointer: 65_540,
                keyboard: 65_541,
            });
            assert_eq!(discrete_event(later, sequence, input), None);
        }
    }

    #[test]
    fn crossing_local_down_and_suppressed_up_requires_rollback_even_if_finally_empty() {
        let snapshot = GrabEpoch {
            pointer: 10,
            keyboard: 11,
        };
        let grab = GrabEpoch {
            pointer: 20,
            keyboard: 21,
        };
        let end = GrabEpoch {
            pointer: 30,
            keyboard: 31,
        };
        for keyboard in [false, true] {
            let mut crossing = AdmissionCrossing::new(snapshot, grab);
            let mut edge = RawEdge {
                keyboard,
                detail: 1,
                down: true,
                deviceid: 2,
                sourceid: 3,
            };
            crossing.note(15, edge);
            edge.down = false;
            crossing.note(25, edge);
            assert!(
                crossing.crossed(),
                "a suppressed up cannot undo a previously local down"
            );
            assert!(
                suppressed_between(grab, end, 25, keyboard),
                "rollback must replay this up locally"
            );
            assert!(
                !suppressed_between(grab, end, 15, keyboard),
                "never replay the local down"
            );
            assert!(
                !suppressed_between(grab, end, 31, keyboard),
                "post-ungrab events are already local"
            );
            let mut completed = AdmissionCrossing::new(snapshot, grab);
            edge.down = true;
            completed.note(15, edge);
            edge.down = false;
            completed.note(16, edge);
            assert!(
                !completed.crossed(),
                "a complete local tap before grab is not a crossing"
            );
            let mut fresh = AdmissionCrossing::new(snapshot, grab);
            edge.down = true;
            fresh.note(25, edge);
            assert!(
                !fresh.crossed(),
                "fresh suppressed input must stay deliverable"
            );
        }
        let partial = GrabEpoch {
            pointer: 20,
            keyboard: u64::MAX,
        };
        assert!(suppressed_between(partial, end, 25, false));
        assert!(
            !suppressed_between(partial, end, 25, true),
            "refused keyboard grab never suppressed keys"
        );
    }

    #[test]
    fn switch_uses_original_native_issuance_through_wrap_and_delayed_receipt() {
        let start = Instant::now();
        let clock = ServerClock {
            sequence: 100,
            time: u32::MAX - 50,
            sent_at: start,
        };
        let expected = start + Duration::from_millis(99);
        for delay in [101, 200, 498] {
            assert_eq!(
                timed_hotkey(
                    1,
                    Some(clock),
                    101,
                    49,
                    start + Duration::from_millis(delay)
                ),
                Some(CapturedEvent::HotkeyPressed(1, expected))
            );
        }
        assert_eq!(
            timed_hotkey(1, Some(clock), 101, 49, start + Duration::from_millis(499)),
            None
        );
        assert_eq!(
            timed_hotkey(1, None, 101, 49, start + Duration::from_millis(101)),
            None
        );
        assert_eq!(
            clock.issued_at(99, 49, start + Duration::from_millis(101)),
            None
        );
        assert_eq!(
            clock.issued_at(101, 49, start + Duration::from_secs(61)),
            None
        );
        assert_eq!(
            clock.issued_at(101, 49, start + Duration::from_millis(98)),
            None
        );
        assert_eq!(
            clock.issued_at(101, u32::MAX - 51, start + Duration::from_millis(101)),
            None
        );
    }

    #[test]
    fn native_issuance_precedes_cancellation_and_old_config_edges_cannot_rearm() {
        let start = Instant::now();
        let clock = ServerClock {
            sequence: 10,
            time: 1000,
            sent_at: start,
        };
        let cancelled_at = start + Duration::from_millis(100);
        let Some(CapturedEvent::HotkeyPressed(1, issued_at)) =
            timed_hotkey(1, Some(clock), 11, 1050, start + Duration::from_millis(200))
        else {
            panic!("fresh native timestamp must be preserved")
        };
        assert!(
            issued_at < cancelled_at,
            "queued receipt must not cross the cancellation watermark"
        );
        let mut hotkeys = crate::hotkeys::HotkeyState::default();
        hotkeys.bindings[1] = Some(crate::hotkeys::Hotkey {
            keycode: 38,
            modifiers: 0,
        });
        assert_eq!(hotkey_edge(&mut hotkeys, 20, 19, 38, true), (false, None));
        assert_eq!(hotkey_edge(&mut hotkeys, 20, 19, 38, false), (false, None));
        assert_eq!(hotkey_edge(&mut hotkeys, 20, 20, 38, true), (true, Some(1)));
        assert_eq!(hotkey_edge(&mut hotkeys, 20, 20, 38, false), (true, None));
        assert!(matches!(
            timed_hotkey(2, None, 21, 0, start),
            Some(CapturedEvent::HotkeyPressed(2, _))
        ));
    }
}
