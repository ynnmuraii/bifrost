//! The Windows clipboard: change notifications, `CF_UNICODETEXT`, and the three documented
//! secret markers.
//!
//! ADR-008 allows text only, and `docs/04-decisions.md` names the markers this adapter has
//! to honour: `ExcludeClipboardContentFromMonitorProcessing` (a password manager telling
//! every monitor to keep away), `CanIncludeInClipboardHistory = 0` (the Win10/11 clipboard
//! history opt-out) and `Clipboard Viewer Ignore` (the oldest of the three, what old
//! password managers still set). All three are *registered* clipboard formats: the format
//! exists for a while even when the current item does not set it, so presence alone is not
//! the signal — the value is. This module reads it and reports [`Marker`]s; the decision
//! belongs to `clipboard::policy`.
//!
//! The listener is the documented one (`AddClipboardFormatListener` on a message-only
//! window, `WM_CLIPBOARDUPDATE`); [`Clipboard::sequence`] is the cheap second way to notice
//! a change (`GetClipboardSequenceNumber`), which is what a caller that already runs a timer
//! can poll instead.

use clipboard::Marker;

/// `ExcludeClipboardContentFromMonitorProcessing`: presence means "do not monitor".
pub const EXCLUDE_FROM_MONITOR: &str = "ExcludeClipboardContentFromMonitorProcessing";
/// `CanIncludeInClipboardHistory`: the value `0` means "keep it out of history".
pub const CAN_INCLUDE_IN_HISTORY: &str = "CanIncludeInClipboardHistory";
/// `Clipboard Viewer Ignore`: presence means "viewers should ignore this".
pub const VIEWER_IGNORE: &str = "Clipboard Viewer Ignore";

/// What one clipboard read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `CF_UNICODETEXT`, decoded from UTF-16. Empty when the clipboard carries no text.
    pub text: String,
    /// Every secret marker the item carried.
    pub markers: Vec<Marker>,
    /// Whether `CF_UNICODETEXT` was there at all: an image-only clipboard is not an error,
    /// it is simply nothing this adapter can offer.
    pub has_text: bool,
    /// `GetClipboardSequenceNumber` at the moment of the read.
    pub sequence: u32,
}

impl Snapshot {
    /// The content description `clipboard::policy` decides on.
    pub fn content(&self) -> clipboard::Content {
        clipboard::Content {
            bytes: self.text.len() as u64,
            markers: self.markers.clone(),
        }
    }
}

/// `CF_UNICODETEXT` text as the clipboard block wants it: UTF-16 code units, NUL-terminated.
///
/// Pure and host-independent, so the encoding is unit-tested on any host: the only FFI part
/// of [`Clipboard::set`] is the allocation the units are copied into.
pub fn to_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Classifies the *names* of the registered formats an item carries.
///
/// Pure, so the mapping is unit-testable without a clipboard: the adapter passes the
/// registered names (and, for `CanIncludeInClipboardHistory`, whether its value was zero).
pub fn classify_names(names: &[&str], history_allowed_zero: bool) -> Vec<Marker> {
    let mut markers = Vec::new();
    for name in names {
        // Case-insensitive: the API's names are documented, but a producer may spell the
        // registered format however `RegisterClipboardFormat` was called.
        if name.eq_ignore_ascii_case(EXCLUDE_FROM_MONITOR) {
            markers.push(Marker::ExcludeFromMonitor);
        } else if name.eq_ignore_ascii_case(CAN_INCLUDE_IN_HISTORY) && history_allowed_zero {
            markers.push(Marker::NoHistory);
        } else if name.eq_ignore_ascii_case(VIEWER_IGNORE) {
            markers.push(Marker::ViewerIgnore);
        }
    }
    markers
}

#[cfg(windows)]
pub use windows_impl::{Change, Clipboard, ClipboardError};

