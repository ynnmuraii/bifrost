//! The edge strip window: the small ASCII particle strip along a linked edge.
//!
//! One layered, click-through (`WS_EX_LAYERED | WS_EX_TRANSPARENT`), never-activated, tool,
//! topmost window covering only the strip (`edgefx::Strip`), painted with
//! `UpdateLayeredWindow` from a reused DIB. It lives on its own thread with its own message
//! loop and receives only lightweight [`edgefx::Update`]s - it never touches the capture hook
//! thread, so rendering can never delay a hook (Windows silently drops slow low-level hooks).
//!
//! Resource rule: while the [`edgefx::Effect`] is idle there is no timer, no DIB and the window
//! is hidden; the thread sleeps in `GetMessageW`. While active a 33 ms timer drives
//! `step` + `draw` into the same buffer (allocation-free frames).

use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use edgefx::{Canvas, Effect, Strip, Update, FRAME_S};
use windows::core::w;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, KillTimer,
    PostThreadMessageW, RegisterClassW, SetTimer, SetWindowPos, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, HWND_TOPMOST, MA_NOACTIVATE, MSG, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOOWNERZORDER, SWP_NOSIZE, SW_HIDE, ULW_ALPHA, WM_APP, WM_MOUSEACTIVATE, WM_QUIT, WM_TIMER,
    WNDCLASSW, WNDCLASS_STYLES, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

const TIMER_ID: usize = 0x4C59;
const TIMER_MS: u32 = 33;
const WM_WAKE: u32 = WM_APP + 2;

/// Where the display of an update sits on the desktop, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// `[x, y, width, height]` of the display in device pixels.
    pub native: [i32; 4],
    /// Physical px per logical px of that display.
    pub scale: f32,
}

enum Command {
    Update(Update, Placement),
    Enabled(bool),
}

/// The handle the agent's Windows backend owns. Sending never blocks and never allocates
/// beyond the channel node.
pub struct EdgeStrip {
    enabled: bool,
    worker: Option<Worker>,
}

struct Worker {
    tx: Sender<Command>,
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    fn send(&self, command: Command) {
        if self.tx.send(command).is_ok() {
            // SAFETY: a plain thread message to the worker's own thread.
            let _ = unsafe { PostThreadMessageW(self.thread_id, WM_WAKE, WPARAM(0), LPARAM(0)) };
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: as above.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl EdgeStrip {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            worker: None,
        }
    }

    /// Feeds one update. The thread starts on the first one.
    pub fn send(&mut self, update: Update, placement: Placement) {
        if !self.enabled {
            return;
        }
        if self.worker.is_none() {
            self.worker = spawn();
        }
        if let Some(worker) = &self.worker {
            worker.send(Command::Update(update, placement));
        }
    }

    /// Live `[appearance].edge_effects`; off clears at once.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            if let Some(worker) = &self.worker {
                worker.send(Command::Enabled(false));
            }
        }
    }
}

fn spawn() -> Option<Worker> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let join = std::thread::Builder::new()
        .name("lws-edge-strip".to_owned())
        .spawn(move || {
            // A panic ends only this cosmetic thread.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(&rx, &ready_tx);
            }));
        })
        .ok()?;
    let thread_id = ready_rx.recv_timeout(Duration::from_secs(2)).ok()?;
    Some(Worker {
        tx,
        thread_id,
        join: Some(join),
    })
}

extern "system" fn proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    // SAFETY: default handling of our own window's messages.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The reused backing store: a top-down 32-bit DIB selected into a memory DC.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    w: usize,
    h: usize,
}

