//! Local input capture on Windows (KVM-010).
//!
//! # Structure
//!
//! One dedicated thread owns everything the OS attaches to a thread:
//!
//! * two low-level hooks, `WH_KEYBOARD_LL` and `WH_MOUSE_LL`;
//! * a message-only window and the Raw Input registration that targets it;
//! * a 50 ms timer used for the cursor hold;
//! * the message loop that both the hooks and the window need in order to run.
//!
//! The hook callbacks do exactly three things: inspect the mode/injected flags,
//! detect the emergency combination, and push a pre-allocated event into a
//! bounded channel with `try_send`. They never wait for the agent, never touch
//! the network, never allocate and never call a blocking API — everything the
//! agent needs to decide is an atomic read in this process.
//!
//! # Recognition of our own injections
//!
//! A hook event is ours when `dwExtraInfo == [`crate::INJECT_TAG`]`, or when the
//! OS marks it injected (`LLKHF_INJECTED` / `LLMHF_INJECTED`) because it came from
//! some other injector. In both cases the event is neither captured nor
//! swallowed: synthetic events belong to the local desktop.
//!
//! Raw Input motion has no such marker, so it is filtered by
//! [`crate::synth`]'s budget instead (see the crate-level notes).
//!
//! # Cursor hold
//!
//! Entering [`CaptureKind::Remote`] records the current cursor position. From
//! then on the mouse hook swallows `WM_MOUSEMOVE` and the timer re-warps the
//! cursor to the recorded point if anything else moved it. See the crate-level
//! notes for why the warp is not done inside the hook.

use std::cell::RefCell;
use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{w, Error};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, KillTimer, PostQuitMessage, PostThreadMessageW, RegisterClassW, SetCursorPos,
    SetTimer, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY,
    WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WNDCLASS_STYLES,
};

use crate::event::EmergencyCombo;
use crate::{hid, synth, INJECT_TAG};
use input_model::{CaptureKind, HidKey, InputEvent, KeyState, MouseButton, Position};

/// Windows uses this `dwExtraInfo` value for the fake Shift that `PrintScreen`
/// and `Pause` synthesise; it is not a real keystroke and is skipped.
const FAKE_SHIFT_TAG: usize = 1;

/// Timer id for the cursor hold.
const HOLD_TIMER_ID: usize = 0x4C57;
/// Cursor hold re-assert period, milliseconds.
const HOLD_TIMER_MS: u32 = 50;

const MOD_CTRL: u32 = 1 << 0;
const MOD_SHIFT: u32 = 1 << 1;
const MOD_ALT: u32 = 1 << 2;

/// Everything the capture thread publishes to the agent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CaptureEvent {
    /// A normalised input event.
    Input(InputEvent),
    /// The emergency combination fired; local mode has already been restored.
    Emergency,
    /// Action index (one through three); the trigger and its release are swallowed.
    HotkeyPressed(u8),
    /// The bounded queue overflowed; the agent must end the remote session.
    QueueOverflow,
}

/// Capture configuration.
#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    /// Starting mode.
    ///
    /// [`CaptureKind::Temporary`] observes local input without suppressing it.
    /// Keys, buttons and scroll are not queued in that mode: replaying them after
    /// commit would duplicate input already delivered to the local desktop.
    pub mode: CaptureKind,
    /// Emergency combination.
    pub emergency: EmergencyCombo,
    /// Bound of the event queue. The architecture document fixes it at 4096.
    pub queue_capacity: usize,
    /// Whether to register for Raw Input mouse deltas.
    pub raw_input: bool,
    /// Development-only opt-in: accept injected events except this process's own.
    ///
    /// Off by default. Own `SendInput` carries `INJECT_TAG` and is always filtered,
    /// even when this option is enabled.
    pub capture_injected: bool,
}
impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: CaptureKind::Observe,
            emergency: EmergencyCombo::default(),
            queue_capacity: 4096,
            raw_input: true,
            capture_injected: false,
        }
    }
}

/// Counters the agent can poll at any time. Cheap atomic reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    /// Events queued to the agent.
    pub captured: u64,
    /// Mapped discrete edges offered to the capture queue (including dropped edges).
    pub edges: input_model::EdgeCounters,
    /// Events dropped because the queue was full or the consumer was gone.
    pub dropped: u64,
    /// Keyboard events whose scancode has no HID usage in the table, including scan code 0.
    pub unmapped_keys: u64,
    /// Events the OS flagged as injected (ours or another injector's).
    pub injected_seen: u64,
    /// Valid keyboard-hook messages seen.
    pub keyboard_hook_events: u64,
    /// Keyboard-hook messages the OS flagged as injected.
    pub keyboard_injected_seen: u64,
    /// Keyboard-hook messages rejected by the injected-event filter.
    pub keyboard_filtered: u64,
    /// Keyboard-hook messages mapped from scancode to HID usage.
    pub keyboard_mapped: u64,
    /// Keyboard-hook messages swallowed while local input was suppressed.
    pub keyboard_suppressed: u64,
    /// `WM_MOUSEMOVE` events seen by the low-level hook.
    pub hook_mouse_moves: u64,
    /// Relative motion events delivered by Raw Input.
    pub raw_mouse_moves: u64,
    /// Raw Input motion reported as absolute (digitizer/tablet); not converted.
    pub raw_absolute: u64,
    /// Raw Input motion dropped because it was ours ([`crate::synth`]).
    pub raw_synth_suppressed: u64,
    /// Times the cursor hold re-warped the pointer back to the hold point.
    pub recenters: u64,
    /// Times the emergency combination fired.
    pub emergencies: u64,
    /// Whether the queue has overflowed at least once.
    pub overflowed: bool,
    /// Whether the Raw Input registration succeeded.
    pub raw_input_ok: bool,
    /// Times a low-level hook callback ran (keyboard + mouse). Zero means the
    /// hooks never fired, which on an idle machine is the normal result.
    pub hook_calls: u64,
    /// Whether this capture accepts externally injected hook events.
    pub capture_injected: bool,
    /// Active hook mode.
    pub mode: CaptureKind,
}

