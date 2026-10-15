//! The cursor veil: hides the local cursor while input is on another machine.
//!
//! # Safety rules (why it looks like this)
//!
//! * **Nothing global, nothing persistent.** No `SetSystemCursor`, no registry, no
//!   `ShowCursor` (that counter is per thread and would not hide anything anyway). The veil is
//!   one small window, so its whole lifetime is the process's: if the agent dies the window
//!   goes with it and the cursor is back. This is the prior art of Synergy / Deskflow / Input
//!   Leap (`MSWindowsDesks::deskLeave`: a window under the held cursor whose class cursor is
//!   blank).
//! * **Not `WS_EX_TRANSPARENT`.** The cursor is drawn by whichever window is under the hotspot,
//!   and a hit-test-transparent window hands `WM_SETCURSOR` to the window below it, so the
//!   cursor would stay visible. The veil is `WS_EX_LAYERED` with alpha 1 (invisible, still
//!   hit-testable) and answers `WM_SETCURSOR` with `SetCursor(NULL)`. It is only
//!   [`SIZE`] px square around the held point, never the desktop, so a stuck veil could not
//!   block anything: input is captured by the hooks while it exists anyway.
//! * **Topmost is re-asserted** on show and on every tick: a monitor edge, the taskbar or
//!   another topmost window may be raised over the veil, and then the cursor would show.
//! * **Self-releasing.** Its own thread polls a [`CursorProbe`] (atomics of the capture) every
//!   [`TICK_MS`] *only while shown*; the emergency combo, an expired lease, a stopped or
//!   dropped capture all make the probe `None`, so the veil hides within one tick even if the
//!   agent's async task is stuck. Idle (hidden) it has no timer and sleeps in `GetMessageW`.
//! * It never runs in the capture hook thread, so it cannot delay a hook.
//!
//! The decisions live in [`edgefx::veil::Veil`] (tested against a fake host); this file is
//! the thin Win32 host around it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use edgefx::veil::{Point, Veil, VeilHost};
use windows::core::w;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    PostThreadMessageW, RegisterClassW, SetCursor, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, ShowWindow, TranslateMessage, HWND_NOTOPMOST, HWND_TOPMOST, LWA_ALPHA,
    MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW,
    SW_HIDE, WM_APP, WM_MOUSEACTIVATE, WM_QUIT, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WNDCLASS_STYLES,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::CursorProbe;

/// Side of the veil window around the held point, physical px. Big enough that the cursor
/// drifting a few px before the hold re-warps it stays under the veil.
pub const SIZE: i32 = 48;
/// The liveness poll while shown.
pub const TICK_MS: u32 = 100;

const TIMER_ID: usize = 0x4C58;
const WM_WAKE: u32 = WM_APP + 1;

enum Command {
    Engage(CursorProbe),
    SetEnabled(bool),
    Release,
}

struct Worker {
    tx: Sender<Command>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    fn send(&self, command: Command) {
        if self.tx.send(command).is_ok() {
            // SAFETY: posts a plain thread message to a thread this worker owns.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_WAKE, WPARAM(0), LPARAM(0)) };
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: as above; WM_QUIT ends the loop, which hides and destroys the window.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Owner side: created with the agent's backend, the thread starts on the first `engage`.
pub struct CursorVeil {
    enabled: bool,
    worker: Option<Worker>,
}

impl CursorVeil {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            worker: None,
        }
    }

    /// The backend entered Remote with this capture. Idempotent; cheap.
    pub fn engage(&mut self, probe: CursorProbe) {
        if !self.enabled {
            return;
        }
        if self.worker.is_none() {
            self.worker = spawn(self.enabled);
        }
        if let Some(worker) = &self.worker {
            worker.send(Command::Engage(probe));
        }
    }

    /// The backend left Remote (explicit end). The probe makes this redundant, which is the
    /// point: it is a promptness hint, not the safety mechanism.
    pub fn release(&self) {
        if let Some(worker) = &self.worker {
            worker.send(Command::Release);
        }
    }

    /// Live `[appearance].hide_remote_cursor`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if let Some(worker) = &self.worker {
            worker.send(Command::SetEnabled(enabled));
        }
    }
}