impl Surface {
    fn new(w: usize, h: usize) -> Option<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        // SAFETY: a memory DC and a DIB section with a valid header; `bits` is written by GDI.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                let _ = DeleteDC(dc);
                return None;
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                let _ = DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bitmap.into());
            Some(Self {
                dc,
                bitmap,
                old,
                bits: bits.cast(),
                w,
                h,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: GDI allocated `w * h` 32-bit pixels that live until `drop`.
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.w * self.h) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the handles were created in `new` and are released once.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

struct Overlay {
    hwnd: HWND,
    effect: Effect,
    placement: Option<Placement>,
    surface: Option<Surface>,
    strip: Option<Strip>,
    timer: bool,
    last: Instant,
    enabled: bool,
}

impl Overlay {
    fn apply(&mut self, update: Update, placement: Placement) {
        if !self.enabled {
            return;
        }
        self.placement = Some(placement);
        self.effect.update(update);
        self.refresh();
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.effect.clear();
            self.refresh();
        }
    }

    /// Brings window, buffer and timer in line with whether the effect is alive.
    fn refresh(&mut self) {
        let active = self.effect.is_active();
        if active && !self.timer {
            self.last = Instant::now();
            // SAFETY: a message timer on our window.
            let _ = unsafe { SetTimer(Some(self.hwnd), TIMER_ID, TIMER_MS, None) };
            self.timer = true;
        }
        if !active {
            if self.timer {
                // SAFETY: as above.
                let _ = unsafe { KillTimer(Some(self.hwnd), TIMER_ID) };
                self.timer = false;
            }
            if self.surface.take().is_some() {
                // SAFETY: our own window.
                let _ = unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            }
            self.strip = None;
            return;
        }
        // The strip may change between updates (another edge, display or segment).
        let strip = self.effect.strip();
        if strip != self.strip || self.surface.is_none() {
            self.strip = strip;
            self.surface = None;
            if let (Some(strip), Some(placement)) = (strip, self.placement) {
                let (w, h) = strip.size_px(placement.scale);
                self.surface = Surface::new(w as usize, h as usize);
            }
        }
        self.paint();
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last)
            .as_secs_f32()
            .max(FRAME_S * 0.25);
        self.last = now;
        self.effect.step(dt);
        self.refresh();
    }

    fn paint(&mut self) {
        let (Some(surface), Some(strip), Some(placement)) =
            (self.surface.as_mut(), self.strip, self.placement)
        else {
            return;
        };
        let (x, y, w, h) = strip.rect_in(placement.native, placement.scale);
        let mut canvas = Canvas::new(surface.pixels(), w as usize, h as usize);
        self.effect.draw(&mut canvas, placement.scale);
        let position = POINT { x, y };
        let size = SIZE {
            cx: w as i32,
            cy: h as i32,
        };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: our layered window and the memory DC that holds the premultiplied pixels.
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&position),
                Some(&size),
                Some(surface.dc),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
            );
            // `UpdateLayeredWindow` does not show a hidden window by itself.
            let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(
                self.hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
            );
        }
    }
}

struct WindowGuard(HWND);

impl Drop for WindowGuard {
    fn drop(&mut self) {
        // SAFETY: the window belongs to the dying thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

fn run(rx: &Receiver<Command>, ready: &Sender<u32>) {
    // SAFETY: the current module handle.
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return;
    };
    let instance = HINSTANCE(module.0);
    let class = w!("MultideskflowEdgeStrip");
    let window_class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(proc),
        hInstance: instance,
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: valid class description; an existing class is fine.
    if unsafe { RegisterClassW(&window_class) } == 0
        && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS
    {
        return;
    }
    // SAFETY: click-through (TRANSPARENT), never activated, out of the taskbar and Alt+Tab.
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class,
            w!("multideskflow edge strip"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )
    }) else {
        return;
    };
    let _guard = WindowGuard(hwnd);
    // SAFETY: the calling thread's id.
    let _ = ready.send(unsafe { GetCurrentThreadId() });

    let seed = 0x5EED_1234;
    let mut overlay = Overlay {
        hwnd,
        effect: Effect::new(seed),
        placement: None,
        surface: None,
        strip: None,
        timer: false,
        last: Instant::now(),
        enabled: true,
    };
    let mut message = MSG::default();
    // SAFETY: the standard loop of the thread that owns the window.
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.hwnd.0.is_null() && message.message == WM_WAKE {
            // Everything queued is applied before one repaint; a burst plays as one effect.
            while let Ok(command) = rx.try_recv() {
                match command {
                    Command::Update(update, placement) => overlay.apply(update, placement),
                    Command::Enabled(on) => overlay.set_enabled(on),
                }
            }
        } else if message.hwnd == hwnd && message.message == WM_TIMER {
            overlay.tick();
        } else {
            // SAFETY: dispatching to our own window procedure.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}