/// Why capture could not be started.
#[derive(Clone, Debug)]
pub enum CaptureError {
    /// The capture thread could not be spawned.
    ThreadSpawn,
    /// The thread did not report readiness in time; it is detached.
    StartupTimeout,
    /// `GetModuleHandleW` failed.
    ModuleHandle(Error),
    /// Class registration failed for a reason other than "already registered".
    WindowClass(Error),
    /// The message-only window could not be created.
    CreateWindow(Error),
    /// `WH_KEYBOARD_LL` could not be installed.
    HookKeyboard(Error),
    /// `WH_MOUSE_LL` could not be installed.
    HookMouse(Error),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn => f.write_str("could not spawn the capture thread"),
            Self::StartupTimeout => f.write_str("capture thread did not become ready in time"),
            Self::ModuleHandle(e) => write!(f, "GetModuleHandleW failed: {e}"),
            Self::WindowClass(e) => write!(f, "RegisterClassW failed: {e}"),
            Self::CreateWindow(e) => write!(f, "CreateWindowExW failed: {e}"),
            Self::HookKeyboard(e) => write!(f, "SetWindowsHookExW(WH_KEYBOARD_LL) failed: {e}"),
            Self::HookMouse(e) => write!(f, "SetWindowsHookExW(WH_MOUSE_LL) failed: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// State shared between the caller and the capture thread.
struct Shared {
    tx: SyncSender<CaptureEvent>,
    mode: AtomicU8,
    /// Packed hold point `(x << 32) | y`.
    hold: AtomicI64,
    hold_valid: AtomicBool,
    emergency_fired: AtomicBool,
    modifiers: AtomicU32,
    hotkeys: AtomicU64,
    swallowed_keys: [AtomicU64; 4],
    captured: AtomicU64,
    dropped: AtomicU64,
    unmapped_keys: AtomicU64,
    injected_seen: AtomicU64,
    hook_mouse_moves: AtomicU64,
    raw_mouse_moves: AtomicU64,
    raw_absolute: AtomicU64,
    raw_synth_suppressed: AtomicU64,
    recenters: AtomicU64,
    emergencies: AtomicU64,
    overflowed: AtomicBool,
    overflow_pending: AtomicBool,
    emergency_pending: AtomicBool,
    raw_input_ok: AtomicBool,
    hook_calls: AtomicU64,
    capture_injected: AtomicBool,
    keyboard_hook_events: AtomicU64,
    keyboard_injected_seen: AtomicU64,
    keyboard_filtered: AtomicU64,
    keyboard_mapped: AtomicU64,
    keyboard_suppressed: AtomicU64,
    keys_down: AtomicU64,
    keys_up: AtomicU64,
    buttons_down: AtomicU64,
    buttons_up: AtomicU64,
    wheel_v: AtomicU64,
    wheel_h: AtomicU64,
}

// Windows invokes low-level hooks on their installing thread; the Raw Input
// window is pumped on that same thread. Each capture owns its callback context,
// including after stop/start and when a takeover detector runs alongside it.
thread_local! {
    static CTX: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

/// What the cursor veil polls (see `crate::veil`): lock-free, no hook-thread involvement.
#[derive(Clone)]
pub struct CursorProbe {
    shared: Arc<Shared>,
}

impl CursorProbe {
    /// The held cursor point, `Some` only while a Remote capture holds it.
    #[must_use]
    pub fn hold_point(&self) -> Option<(i32, i32)> {
        remote_hold(&self.shared)
    }
}

/// A running capture.
pub struct Capture {
    rx: Receiver<CaptureEvent>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    thread_id: Arc<AtomicU32>,
}

impl Capture {
    /// Install hooks and start the capture thread.
    ///
    /// Returns only once both hooks are installed (or with an error), so that
    /// `begin_capture` can be atomic with respect to local suppression: the
    /// caller never believes it holds input while the hooks are not there yet.
    pub fn start(config: CaptureConfig) -> Result<Self, CaptureError> {
        let (tx, rx) = mpsc::sync_channel(config.queue_capacity.max(16));
        let shared = Arc::new(Shared {
            tx,
            mode: AtomicU8::new(config.mode as u8),
            hold: AtomicI64::new(0),
            hold_valid: AtomicBool::new(false),
            emergency_fired: AtomicBool::new(false),
            modifiers: AtomicU32::new(0),
            hotkeys: AtomicU64::new(
                crate::event::Hotkeys::new([Some(config.emergency), None, None, None, None]).0,
            ),
            swallowed_keys: std::array::from_fn(|_| AtomicU64::new(0)),
            keyboard_hook_events: AtomicU64::new(0),
            keyboard_injected_seen: AtomicU64::new(0),
            keyboard_filtered: AtomicU64::new(0),
            keyboard_mapped: AtomicU64::new(0),
            keyboard_suppressed: AtomicU64::new(0),
            keys_down: AtomicU64::new(0),
            keys_up: AtomicU64::new(0),
            buttons_down: AtomicU64::new(0),
            buttons_up: AtomicU64::new(0),
            wheel_v: AtomicU64::new(0),
            wheel_h: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            unmapped_keys: AtomicU64::new(0),
            injected_seen: AtomicU64::new(0),
            hook_mouse_moves: AtomicU64::new(0),
            raw_mouse_moves: AtomicU64::new(0),
            raw_absolute: AtomicU64::new(0),
            capture_injected: AtomicBool::new(config.capture_injected),
            raw_synth_suppressed: AtomicU64::new(0),
            recenters: AtomicU64::new(0),
            emergencies: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
            overflow_pending: AtomicBool::new(false),
            emergency_pending: AtomicBool::new(false),
            raw_input_ok: AtomicBool::new(false),
            hook_calls: AtomicU64::new(0),
        });

        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_id = Arc::new(AtomicU32::new(0));
        let thread_shared = Arc::clone(&shared);
        let thread_id_out = Arc::clone(&thread_id);

        let thread = thread::Builder::new()
            .name("lws-capture".to_owned())
            .spawn(move || run(config, thread_shared, ready_tx, thread_id_out))
            .map_err(|_| CaptureError::ThreadSpawn)?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                rx,
                shared,
                thread: Some(thread),
                thread_id,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => Err(CaptureError::StartupTimeout),
        }
    }

    /// Current physical cursor position, used once to seed a fresh source capture.
    pub fn cursor_position() -> Result<Position, Error> {
        let mut point = POINT::default();
        // SAFETY: GetCursorPos writes into this valid POINT.
        unsafe { GetCursorPos(&mut point) }?;
        Ok(Position::new(f64::from(point.x), f64::from(point.y)))
    }

    /// Event stream. Use the convenience methods below instead of draining this
    /// in a hot loop when a bounded wait is enough.
    #[must_use]
    pub fn events(&self) -> &Receiver<CaptureEvent> {
        &self.rx
    }

    /// Wait up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<CaptureEvent, RecvTimeoutError> {
        if let Some(control) = self.take_control() {
            return Ok(control);
        }
        self.rx.recv_timeout(timeout)
    }

    /// Non-blocking poll.
    pub fn try_recv(&self) -> Option<CaptureEvent> {
        self.take_control().or_else(|| self.rx.try_recv().ok())
    }

    /// Safety signals never compete with input for bounded queue capacity.
    fn take_control(&self) -> Option<CaptureEvent> {
        if self.shared.emergency_pending.swap(false, Ordering::SeqCst) {
            Some(CaptureEvent::Emergency)
        } else if self.shared.overflow_pending.swap(false, Ordering::SeqCst) {
            Some(CaptureEvent::QueueOverflow)
        } else {
            None
        }
    }

    /// Current mode.
    #[must_use]
    pub fn mode(&self) -> CaptureKind {
        capture_kind(&self.shared)
    }

    /// Replaces all combinations atomically without interrupting capture or held-key swallowing.
    pub fn set_hotkeys(&self, hotkeys: crate::event::Hotkeys) {
        self.shared.hotkeys.store(hotkeys.0, Ordering::SeqCst);
    }

    /// Switch mode.
    ///
    /// Entering [`CaptureKind::Remote`] snapshots the cursor position as the hold
    /// point; leaving it releases the hold. Temporary leaves all local input
    /// untouched. The mode flip is a single atomic store.
    pub fn set_mode(&self, mode: CaptureKind) {
        match mode {
            CaptureKind::Remote => {
                let mut point = POINT { x: 0, y: 0 };
                // SAFETY: `GetCursorPos` writes into a valid `POINT`.
                if unsafe { GetCursorPos(&mut point) }.is_ok() {
                    self.shared
                        .hold
                        .store(pack(point.x, point.y), Ordering::SeqCst);
                    self.shared.hold_valid.store(true, Ordering::SeqCst);
                } else {
                    self.shared.hold_valid.store(false, Ordering::SeqCst);
                }
            }
            CaptureKind::Observe | CaptureKind::Temporary => {
                self.shared.hold_valid.store(false, Ordering::SeqCst);
            }
        }
        self.shared.emergency_fired.store(false, Ordering::SeqCst);
        self.shared.mode.store(mode as u8, Ordering::SeqCst);
    }

    /// The point the cursor is being held at, if Remote.
    #[must_use]
    pub fn hold_point(&self) -> Option<(i32, i32)> {
        if self.shared.hold_valid.load(Ordering::SeqCst) {
            Some(unpack(self.shared.hold.load(Ordering::SeqCst)))
        } else {
            None
        }
    }

    /// A read-only view of the hold for the cursor veil's own thread.
    #[must_use]
    pub fn cursor_probe(&self) -> CursorProbe {
        CursorProbe {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Snapshot the counters.
    #[must_use]
    pub fn stats(&self) -> CaptureStats {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        CaptureStats {
            keyboard_hook_events: load(&self.shared.keyboard_hook_events),
            keyboard_injected_seen: load(&self.shared.keyboard_injected_seen),
            keyboard_filtered: load(&self.shared.keyboard_filtered),
            keyboard_mapped: load(&self.shared.keyboard_mapped),
            keyboard_suppressed: load(&self.shared.keyboard_suppressed),
            edges: input_model::EdgeCounters {
                keys_down: load(&self.shared.keys_down),
                keys_up: load(&self.shared.keys_up),
                buttons_down: load(&self.shared.buttons_down),
                buttons_up: load(&self.shared.buttons_up),
                wheel_v: load(&self.shared.wheel_v),
                wheel_h: load(&self.shared.wheel_h),
            },
            capture_injected: self.shared.capture_injected.load(Ordering::Relaxed),
            mode: capture_kind(&self.shared),
            captured: load(&self.shared.captured),
            dropped: load(&self.shared.dropped),
            unmapped_keys: load(&self.shared.unmapped_keys),
            injected_seen: load(&self.shared.injected_seen),
            hook_mouse_moves: load(&self.shared.hook_mouse_moves),
            raw_mouse_moves: load(&self.shared.raw_mouse_moves),
            raw_absolute: load(&self.shared.raw_absolute),
            raw_synth_suppressed: load(&self.shared.raw_synth_suppressed),
            recenters: load(&self.shared.recenters),
            emergencies: load(&self.shared.emergencies),
            overflowed: self.shared.overflowed.load(Ordering::Relaxed),
            raw_input_ok: self.shared.raw_input_ok.load(Ordering::Relaxed),
            hook_calls: self.shared.hook_calls.load(Ordering::Relaxed),
        }
    }
    /// Controls whether externally injected events are accepted by the hooks.
    pub fn set_capture_injected(&self, capture: bool) {
        self.shared
            .capture_injected
            .store(capture, Ordering::Relaxed);
    }

    /// Unhook, tear the window down and join the thread.
    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let id = self.thread_id.load(Ordering::SeqCst);
            if id != 0 {
                // SAFETY: posting a thread message to a thread we own.
                let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            }
            let _ = thread.join();
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Thread entry point: install, report readiness, pump messages, clean up.
fn run(
    config: CaptureConfig,
    shared: Arc<Shared>,
    ready: mpsc::Sender<Result<(), CaptureError>>,
    thread_id_out: Arc<AtomicU32>,
) {
    // SAFETY: every call below is a documented Win32 call with checked results;
    // the whole body runs on the dedicated capture thread and unhooks on exit.
    let installed = unsafe { install(config, &shared, &ready, &thread_id_out) };
    if !installed {
        return;
    }

    let mut message = MSG::default();
    // SAFETY: standard message pump; it ends when WM_QUIT arrives.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    teardown();
}

/// Install hooks/window/timer, send readiness. Returns `false` when installation
/// failed (the error has already been reported to the caller).
unsafe fn install(
    config: CaptureConfig,
    shared: &Arc<Shared>,
    ready: &mpsc::Sender<Result<(), CaptureError>>,
    thread_id_out: &Arc<AtomicU32>,
) -> bool {
    // SAFETY: `GetModuleHandleW(None)` asks for the current module.
    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => HINSTANCE(module.0),
        Err(error) => {
            let _ = ready.send(Err(CaptureError::ModuleHandle(error)));
            return false;
        }
    };

    let class_name = w!("LwsSpikeCaptureWindow");
    let window_class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class_name,
        ..Default::default()
    };

    // SAFETY: `window_class` outlives the call; the class name is copied by the OS.
    if unsafe { RegisterClassW(&window_class) } == 0 {
        // SAFETY: read-only query of the last error.
        let already = unsafe { GetLastError() } == ERROR_CLASS_ALREADY_EXISTS;
        if !already {
            let _ = ready.send(Err(CaptureError::WindowClass(Error::from_thread())));
            return false;
        }
    }

    // A message-only window is enough: Raw Input needs a target window, not a
    // visible one, and it must never appear to the user.
    // SAFETY: class and instance are valid, parent is HWND_MESSAGE.
    let window = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("lws-capture"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    } {
        Ok(window) => window,
        Err(error) => {
            let _ = ready.send(Err(CaptureError::CreateWindow(error)));
            return false;
        }
    };

    if config.raw_input {
        let devices = [RAWINPUTDEVICE {
            usUsagePage: 0x01, // generic desktop controls
            usUsage: 0x02,     // mouse
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        }];
        // SAFETY: one fully initialised device entry with its correct size.
        let registered =
            unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) };
        shared
            .raw_input_ok
            .store(registered.is_ok(), Ordering::SeqCst);
        // Raw Input failing is not fatal: hooks still carry keys and buttons.
    }

