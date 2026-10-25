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
    HotkeyPressed(u8),
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
        let mask = vec![
            xinput::XIEventMask::RAW_MOTION,
            xinput::XIEventMask::RAW_KEY_PRESS,
            xinput::XIEventMask::RAW_KEY_RELEASE,
            xinput::XIEventMask::RAW_BUTTON_PRESS,
            xinput::XIEventMask::RAW_BUTTON_RELEASE,
        ];
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: XI_ALL_DEVICES,
                mask,
            }],
        )?;
        conn.flush()?;
        let start = query_pointer(&conn, root)?;
        Ok(Self {
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
        })
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
        let physical = self.conn.query_keymap()?.reply()?.keys;
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

    /// Reads the real pointer position back from the server.
    pub fn pointer_position(&self) -> Result<Position, X11Error> {
        query_pointer(&self.conn, self.root)
    }

    /// Takes the pointer and keyboard grab with `owner_events = false`.
    ///
    /// On *any* failure both grabs are released again and the error is returned, so
    /// the caller can cancel the transfer knowing the pointer is not half-grabbed.
    pub fn grab(&mut self) -> Result<(), X11Error> {
        if self.grab_active {
            return Ok(());
        }
        if let Err(err) = self.take_grabs() {
            self.drop_grabs();
            return Err(err);
        }
        self.grab_active = true;
        let position = self
            .pointer_position()
            .unwrap_or_else(|_| screen_centre(self.root_width, self.root_height));
        self.integrated = position;
        self.local = position;
        Ok(())
    }

    /// Releases both grabs if they are held. Safe to call more than once.
    pub fn ungrab(&mut self) {
        self.drop_grabs();
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
        loop {
            if let Some(event) = self.conn.poll_for_event()? {
                if let Some(captured) = self.translate(event) {
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

    fn take_grabs(&self) -> Result<(), X11Error> {
        let pointer = self
            .conn
            .grab_pointer(
                false,
                self.root,
                xproto::EventMask::NO_EVENT,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                x11rb::CURRENT_TIME,
            )?
            .reply()?;
        if pointer.status != xproto::GrabStatus::SUCCESS {
            return Err(X11Error::Internal(format!(
                "XGrabPointer was refused (status {})",
                u8::from(pointer.status)
            )));
        }
        let keyboard = self
            .conn
            .grab_keyboard(
                false,
                self.root,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )?
            .reply()?;
        if keyboard.status != xproto::GrabStatus::SUCCESS {
            return Err(X11Error::Internal(format!(
                "XGrabKeyboard was refused (status {})",
                u8::from(keyboard.status)
            )));
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Best-effort release of both grabs. Never fails.
    fn drop_grabs(&mut self) {
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
        self.grab_active = false;
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

    fn translate(&mut self, event: Event) -> Option<CapturedEvent> {
        match event {
            Event::XinputRawMotion(raw) => self.capture_motion(&raw),
            Event::XinputRawKeyPress(raw) => self.capture_key(&raw, KeyState::Down),
            Event::XinputRawKeyRelease(raw) => self.capture_key(&raw, KeyState::Up),
            Event::XinputRawButtonPress(raw) => self.capture_button(&raw, KeyState::Down),
            Event::XinputRawButtonRelease(raw) => self.capture_button(&raw, KeyState::Up),
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
    ) -> Option<CapturedEvent> {
        self.last_event_deviceid = raw.deviceid;
        if self.is_own_injection(raw.deviceid, raw.sourceid) {
            self.filtered_injected += 1;
            return None;
        }
        let keycode = u8::try_from(raw.detail).ok()?;
        let (swallow, action) = self.hotkeys.edge(keycode, state.is_down());
        if let Some(action) = action {
            if action == 0 {
                self.emergencies += 1;
                self.fire_emergency();
                return Some(CapturedEvent::Emergency);
            }
            return Some(CapturedEvent::HotkeyPressed(action));
        }
        if swallow {
            return None;
        }
        self.ledger.note_key(keycode, state);
        let key = x11_keycode_to_hid_key(keycode)?;
        Some(CapturedEvent::Input(InputEvent::Key { key, state }))
    }

    fn capture_button(
        &mut self,
        raw: &xinput::RawButtonPressEvent,
        state: KeyState,
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
            return Some(CapturedEvent::Input(InputEvent::Scroll { h120, v120 }));
        }
        let kind = x11_button_to_mouse_button(button)?;
        self.ledger.note_button(button, state);
        Some(CapturedEvent::Input(InputEvent::Button {
            button: kind,
            state,
        }))
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
        self.ungrab();
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
        self.ungrab();
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