fn spawn(enabled: bool) -> Option<Worker> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("lws-cursor-veil".to_owned())
        .spawn(move || {
            // A panic here must end only the veil (its window dies with the thread).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&rx, enabled, &ready_tx);
            }));
        })
        .ok()?;
    let id = ready_rx.recv_timeout(Duration::from_secs(2)).ok()?;
    Some(Worker {
        tx,
        thread_id: id,
        join: Some(join),
    })
}

/// The Win32 host: one hidden-until-needed window.
struct Host {
    hwnd: HWND,
}

impl Host {
    fn place(&self, at: Point) {
        // SAFETY: our own window; topmost, never activated.
        let _ = unsafe {
            SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                at.0 - SIZE / 2,
                at.1 - SIZE / 2,
                SIZE,
                SIZE,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        };
    }
}

impl VeilHost for Host {
    fn show(&mut self, at: Point) {
        self.place(at);
        self.raise();
        // SAFETY: a null cursor hides it for this thread's window under the hotspot.
        let _ = unsafe { SetCursor(None) };
    }

    fn raise(&mut self) {
        // Reassert even if another topmost window rose above us.
        for band in [HWND_NOTOPMOST, HWND_TOPMOST] {
            // SAFETY: our window, without moving, sizing or activating.
            let _ = unsafe {
                SetWindowPos(
                    self.hwnd,
                    Some(band),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )
            };
        }
    }

    fn hide(&mut self) {
        // SAFETY: our own window.
        let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
    }

    fn timer(&mut self, on: bool) {
        if on {
            // SAFETY: a plain message timer on our window.
            let _ = unsafe { SetTimer(Some(self.hwnd), TIMER_ID, TICK_MS, None) };
        } else {
            // SAFETY: as above.
            let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ID) };
        }
    }
}

/// Destroys the window however the thread ends (also on unwind).
struct WindowGuard(HWND);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        // SAFETY: the window belongs to the dying thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

extern "system" fn proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_SETCURSOR => {
            // SAFETY: the veil's whole purpose: no cursor while the pointer is over it.
            let _ = unsafe { SetCursor(None) };
            LRESULT(1)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // SAFETY: default handling of every other message of our own window.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn run(rx: &Receiver<Command>, enabled: bool, ready: &Sender<u32>) {
    // SAFETY: the current module handle.
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    let class = w!("MultideskflowCursorVeil");
    let window_class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(proc),
        hInstance: instance,
        lpszClassName: class,
        // `hCursor` stays NULL: the window decides in WM_SETCURSOR.
        ..Default::default()
    };
    // SAFETY: valid class description; an existing class is fine.
    if unsafe { RegisterClassW(&window_class) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return;
    }
    // SAFETY: valid class and instance; a top-level popup that is never activated and
    // stays out of the taskbar and Alt+Tab. NOT transparent (see the module docs).
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class,
            w!("multideskflow cursor veil"),
            WS_POPUP,
            0,
            0,
            SIZE,
            SIZE,
            None,
            None,
            Some(instance),
            None,
        )
    }) else {
        return;
    };
    let _guard = WindowGuard(hwnd);
    // Alpha 1/255: invisible, but a layered window with LWA_ALPHA is still hit-testable.
    // SAFETY: our own layered window.
    let _ = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA) };

    // SAFETY: the calling thread's id.
    let _ = ready.send(unsafe { GetCurrentThreadId() });

    let mut host = Host { hwnd };
    let mut veil = Veil::new(enabled);
    let mut probe: Option<CursorProbe> = None;
    let hold = |probe: &Option<CursorProbe>| probe.as_ref().and_then(CursorProbe::hold_point);

    let mut message = MSG::default();
    // SAFETY: standard message loop on the thread that owns the window.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.hwnd.0.is_null() && message.message == WM_WAKE {
            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::Engage(new) => {
                        probe = Some(new);
                        veil.engage(hold(&probe), &mut host);
                    }
                    Command::SetEnabled(on) => veil.set_enabled(on, hold(&probe), &mut host),
                    Command::Release => {
                        probe = None;
                        veil.release(&mut host);
                    }
                }
            }
        } else if message.hwnd == hwnd && message.message == WM_TIMER {
            veil.tick(hold(&probe), &mut host);
        } else {
            // SAFETY: dispatching to our own window procedure.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    veil.release(&mut host);
}