    // SAFETY: hook callbacks live in this process and outlive the hook.
    let keyboard = match unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(instance), 0)
    } {
        Ok(hook) => hook,
        Err(error) => {
            let _ = ready.send(Err(CaptureError::HookKeyboard(error)));
            return false;
        }
    };

    // SAFETY: as above.
    let mouse = match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(instance), 0) }
    {
        Ok(hook) => hook,
        Err(error) => {
            // SAFETY: `keyboard` is a hook we just installed.
            let _ = unsafe { UnhookWindowsHookEx(keyboard) };
            let _ = ready.send(Err(CaptureError::HookMouse(error)));
            return false;
        }
    };

    HOOKS.with(|h| {
        *h.borrow_mut() = Some(Hooks {
            keyboard,
            mouse,
            window,
        });
    });
    CTX.with(|context| *context.borrow_mut() = Some(Arc::clone(shared)));

    // SAFETY: thread id of the calling thread.
    thread_id_out.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    // SAFETY: `None` selects the default (message-based) timer.
    let _ = unsafe { SetTimer(Some(window), HOLD_TIMER_ID, HOLD_TIMER_MS, None) };

    let _ = ready.send(Ok(()));
    true
}

/// Handle to the OS objects owned by the capture thread.
struct Hooks {
    keyboard: HHOOK,
    mouse: HHOOK,
    window: HWND,
}