#[cfg(windows)]
mod windows_impl {
    use super::{
        classify_names, Snapshot, CAN_INCLUDE_IN_HISTORY, EXCLUDE_FROM_MONITOR, VIEWER_IGNORE,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use windows::core::w;
    use windows::Win32::Foundation::GlobalFree;
    use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, WPARAM};
    use windows::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
        GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        RegisterClassW, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WM_CLIPBOARDUPDATE, WM_DESTROY,
        WNDCLASSW, WS_OVERLAPPED,
    };

    /// Why a clipboard read failed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ClipboardError {
        /// `OpenClipboard` was refused: another process holds it (with retries this is
        /// transient; without them it is what a busy clipboard looks like).
        Busy,
        /// The advertised format was not there when the data was read (a race with the
        /// owner).
        Gone,
        /// The registry of formats could not be queried.
        Formats,
        /// The listener's window class or window could not be created.
        Window,
    }

    impl std::fmt::Display for ClipboardError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let text = match self {
                ClipboardError::Busy => "the clipboard is held by another process",
                ClipboardError::Gone => "the clipboard changed while it was being read",
                ClipboardError::Formats => "the clipboard format list could not be read",
                ClipboardError::Window => "the clipboard listener window could not be created",
            };
            f.write_str(text)
        }
    }

    impl std::error::Error for ClipboardError {}

    /// A `WM_CLIPBOARDUPDATE` arrived.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Change;

    /// A message-only window that subscribes to clipboard updates.
    ///
    /// The window lives on its own thread with a message loop, exactly like the capture
    /// hook: the caller gets [`Change`] values over a channel and never blocks on the
    /// clipboard itself.
    pub struct Clipboard {
        changes: Receiver<Change>,
        /// The listener's thread, joined on drop.
        thread: Option<JoinHandle<()>>,
        window: Option<isize>,
        /// The listener thread's own id: `WM_QUIT` goes to the *thread*, not to the window,
        /// or the message pump waits forever (the capture thread does the same).
        thread_id: Arc<AtomicU32>,
    }

    // The window handle travels between threads as an address, which is what the API
    // requires; the message loop itself never leaves its thread.
    unsafe impl Send for Clipboard {}

    impl Clipboard {
        /// Creates the listener window and starts its message loop.
        pub fn listen() -> Result<Self, ClipboardError> {
            let (sender, changes) = mpsc::channel();
            let (ready_sender, ready) = mpsc::channel();
            let thread_id = Arc::new(AtomicU32::new(0));
            let thread_id_out = Arc::clone(&thread_id);
            let thread = std::thread::Builder::new()
                .name("lws-clipboard".to_owned())
                .spawn(move || {
                    run_listener(sender, ready_sender, thread_id_out);
                })
                .map_err(|_| ClipboardError::Window)?;
            match ready.recv().map_err(|_| ClipboardError::Window)? {
                Ok(window) => Ok(Clipboard {
                    changes,
                    thread: Some(thread),
                    window: Some(window),
                    thread_id,
                }),
                Err(error) => {
                    let _ = thread.join();
                    Err(error)
                }
            }
        }

        /// Waits for the next clipboard change.
        pub fn wait(&self, timeout: std::time::Duration) -> Option<Change> {
            self.changes.recv_timeout(timeout).ok()
        }

        /// The clipboard's change counter: two reads with the same number saw the same item.
        pub fn sequence(&self) -> u32 {
            // SAFETY: the call takes no arguments and returns a counter.
            unsafe { GetClipboardSequenceNumber() }
        }

        /// Reads the current text and its secret markers.
        pub fn read(&self) -> Result<Snapshot, ClipboardError> {
            read_clipboard()
        }

        /// Takes ownership of the clipboard and puts `text` on it as `CF_UNICODETEXT`.
        ///
        /// `&self` is enough: the write is a sequence of Win32 calls a thread makes on its
        /// own, and this struct owns no per-write state — the listener's message loop is
        /// unaffected (`WM_CLIPBOARDUPDATE` is what announces this write to everyone else,
        /// including this process; recognising it as our own is the caller's job, see
        /// `Clipboard::sequence`).
        pub fn set(&self, text: &str) -> Result<(), ClipboardError> {
            write_clipboard(Some(text))
        }

        /// Empties the clipboard, giving up the selection.
        ///
        /// The result is an open, empty clipboard: `CF_UNICODETEXT` is gone, so a paste
        /// finds nothing, and the update counter still moves.
        pub fn clear(&self) -> Result<(), ClipboardError> {
            write_clipboard(None)
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            if let Some(window) = self.window.take() {
                // Tell the loop to end; the window is destroyed on its own thread.
                let hwnd = HWND(window as *mut std::ffi::c_void);
                // SAFETY: `hwnd` came from this module's `CreateWindowExW`.
                unsafe {
                    let _ = RemoveClipboardFormatListener(hwnd);
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        Some(hwnd),
                        WM_DESTROY,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            // `WM_QUIT` ends the pump: without it the thread waits in `GetMessageW`
            // forever, which is a hang in production and not a test detail.
            let id = self.thread_id.load(Ordering::SeqCst);
            if id != 0 {
                // SAFETY: posting a thread message to a thread this struct owns.
                let _ = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        id,
                        windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                        WPARAM(0),
                        LPARAM(0),
                    )
                };
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// The listener thread: register, subscribe, pump messages.
    fn run_listener(
        changes: Sender<Change>,
        ready: Sender<Result<isize, ClipboardError>>,
        thread_id_out: Arc<AtomicU32>,
    ) {
        const CLASS: windows::core::PCWSTR = w!("lws-clipboard-listener");
        let class = WNDCLASSW {
            style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
            lpfnWndProc: Some(window_proc),
            lpszClassName: CLASS,
            ..Default::default()
        };
        // The first listener registers the class; a second one gets
        // `ERROR_CLASS_ALREADY_EXISTS`, which is success for our purposes.
        // SAFETY: the class points at a static name and a `'static` window procedure.
        if unsafe { RegisterClassW(&class) } == 0 {
            use windows::Win32::Foundation::{GetLastError, ERROR_CLASS_ALREADY_EXISTS};
            if unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
                let _ = ready.send(Err(ClipboardError::Window));
                return;
            }
        }
        // SAFETY: a documented call with no preconditions.
        thread_id_out.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        // A message-only window: it is never shown and exists only for the notification.
        // SAFETY: the class is registered and the name is static.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                CLASS,
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        };
        let Ok(window) = window else {
            let _ = ready.send(Err(ClipboardError::Window));
            return;
        };
        // SAFETY: `window` is a live window handle owned by this thread.
        if unsafe { AddClipboardFormatListener(window) }.is_err() {
            let _ = ready.send(Err(ClipboardError::Window));
            return;
        }
        let _ = ready.send(Ok(window.0 as isize));

        let mut message = MSG::default();
        // SAFETY: a standard message loop for this thread's window.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            if message.message == WM_CLIPBOARDUPDATE {
                // A receiver that went away means the listener is being dropped.
                if changes.send(Change).is_err() {
                    break;
                }
                continue;
            }
            unsafe {
                let _ = DispatchMessageW(&message);
            }
        }
        // SAFETY: this thread owns `window`.
        unsafe {
            let _ = DestroyWindow(window);
        }
    }

    /// The window procedure: only the clipboard notification matters here.
    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        // SAFETY: the default handler is what every unhandled message needs.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Reads the clipboard: text, the registered marker formats, and the sequence number.
    pub fn read_clipboard() -> Result<Snapshot, ClipboardError> {
        let sequence = unsafe { GetClipboardSequenceNumber() };
        open_clipboard()?;
        let result = read_open_clipboard(sequence);
        // SAFETY: the clipboard was opened above, on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
        result
    }

    /// Opens the clipboard on this thread, retrying briefly while another process holds it.
    ///
    /// The caller closes it: an open clipboard blocks every other process, so it is never
    /// left open across an operation that can fail.
    fn open_clipboard() -> Result<(), ClipboardError> {
        for _ in 0..10 {
            // SAFETY: a null owner means "this thread"; the handle is closed by the caller.
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(ClipboardError::Busy)
    }

    /// Replaces the clipboard contents with `text`, or empties it for `None`.
    fn write_clipboard(text: Option<&str>) -> Result<(), ClipboardError> {
        open_clipboard()?;
        let result = write_open_clipboard(text);
        // SAFETY: the clipboard was opened above, on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
        result
    }

    /// The body of [`write_clipboard`], with the clipboard already open.
    fn write_open_clipboard(text: Option<&str>) -> Result<(), ClipboardError> {
        // SAFETY: the clipboard is open on this thread.
        unsafe { EmptyClipboard() }.map_err(|_| ClipboardError::Gone)?;
        let Some(text) = text else {
            return Ok(());
        };

        let units = super::to_utf16(text);
        let bytes = std::mem::size_of_val(units.as_slice());
        // SAFETY: a movable block of `bytes`, uninitialised by contract; it is freed on
        // every path below unless the clipboard takes ownership of it.
        let block =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|_| ClipboardError::Gone)?;
        // SAFETY: the block was just allocated and is not owned by anyone else.
        let pointer = unsafe { GlobalLock(block) };
        if pointer.is_null() {
            // SAFETY: this thread still owns the block.
            unsafe {
                let _ = GlobalFree(Some(block));
            }
            return Err(ClipboardError::Gone);
        }
        // SAFETY: the block is `bytes` long, which is exactly the encoded text, and a
        // locked block may be written.
        unsafe {
            std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, pointer as *mut u8, bytes);
            let _ = GlobalUnlock(block);
        }
        // SAFETY: the clipboard is open and the block is unlocked; on success the clipboard
        // owns it and must not be freed here.
        let taken = unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(block.0))) };
        if taken.is_err() {
            // SAFETY: the clipboard refused the block, so this thread still owns it.
            unsafe {
                let _ = GlobalFree(Some(block));
            }
            return Err(ClipboardError::Gone);
        }
        Ok(())
    }

    /// `CF_UNICODETEXT` is the documented predefined format number 13; the bindings'
    /// constant lives behind a feature this crate does not need for anything else.
    const CF_UNICODETEXT: u32 = 13;

    /// The body of [`read_clipboard`], with the clipboard already open.
    fn read_open_clipboard(sequence: u32) -> Result<Snapshot, ClipboardError> {
        let text_format = CF_UNICODETEXT;
        // SAFETY: the clipboard is open on this thread.
        let has_text = unsafe { IsClipboardFormatAvailable(text_format) }.is_ok();

        let mut names: Vec<String> = Vec::new();
        let mut history_allowed_zero = false;
        let exclude = register(EXCLUDE_FROM_MONITOR)?;
        let history = register(CAN_INCLUDE_IN_HISTORY)?;
        let viewer = register(VIEWER_IGNORE)?;

        // Walk the format list: `EnumClipboardFormats` returns 0 at the end.
        let mut format = 0u32;
        loop {
            // SAFETY: the clipboard is open; 0 asks for the first format.
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                break;
            }
            if format == exclude {
                names.push(EXCLUDE_FROM_MONITOR.to_owned());
            } else if format == viewer {
                names.push(VIEWER_IGNORE.to_owned());
            } else if format == history {
                names.push(CAN_INCLUDE_IN_HISTORY.to_owned());
                // The flag is a DWORD: `0` means "not allowed in history", which is the
                // password-manager case this marker exists for.
                history_allowed_zero = read_dword(format)? == Some(0);
            }
        }

        let text = if has_text {
            read_text(text_format)?
        } else {
            String::new()
        };
        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        Ok(Snapshot {
            text,
            markers: classify_names(&borrowed, history_allowed_zero),
            has_text,
            sequence,
        })
    }

    /// `RegisterClipboardFormatW` for one of the documented names.
    fn register(name: &str) -> Result<u32, ClipboardError> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the call.
        let format = unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(wide.as_ptr())) };
        if format == 0 {
            return Err(ClipboardError::Formats);
        }
        Ok(format)
    }

    /// The clipboard text as UTF-16, decoded.
    fn read_text(format: u32) -> Result<String, ClipboardError> {
        // SAFETY: the clipboard is open and the format was reported as available.
        let handle = unsafe { GetClipboardData(format) }.map_err(|_| ClipboardError::Gone)?;
        if handle.is_invalid() {
            return Err(ClipboardError::Gone);
        }
        // SAFETY: the handle is a `HGLOBAL` for a text format, as the API documents.
        let pointer = unsafe { GlobalLock(HGLOBAL(handle.0)) };
        if pointer.is_null() {
            return Err(ClipboardError::Gone);
        }
        let mut units: Vec<u16> = Vec::new();
        // SAFETY: a `CF_UNICODETEXT` block is NUL-terminated UTF-16.
        unsafe {
            let mut cursor = pointer as *const u16;
            while *cursor != 0 {
                units.push(*cursor);
                cursor = cursor.add(1);
            }
            let _ = GlobalUnlock(HGLOBAL(handle.0));
        }
        Ok(String::from_utf16_lossy(&units))
    }

    /// Reads a `DWORD` payload, as the history flag format carries.
    fn read_dword(format: u32) -> Result<Option<u32>, ClipboardError> {
        // SAFETY: the clipboard is open and the format is registered.
        let handle = unsafe { GetClipboardData(format) }.map_err(|_| ClipboardError::Gone)?;
        if handle.is_invalid() {
            return Ok(None);
        }
        // SAFETY: the flag format is a `DWORD` block, as the API documents.
        let pointer = unsafe { GlobalLock(HGLOBAL(handle.0)) };
        if pointer.is_null() {
            return Ok(None);
        }
        // SAFETY: a `DWORD` is four bytes and the block is at least that large.
        let value = unsafe {
            let value = *(pointer as *const u32);
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            value
        };
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_names_map_to_their_markers() {
        assert_eq!(
            classify_names(&[EXCLUDE_FROM_MONITOR], false),
            vec![Marker::ExcludeFromMonitor]
        );
        assert_eq!(
            classify_names(&[VIEWER_IGNORE], false),
            vec![Marker::ViewerIgnore]
        );
        // The history format only means "secret" when its value is zero.
        assert_eq!(
            classify_names(&[CAN_INCLUDE_IN_HISTORY], true),
            vec![Marker::NoHistory]
        );
        assert!(classify_names(&[CAN_INCLUDE_IN_HISTORY], false).is_empty());
    }

    #[test]
    fn the_names_are_matched_case_insensitively_and_all_at_once() {
        let names = [
            "excludeclipboardcontentfrommonitorprocessing",
            "CLIPBOARD VIEWER IGNORE",
            "SomeOtherFormat",
        ];
        assert_eq!(
            classify_names(&names, false),
            vec![Marker::ExcludeFromMonitor, Marker::ViewerIgnore]
        );
        // An unrelated format never becomes a marker.
        assert!(classify_names(&["PNG", "HTML Format"], false).is_empty());
    }

    #[test]
    fn the_encoded_text_is_nul_terminated_utf16() {
        assert_eq!(to_utf16(""), vec![0]);
        assert_eq!(to_utf16("A"), vec![0x0041, 0]);
        // Six Cyrillic letters are six code units plus the terminator.
        assert_eq!(
            to_utf16("привет"),
            vec![0x043F, 0x0440, 0x0438, 0x0432, 0x0435, 0x0442, 0]
        );
        // Outside the BMP the encoding is a surrogate pair, and there is still one NUL.
        let crab = to_utf16("🦀");
        assert_eq!(crab.len(), 3);
        assert_eq!(crab.last(), Some(&0));
        assert!(
            crab[0] != 0 && crab[1] != 0,
            "a surrogate pair, not the terminator"
        );
        // A text that already contains a NUL keeps it and gains no other terminator.
        assert_eq!(to_utf16("a\u{0}"), vec![0x0061, 0, 0]);
    }

    #[test]
    fn the_snapshot_describes_itself_as_content() {
        let snapshot = Snapshot {
            text: "привет".to_owned(),
            markers: vec![Marker::ViewerIgnore],
            has_text: true,
            sequence: 7,
        };
        let content = snapshot.content();
        assert_eq!(content.bytes, "привет".len() as u64);
        assert_eq!(content.markers, vec![Marker::ViewerIgnore]);
    }
}