thread_local! {
    /// The hooks and window to tear down on thread exit.
    static HOOKS: RefCell<Option<Hooks>> = const { RefCell::new(None) };
}

fn teardown() {
    let hooks = HOOKS.with(|h| h.borrow_mut().take());
    if let Some(hooks) = hooks {
        // SAFETY: all three handles belong to this thread and are still live.
        unsafe {
            let _ = KillTimer(Some(hooks.window), HOLD_TIMER_ID);
            let _ = UnhookWindowsHookEx(hooks.mouse);
            let _ = UnhookWindowsHookEx(hooks.keyboard);
            let _ = DestroyWindow(hooks.window);
        }
    }
    CTX.with(|context| *context.borrow_mut() = None);
}

fn with_shared<R>(f: impl FnOnce(&Shared) -> R) -> Option<R> {
    CTX.with(|context| context.borrow().as_deref().map(f))
}

fn push(shared: &Shared, event: InputEvent) {
    shared.captured.fetch_add(1, Ordering::Relaxed);
    let edge = match &event {
        InputEvent::Key {
            state: KeyState::Down,
            ..
        } => Some(&shared.keys_down),
        InputEvent::Key {
            state: KeyState::Up,
            ..
        } => Some(&shared.keys_up),
        InputEvent::Button {
            state: KeyState::Down,
            ..
        } => Some(&shared.buttons_down),
        InputEvent::Button {
            state: KeyState::Up,
            ..
        } => Some(&shared.buttons_up),
        // The wheel has no edge to balance, but it is the input whose loss is otherwise
        // invisible: count one per axis that carried units, at the hook itself.
        InputEvent::Scroll { h120, v120 } => {
            for (units, counter) in [(v120, &shared.wheel_v), (h120, &shared.wheel_h)] {
                if *units != 0 {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
            None
        }
        _ => None,
    };
    if let Some(counter) = edge {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    match shared.tx.try_send(CaptureEvent::Input(event)) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            shared.overflowed.store(true, Ordering::Relaxed);
            // Restore local suppression immediately, even if the consumer is stalled.
            shared
                .mode
                .store(CaptureKind::Observe as u8, Ordering::SeqCst);
            shared.hold_valid.store(false, Ordering::SeqCst);
            shared.overflow_pending.store(true, Ordering::SeqCst);
        }
        Err(TrySendError::Disconnected(_)) => {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn pack(x: i32, y: i32) -> i64 {
    (i64::from(x) << 32) | i64::from(y as u32)
}

fn unpack(value: i64) -> (i32, i32) {
    ((value >> 32) as i32, value as u32 as i32)
}

/// Decode the atomic mode byte back into a [`CaptureKind`].
fn capture_kind(shared: &Shared) -> CaptureKind {
    match shared.mode.load(Ordering::Relaxed) {
        mode if mode == CaptureKind::Temporary as u8 => CaptureKind::Temporary,
        mode if mode == CaptureKind::Remote as u8 => CaptureKind::Remote,
        _ => CaptureKind::Observe,
    }
}

/// Only a committed Remote capture suppresses local input.
fn remote(shared: &Shared) -> bool {
    capture_kind(shared).is_suppressed()
}

/// Where the cursor is held, only while a committed Remote capture holds it. The cursor veil
/// follows exactly this: `None` in Local (`Observe`), Preparing (`Temporary`), after an
/// emergency / expiry / stop. It is plain atomic loads, safe from any thread.
fn remote_hold(shared: &Shared) -> Option<(i32, i32)> {
    if remote(shared) && shared.hold_valid.load(Ordering::SeqCst) {
        Some(unpack(shared.hold.load(Ordering::SeqCst)))
    } else {
        None
    }
}

fn update_modifiers(shared: &Shared, vk: u16, down: bool) {
    let bit = if vk == VK_LCONTROL.0 || vk == VK_RCONTROL.0 {
        MOD_CTRL
    } else if vk == VK_LSHIFT.0 || vk == VK_RSHIFT.0 {
        MOD_SHIFT
    } else if vk == VK_LMENU.0 || vk == VK_RMENU.0 {
        MOD_ALT
    } else {
        return;
    };

    if down {
        shared.modifiers.fetch_or(bit, Ordering::Relaxed);
    } else {
        shared.modifiers.fetch_and(!bit, Ordering::Relaxed);
    }
}

fn physical_modifiers() -> u32 {
    let down = |left, right| {
        // SAFETY: querying current key state has no input side effects.
        unsafe { GetAsyncKeyState(left) < 0 || GetAsyncKeyState(right) < 0 }
    };
    (if down(i32::from(VK_LCONTROL.0), i32::from(VK_RCONTROL.0)) {
        MOD_CTRL
    } else {
        0
    }) | (if down(i32::from(VK_LMENU.0), i32::from(VK_RMENU.0)) {
        MOD_ALT
    } else {
        0
    }) | (if down(i32::from(VK_LSHIFT.0), i32::from(VK_RSHIFT.0)) {
        MOD_SHIFT
    } else {
        0
    })
}

/// Restore local mode unconditionally. This is the one path that must work even
/// if everything else in the agent is stuck: atomic stores and a separate control flag.
fn fire_emergency(shared: &Shared) {
    shared
        .mode
        .store(CaptureKind::Observe as u8, Ordering::SeqCst);
    shared.hold_valid.store(false, Ordering::SeqCst);
    if !shared.emergency_fired.swap(true, Ordering::SeqCst) {
        shared.emergencies.fetch_add(1, Ordering::Relaxed);
        shared.emergency_pending.store(true, Ordering::SeqCst);
    }
}

/// Handle one keyboard hook event. `Some(true)` means "swallow".
fn handle_keyboard(shared: &Shared, wparam: WPARAM, lparam: LPARAM) -> Option<bool> {
    let message = wparam.0 as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    if !down && message != WM_KEYUP && message != WM_SYSKEYUP {
        return None;
    }
    shared.keyboard_hook_events.fetch_add(1, Ordering::Relaxed);

    // SAFETY: for HC_ACTION the OS passes a valid KBDLLHOOKSTRUCT pointer.
    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

    if kb.dwExtraInfo == FAKE_SHIFT_TAG {
        return None;
    }
    let own_tag = kb.dwExtraInfo == INJECT_TAG;
    let injected = own_tag || kb.flags.0 & LLKHF_INJECTED.0 != 0;
    if injected {
        shared.injected_seen.fetch_add(1, Ordering::Relaxed);
        shared
            .keyboard_injected_seen
            .fetch_add(1, Ordering::Relaxed);
    }
    if !should_capture_event(
        injected,
        own_tag,
        shared.capture_injected.load(Ordering::Relaxed),
    ) {
        shared.keyboard_filtered.fetch_add(1, Ordering::Relaxed);
        return None;
    }

    let vk = kb.vkCode as u16;
    if !injected {
        update_modifiers(shared, vk, down);
    }

    let bucket = &shared.swallowed_keys[usize::from(vk / 64).min(3)];
    let bit = 1u64 << (vk % 64);
    if !injected && bucket.load(Ordering::Relaxed) & bit != 0 {
        if !crate::event::hotkey_transition(true, down, None).0 {
            bucket.fetch_and(!bit, Ordering::Relaxed);
        }
        return Some(true);
    }
    let hotkeys = crate::event::Hotkeys(shared.hotkeys.load(Ordering::SeqCst));
    if !injected && down && hotkeys.contains_key(vk) {
        let mask = shared.modifiers.load(Ordering::Relaxed) | physical_modifiers();
        let action = hotkeys.matching(
            vk,
            mask & MOD_CTRL != 0,
            mask & MOD_ALT != 0,
            mask & MOD_SHIFT != 0,
            // SAFETY: these queries have no input side effects.
            unsafe { GetAsyncKeyState(0x5B) < 0 || GetAsyncKeyState(0x5C) < 0 },
        );
        if let (_, Some(action)) = crate::event::hotkey_transition(false, down, action) {
            bucket.fetch_or(bit, Ordering::Relaxed);
            if action == 0 {
                fire_emergency(shared);
            } else {
                let _ = shared.tx.try_send(CaptureEvent::HotkeyPressed(action));
            }
            return Some(true);
        }
    }

    if capture_kind(shared) == CaptureKind::Temporary {
        return Some(false);
    }

    let swallow = remote(shared);
    let scancode = hid::scancode_from_hook(kb.scanCode, kb.flags.0 & LLKHF_EXTENDED.0 != 0);
    match hid::scancode_to_hid(scancode) {
        Some(usage) => {
            shared.keyboard_mapped.fetch_add(1, Ordering::Relaxed);
            if swallow {
                shared.keyboard_suppressed.fetch_add(1, Ordering::Relaxed);
            }
            push(
                shared,
                InputEvent::Key {
                    key: HidKey::keyboard(usage),
                    state: KeyState::from_down(down),
                },
            );
        }
        None => {
            shared.unmapped_keys.fetch_add(1, Ordering::Relaxed);
            if swallow {
                shared.keyboard_suppressed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    Some(swallow)
}

/// Handle one mouse hook event. `Some(true)` means "swallow".
fn handle_mouse(shared: &Shared, wparam: WPARAM, lparam: LPARAM) -> Option<bool> {
    let message = wparam.0 as u32;
    // SAFETY: for HC_ACTION the OS passes a valid MSLLHOOKSTRUCT pointer.
    let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };

    let own_tag = ms.dwExtraInfo == INJECT_TAG;
    let injected = own_tag || ms.flags & LLMHF_INJECTED != 0;
    if injected {
        shared.injected_seen.fetch_add(1, Ordering::Relaxed);
    }
    if !should_capture_event(
        injected,
        own_tag,
        shared.capture_injected.load(Ordering::Relaxed),
    ) {
        return None;
    }

    let swallow = remote(shared);
    if capture_kind(shared) == CaptureKind::Temporary && message != WM_MOUSEMOVE {
        return Some(false);
    }

    match message {
        WM_MOUSEMOVE => {
            shared.hook_mouse_moves.fetch_add(1, Ordering::Relaxed);
            if swallow {
                return Some(true);
            }
            // In Observe the cursor really follows the mouse, so the hook's
            // coordinates are the authoritative absolute position.
            push(
                shared,
                InputEvent::PointerAbsolute(Position::new(ms.pt.x as f64, ms.pt.y as f64)),
            );
            Some(false)
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => {
            let down = message == WM_LBUTTONDOWN;
            push(
                shared,
                InputEvent::Button {
                    button: MouseButton::Left,
                    state: KeyState::from_down(down),
                },
            );
            Some(swallow)
        }
        WM_RBUTTONDOWN | WM_RBUTTONUP => {
            let down = message == WM_RBUTTONDOWN;
            push(
                shared,
                InputEvent::Button {
                    button: MouseButton::Right,
                    state: KeyState::from_down(down),
                },
            );
            Some(swallow)
        }
        WM_MBUTTONDOWN | WM_MBUTTONUP => {
            let down = message == WM_MBUTTONDOWN;
            push(
                shared,
                InputEvent::Button {
                    button: MouseButton::Middle,
                    state: KeyState::from_down(down),
                },
            );
            Some(swallow)
        }
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let down = message == WM_XBUTTONDOWN;
            let which = (ms.mouseData >> 16) as u16;
            let button = if which == 1 {
                MouseButton::Back
            } else {
                MouseButton::Forward
            };
            push(
                shared,
                InputEvent::Button {
                    button,
                    state: KeyState::from_down(down),
                },
            );
            Some(swallow)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((ms.mouseData >> 16) as u16 as i16) as i32;
            let event = if message == WM_MOUSEWHEEL {
                InputEvent::Scroll {
                    h120: 0,
                    v120: delta,
                }
            } else {
                InputEvent::Scroll {
                    h120: delta,
                    v120: 0,
                }
            };
            push(shared, event);
            Some(swallow)
        }
        _ => None,
    }
}

/// Raw Input: relative mouse deltas, and the only place where a physical device
/// can be identified and where motion keeps arriving independently of the hook.
fn handle_raw_input(shared: &Shared, lparam: LPARAM) {
    let handle = HRAWINPUT(lparam.0 as *mut core::ffi::c_void);
    // 128 bytes, pointer-aligned so the RAWINPUT cast below is well aligned.
    let mut buffer = [0usize; 16];
    let mut size = size_of::<[usize; 16]>() as u32;

    // SAFETY: the buffer and its size describe the same 128 bytes.
    let written = unsafe {
        GetRawInputData(
            handle,
            RID_INPUT,
            Some(buffer.as_mut_ptr().cast::<core::ffi::c_void>()),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if written == u32::MAX || written == 0 || written > buffer.len() as u32 * 8 {
        return;
    }

    // SAFETY: `written` bytes were written into an aligned buffer at least that
    // large, which is a RAWINPUT for the mouse registration we made.
    let input = unsafe { &*(buffer.as_ptr().cast::<RAWINPUT>()) };
    if input.header.dwType != RIM_TYPEMOUSE.0 {
        return;
    }
    // SAFETY: the union is discriminated by dwType above.
    let mouse = unsafe { &input.data.mouse };

    let Some(event) = raw_mouse_motion(mouse.usFlags.0, mouse.lLastX, mouse.lLastY) else {
        if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
            shared.raw_absolute.fetch_add(1, Ordering::Relaxed);
        }
        return;
    };

    if synth::consume_motion() {
        shared.raw_synth_suppressed.fetch_add(1, Ordering::Relaxed);
        return;
    }

    shared.raw_mouse_moves.fetch_add(1, Ordering::Relaxed);
    push(shared, event);
}

fn raw_mouse_motion(flags: u16, dx: i32, dy: i32) -> Option<InputEvent> {
    (flags & MOUSE_MOVE_ABSOLUTE.0 == 0 && (dx != 0 || dy != 0))
        .then_some(InputEvent::PointerRelative { dx, dy })
}

/// Re-assert the cursor hold point while in Remote.
fn hold_tick(shared: &Shared) {
    if !remote(shared) || !shared.hold_valid.load(Ordering::Relaxed) {
        return;
    }
    let (target_x, target_y) = unpack(shared.hold.load(Ordering::Relaxed));
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `GetCursorPos` writes into a valid `POINT`.
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return;
    }
    if point.x != target_x || point.y != target_y {
        // SAFETY: numbers only; the call cannot fail in a way we can act on here.
        let _ = unsafe { SetCursorPos(target_x, target_y) };
        shared.recenters.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let _ = with_shared(|shared| {
            shared.hook_calls.fetch_add(1, Ordering::Relaxed);
        });
        if let Some(Some(true)) = with_shared(|shared| handle_keyboard(shared, wparam, lparam)) {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the very parameters we were called with, as required.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let _ = with_shared(|shared| {
            shared.hook_calls.fetch_add(1, Ordering::Relaxed);
        });
        if let Some(Some(true)) = with_shared(|shared| handle_mouse(shared, wparam, lparam)) {
            return LRESULT(1);
        }
    }
    // SAFETY: forwarding the very parameters we were called with, as required.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_INPUT => {
            let _ = with_shared(|shared| handle_raw_input(shared, lparam));
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == HOLD_TIMER_ID {
                let _ = with_shared(hold_tick);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: no preconditions; ends the message loop with this code.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for anything else.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}
fn should_capture_event(injected: bool, own_tag: bool, capture_injected: bool) -> bool {
    !own_tag && (!injected || capture_injected)
}

#[cfg(test)]
mod tests {
    use super::should_capture_event;

    #[test]
    fn full_queue_delivers_overflow_before_queued_input() {
        use super::*;
        let (mut shared, _) = hook_state(CaptureKind::Remote);
        let (tx, rx) = mpsc::sync_channel(1);
        shared.tx = tx;
        let capture = Capture {
            rx,
            shared: Arc::new(shared),
            thread: None,
            thread_id: Arc::new(AtomicU32::new(0)),
        };
        for _ in 0..2 {
            push(&capture.shared, InputEvent::Scroll { h120: 0, v120: 120 });
        }
        assert_eq!(capture.shared.dropped.load(Ordering::Relaxed), 1);
        assert!(matches!(
            capture.try_recv(),
            Some(CaptureEvent::QueueOverflow)
        ));
        assert_eq!(capture.mode(), CaptureKind::Observe);
        assert!(!capture.shared.hold_valid.load(Ordering::SeqCst));
    }

    #[test]
    fn emergency_is_observed_even_when_capture_queue_is_full() {
        use super::*;
        let (mut shared, _) = hook_state(CaptureKind::Remote);
        let (tx, rx) = mpsc::sync_channel(1);
        shared.tx = tx;
        let capture = Capture {
            rx,
            shared: Arc::new(shared),
            thread: None,
            thread_id: Arc::new(AtomicU32::new(0)),
        };
        push(&capture.shared, InputEvent::Scroll { h120: 0, v120: 120 });
        fire_emergency(&capture.shared);
        assert!(matches!(capture.try_recv(), Some(CaptureEvent::Emergency)));
        assert_eq!(capture.mode(), CaptureKind::Observe);
    }

    #[test]
    fn lifetime_hook_publishes_complete_set_without_resetting_swallowed_keys() {
        use super::*;
        let (shared, _) = hook_state(CaptureKind::Observe);
        let old = shared.hotkeys.load(Ordering::SeqCst);
        shared.swallowed_keys[1].store(1 << 59, Ordering::Relaxed);
        let replacement = crate::event::Hotkeys::new([
            Some(EmergencyCombo {
                vk: 0x70,
                ctrl: false,
                alt: false,
                shift: false,
            }),
            None,
            None,
            None,
            None,
        ]);
        shared.hotkeys.store(replacement.0, Ordering::SeqCst);
        assert_ne!(old, shared.hotkeys.load(Ordering::SeqCst));
        assert_eq!(shared.swallowed_keys[1].load(Ordering::Relaxed), 1 << 59);
        assert_eq!(
            replacement.matching(0x70, false, false, false, false),
            Some(0)
        );
        assert_eq!(replacement.matching(0x7b, true, true, true, false), None);
    }
    fn hook_state(
        mode: super::CaptureKind,
    ) -> (super::Shared, super::Receiver<super::CaptureEvent>) {
        use super::*;
        let (tx, rx) = mpsc::sync_channel(16);
        let shared = Shared {
            tx,
            mode: AtomicU8::new(mode as u8),
            hold: AtomicI64::new(0),
            hold_valid: AtomicBool::new(true),
            emergency_fired: AtomicBool::new(false),
            modifiers: AtomicU32::new(MOD_CTRL | MOD_ALT | MOD_SHIFT),
            hotkeys: AtomicU64::new(
                crate::event::Hotkeys::new([
                    Some(EmergencyCombo::default()),
                    None,
                    None,
                    None,
                    None,
                ])
                .0,
            ),
            swallowed_keys: std::array::from_fn(|_| AtomicU64::new(0)),
            captured: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            unmapped_keys: AtomicU64::new(0),
            injected_seen: AtomicU64::new(0),
            hook_mouse_moves: AtomicU64::new(0),
            raw_mouse_moves: AtomicU64::new(0),
            raw_absolute: AtomicU64::new(0),
            raw_synth_suppressed: AtomicU64::new(0),
            recenters: AtomicU64::new(0),
            emergencies: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
            overflow_pending: AtomicBool::new(false),
            emergency_pending: AtomicBool::new(false),
            raw_input_ok: AtomicBool::new(false),
            hook_calls: AtomicU64::new(0),
            capture_injected: AtomicBool::new(false),
            keyboard_hook_events: AtomicU64::new(0),
            keyboard_injected_seen: AtomicU64::new(0),
            keyboard_filtered: AtomicU64::new(0),
            keyboard_mapped: AtomicU64::new(0),
            keyboard_suppressed: AtomicU64::new(0),
            keys_down: AtomicU64::new(0),
            keys_up: AtomicU64::new(0),
            buttons_down: AtomicU64::new(0),
            buttons_up: AtomicU64::new(0),
            wheel_v: AtomicU64::new(0),
            wheel_h: AtomicU64::new(0),
        };
        (shared, rx)
    }

    #[test]
    fn emergency_restores_local_input_in_every_capture_mode() {
        use super::*;
        for mode in [
            CaptureKind::Observe,
            CaptureKind::Temporary,
            CaptureKind::Remote,
        ] {
            let (shared, rx) = hook_state(mode);
            let kb = KBDLLHOOKSTRUCT {
                // The emergency binding `hook_state` packed: Ctrl+Alt+Shift+F12.
                vkCode: u32::from(EmergencyCombo::default().vk),
                ..Default::default()
            };
            let press = |shared: &Shared, message: u32| {
                handle_keyboard(
                    shared,
                    WPARAM(message as usize),
                    LPARAM(&kb as *const _ as isize),
                )
            };
            // A hotkey trigger is this app's own shortcut: the hook swallows the key, on the way
            // down and on the way up, so no local application sees the combination.
            assert_eq!(press(&shared, WM_KEYDOWN), Some(true));
            assert_eq!(capture_kind(&shared), CaptureKind::Observe);
            assert!(!shared.hold_valid.load(Ordering::SeqCst));
            assert!(shared.emergency_pending.load(Ordering::SeqCst));
            assert!(rx.try_recv().is_err());
            assert!(!remote(&shared));
            assert_eq!(press(&shared, WM_KEYUP), Some(true));
        }
    }

    /// The cursor-veil invariant: the probe reports a point only for a committed Remote
    /// capture with a valid hold - never in Local (Observe) or Preparing (Temporary) mode,
    /// and not after an emergency, whatever `hold_valid` says.
    #[test]
    fn the_veil_probe_is_none_unless_the_capture_is_remote_with_a_hold() {
        use super::*;
        for mode in [CaptureKind::Observe, CaptureKind::Temporary] {
            let (shared, _rx) = hook_state(mode);
            assert_eq!(remote_hold(&shared), None, "{mode:?}");
        }
        let (shared, _rx) = hook_state(CaptureKind::Remote);
        shared.hold.store(pack(1919, -40), Ordering::SeqCst);
        assert_eq!(remote_hold(&shared), Some((1919, -40)));
        shared.hold_valid.store(false, Ordering::SeqCst);
        assert_eq!(remote_hold(&shared), None);
        shared.hold_valid.store(true, Ordering::SeqCst);
        fire_emergency(&shared);
        assert_eq!(remote_hold(&shared), None, "an emergency releases the veil");
    }

    #[test]
    fn temporary_passes_keys_buttons_and_motion_without_buffering_edges() {
        use super::*;
        let (shared, rx) = hook_state(CaptureKind::Temporary);
        let kb = KBDLLHOOKSTRUCT {
            vkCode: 0x41,
            scanCode: 0x1e,
            ..Default::default()
        };
        for message in [WM_KEYDOWN, WM_KEYUP] {
            assert_eq!(
                handle_keyboard(
                    &shared,
                    WPARAM(message as usize),
                    LPARAM(&kb as *const _ as isize)
                ),
                Some(false)
            );
        }
        let ms = MSLLHOOKSTRUCT::default();
        for message in [WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL] {
            assert_eq!(
                handle_mouse(
                    &shared,
                    WPARAM(message as usize),
                    LPARAM(&ms as *const _ as isize)
                ),
                Some(false)
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "local edges must never reach a committed peer"
        );
        assert_eq!(
            handle_mouse(
                &shared,
                WPARAM(WM_MOUSEMOVE as usize),
                LPARAM(&ms as *const _ as isize)
            ),
            Some(false)
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(CaptureEvent::Input(InputEvent::PointerAbsolute(_)))
        ));
    }

    #[test]
    fn remote_click_suppresses_and_queues_both_button_edges() {
        use super::*;
        let (shared, rx) = hook_state(CaptureKind::Remote);
        let ms = MSLLHOOKSTRUCT::default();
        for (message, state) in [
            (WM_LBUTTONDOWN, KeyState::Down),
            (WM_LBUTTONUP, KeyState::Up),
        ] {
            assert_eq!(
                handle_mouse(
                    &shared,
                    WPARAM(message as usize),
                    LPARAM(&ms as *const _ as isize)
                ),
                Some(true)
            );
            assert!(matches!(
                rx.try_recv(),
                Ok(CaptureEvent::Input(InputEvent::Button {
                    button: MouseButton::Left,
                    state: actual,
                })) if actual == state
            ));
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(shared.buttons_down.load(Ordering::Relaxed), 1);
        assert_eq!(shared.buttons_up.load(Ordering::Relaxed), 1);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0);
    }

    /// The hook end of the wheel: `WHEEL_DELTA` is 120 per notch, signed in the high word
    /// (positive is away from the user), the axis is told by the message, and a
    /// high-resolution wheel's fraction of a notch travels as it is — the receiver
    /// accumulates it, so normalising it here would lose the user's slow scroll.
    #[test]
    fn remote_wheel_messages_keep_their_units_sign_and_axis() {
        use super::*;
        let (shared, rx) = hook_state(CaptureKind::Remote);
        let wheel = |delta: i16| MSLLHOOKSTRUCT {
            mouseData: u32::from(delta as u16) << 16,
            ..Default::default()
        };
        for (message, delta, expected) in [
            (
                WM_MOUSEWHEEL,
                120i16,
                InputEvent::Scroll { h120: 0, v120: 120 },
            ),
            (
                WM_MOUSEWHEEL,
                -120,
                InputEvent::Scroll {
                    h120: 0,
                    v120: -120,
                },
            ),
            (WM_MOUSEWHEEL, 30, InputEvent::Scroll { h120: 0, v120: 30 }),
            (
                WM_MOUSEHWHEEL,
                120,
                InputEvent::Scroll { h120: 120, v120: 0 },
            ),
            (
                WM_MOUSEHWHEEL,
                -30,
                InputEvent::Scroll { h120: -30, v120: 0 },
            ),
        ] {
            let ms = wheel(delta);
            assert_eq!(
                handle_mouse(
                    &shared,
                    WPARAM(message as usize),
                    LPARAM(&ms as *const _ as isize)
                ),
                Some(true),
                "a wheel turn in Remote must be swallowed locally"
            );
            assert!(
                matches!(
                    rx.try_recv(),
                    Ok(CaptureEvent::Input(event)) if event == expected
                ),
                "message {message:#x} delta {delta} must queue {expected:?}"
            );
        }
        assert!(rx.try_recv().is_err());
        assert_eq!(shared.wheel_v.load(Ordering::Relaxed), 3);
        assert_eq!(shared.wheel_h.load(Ordering::Relaxed), 2);
        assert_eq!(shared.captured.load(Ordering::Relaxed), 5);
        assert_eq!(shared.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn raw_mouse_motion_preserves_relative_deltas_and_rejects_absolute_records() {
        use super::*;
        assert_eq!(
            raw_mouse_motion(0, -7, 12),
            Some(InputEvent::PointerRelative { dx: -7, dy: 12 })
        );
        assert_eq!(raw_mouse_motion(0, 0, 0), None);
        assert_eq!(raw_mouse_motion(MOUSE_MOVE_ABSOLUTE.0, 400, 600), None);
        // Other RAWMOUSE flags do not turn a relative record into absolute.
        assert_eq!(
            raw_mouse_motion(0x08, 0, -1),
            Some(InputEvent::PointerRelative { dx: 0, dy: -1 })
        );
    }

    #[test]
    #[ignore = "needs an interactive Windows desktop; sends bounded mouse motion"]
    fn restarted_remote_capture_receives_raw_motion() {
        use super::*;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
        };

        let config = CaptureConfig {
            capture_injected: true,
            ..CaptureConfig::default()
        };
        let mut first = Capture::start(config).expect("first capture");
        first.stop();
        let first_count = first.stats().captured;
        let second = Capture::start(config).expect("second capture");
        second.set_mode(CaptureKind::Remote);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 3,
                    dy: -2,
                    dwFlags: MOUSEEVENTF_MOVE,
                    ..Default::default()
                },
            },
        };
        // SAFETY: fully initialized untagged dev motion. Remote suppresses
        // local movement; dropping capture always removes the hook.
        assert_eq!(unsafe { SendInput(&[input], size_of::<INPUT>() as i32) }, 1);
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let mut received = false;
        while std::time::Instant::now() < deadline && !received {
            received = matches!(
                second.recv_timeout(Duration::from_millis(25)),
                Ok(CaptureEvent::Input(InputEvent::PointerRelative {
                    dx: 3,
                    dy: -2
                }))
            );
        }
        assert!(
            received,
            "new Remote capture lost motion: {:?}",
            second.stats()
        );
        assert_eq!(
            first.stats().captured,
            first_count,
            "old capture received new input"
        );
        assert!(second.stats().raw_input_ok);
    }

    #[test]
    fn injected_event_filter_preserves_physical_input_and_drops_own_injection() {
        assert!(should_capture_event(false, false, false));
        assert!(!should_capture_event(true, false, false));
        assert!(should_capture_event(true, false, true));
        assert!(!should_capture_event(true, true, true));
        assert!(!should_capture_event(true, true, false));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "needs an interactive Windows desktop; uses harmless Shift taps"]
    fn capture_distinguishes_vk_only_from_scancode_sendinput() {
        use std::mem::size_of;
        use std::time::{Duration, Instant};

        use input_model::{HidKey, InputEvent, KeyState};
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, VIRTUAL_KEY, VK_LSHIFT,
        };

        let capture = super::Capture::start(super::CaptureConfig {
            capture_injected: true,
            ..super::CaptureConfig::default()
        })
        .expect("capture hooks start");
        let key_input = |vk, scan, flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let scancode_flags = KEYEVENTF_SCANCODE;
        let input = [
            key_input(VK_LSHIFT, 0, KEYBD_EVENT_FLAGS(0)),
            key_input(VK_LSHIFT, 0, KEYEVENTF_KEYUP),
            key_input(VIRTUAL_KEY(0), 0x2A, scancode_flags),
            key_input(VIRTUAL_KEY(0), 0x2A, scancode_flags | KEYEVENTF_KEYUP),
        ];
        // SAFETY: all entries are fully initialised KEYBOARD INPUT records.
        let sent = unsafe { SendInput(&input, size_of::<INPUT>() as i32) };
        assert_eq!(sent, 4, "Windows must accept both Shift taps");

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut down = false;
        let mut up = false;
        while Instant::now() < deadline && !(down && up) {
            if let Ok(super::CaptureEvent::Input(InputEvent::Key { key, state })) =
                capture.recv_timeout(Duration::from_millis(25))
            {
                if key == HidKey::keyboard(0xE1) {
                    down |= state == KeyState::Down;
                    up |= state == KeyState::Up;
                }
            }
        }
        let stats = capture.stats();
        assert!(
            down && up,
            "scancode SendInput must reach the hook as HID left Shift: \
             down={down} up={up} stats={stats:?}"
        );
        assert_eq!(stats.keyboard_hook_events, 4);
        assert_eq!(stats.keyboard_injected_seen, 4);
        assert_eq!(stats.keyboard_filtered, 0);
        assert_eq!(stats.keyboard_mapped, 2);
        assert_eq!(
            stats.unmapped_keys, 2,
            "VK-only keys have no physical scancode"
        );
    }
}
