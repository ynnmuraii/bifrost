//! Windows clipboard notifications, Unicode text, PNG/DIB images and secret markers.
//!
//! Text wins over image representations. Images are read only after secret-marker checks;
//! native conversion belongs on a blocking worker, never on the hook or session thread.
//! The listener only queues `WM_CLIPBOARDUPDATE`; sequence numbers suppress applied echoes.
//!
//! Reads are bounded by `GlobalSize`. Image byte limits apply to PNG, not the native DIB;
//! decoded pixel limits live in [`image`].

use clipboard::Marker;

#[cfg(windows)]
pub mod image;

/// Native representations, in priority order. Text selection is independent of image opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFormat {
    Text,
    Png,
    DibV5,
    Dib,
}

/// Selects metadata only; a secret image must not be read or encoded.
pub fn choose_format(
    text: bool,
    png: bool,
    dib_v5: bool,
    dib: bool,
    secret: bool,
) -> Option<ClipboardFormat> {
    if text {
        Some(ClipboardFormat::Text)
    } else if secret {
        None
    } else if png {
        Some(ClipboardFormat::Png)
    } else if dib_v5 {
        Some(ClipboardFormat::DibV5)
    } else if dib {
        Some(ClipboardFormat::Dib)
    } else {
        None
    }
}

/// `ExcludeClipboardContentFromMonitorProcessing`: presence means "do not monitor".
pub const EXCLUDE_FROM_MONITOR: &str = "ExcludeClipboardContentFromMonitorProcessing";
/// `CanIncludeInClipboardHistory`: the value `0` means "keep it out of history".
pub const CAN_INCLUDE_IN_HISTORY: &str = "CanIncludeInClipboardHistory";
/// `Clipboard Viewer Ignore`: presence means "viewers should ignore this".
pub const VIEWER_IGNORE: &str = "Clipboard Viewer Ignore";

/// What one clipboard read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The selected representation; absent for unsupported or deliberately unread secret images.
    pub content: Option<clipboard::ClipboardContent>,
    /// Every secret marker the item carried, including deliberately unread images.
    pub markers: Vec<Marker>,
    /// `GetClipboardSequenceNumber` at the moment of the read.
    pub sequence: u32,
}

/// `CF_UNICODETEXT` text as the clipboard block wants it: UTF-16 code units, NUL-terminated.
///
/// Pure and host-independent, so the encoding is unit-tested on any host: the only FFI part
/// of [`Clipboard::set`] is the allocation the units are copied into.
pub fn to_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes a bounded clipboard block, checking UTF-8 size before allocating text.
pub fn decode_text(units: &[u16]) -> Result<String, clipboard::ClipboardError> {
    let mut end = 0;
    while end < units.len() && units[end] != 0 {
        end += 1;
        if end as u64 > clipboard::MAX_BYTES {
            return Err(clipboard::ClipboardError::TooLarge);
        }
    }
    if end == units.len() {
        return Err(clipboard::ClipboardError::Invalid);
    }
    let mut bytes = 0;
    for decoded in char::decode_utf16(units[..end].iter().copied()) {
        bytes += decoded.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8();
        if bytes as u64 > clipboard::MAX_BYTES {
            return Err(clipboard::ClipboardError::TooLarge);
        }
    }
    Ok(String::from_utf16_lossy(&units[..end]))
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
pub use windows_impl::{
    read_clipboard, write_clipboard, write_clipboard_sequence, write_content_sequence, Change,
    Clipboard, ClipboardError,
};

#[cfg(windows)]
mod windows_impl {
    use super::{
        choose_format, classify_names, decode_text, image, ClipboardFormat, Snapshot,
        CAN_INCLUDE_IN_HISTORY, EXCLUDE_FROM_MONITOR, VIEWER_IGNORE,
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
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
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
        /// Malformed content, an image decode limit, or the protocol payload limit.
        Content(clipboard::ClipboardError),
    }

    impl std::fmt::Display for ClipboardError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let text = match self {
                ClipboardError::Busy => "the clipboard is held by another process",
                ClipboardError::Gone => "the clipboard changed while it was being read",
                ClipboardError::Formats => "the clipboard format list could not be read",
                ClipboardError::Window => "the clipboard listener window could not be created",
                ClipboardError::Content(error) => error.code(),
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

        // The read and the write are the module-level [`read_clipboard`] and
        // [`write_clipboard`]: they are Win32 work with a retry loop, so an async caller
        // runs them on a blocking thread, and there is no reason to reach them through a
        // method on the notification listener.
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

    /// Takes a bounded native snapshot, then converts DIB after releasing the OS clipboard.
    ///
    /// The caller runs this on a blocking worker; neither the notification window nor the
    /// session loop performs image encoding.
    pub fn read_clipboard() -> Result<Snapshot, ClipboardError> {
        let raw = {
            let _open = open_clipboard()?;
            // SAFETY: the clipboard is exclusively open on this thread.
            let sequence = unsafe { GetClipboardSequenceNumber() };
            read_open_clipboard(sequence)?
        };
        match raw {
            ReadSnapshot::Ready(snapshot) => Ok(snapshot),
            ReadSnapshot::Dib {
                bytes,
                markers,
                sequence,
            } => {
                let content = image::dib_to_png(&bytes)
                    .map_err(ClipboardError::Content)?
                    .map(|png| clipboard::ClipboardContent::Image { png });
                Ok(Snapshot {
                    content,
                    markers,
                    sequence,
                })
            }
        }
    }

    /// Opens the clipboard on this thread, retrying briefly while another process holds it.
    ///
    /// The caller closes it: an open clipboard blocks every other process, so it is never
    /// left open across an operation that can fail.
    fn open_clipboard() -> Result<OpenClipboardGuard, ClipboardError> {
        open_clipboard_for(None)
    }

    fn open_clipboard_for(owner: Option<HWND>) -> Result<OpenClipboardGuard, ClipboardError> {
        for _ in 0..10 {
            // SAFETY: a supplied owner is live on this thread; the caller closes the handle.
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Ok(OpenClipboardGuard);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Err(ClipboardError::Busy)
    }

    struct OpenClipboardGuard;

    impl Drop for OpenClipboardGuard {
        fn drop(&mut self) {
            // SAFETY: this guard exists only after a successful open on the same thread.
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    /// EmptyClipboard must have a non-null owner or SetClipboardData fails. This
    /// message-only window lives on the blocking writer thread until the clipboard
    /// closes. Eager HGLOBAL data remains owned by the system after its destruction.
    struct WriteOwner(HWND);

    impl WriteOwner {
        fn new() -> Result<Self, ClipboardError> {
            // SAFETY: STATIC is a system window class; this hidden window needs no
            // custom procedure or message pump because no delayed formats are offered.
            let window = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("Bifrost clipboard writer"),
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
            }
            .map_err(|_| ClipboardError::Window)?;
            Ok(Self(window))
        }
    }

    impl Drop for WriteOwner {
        fn drop(&mut self) {
            // SAFETY: the window was created and is destroyed on this blocking thread,
            // after the exclusively open clipboard guard has been dropped.
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    /// Replaces the clipboard contents with `text`, or empties it for `None`.
    ///
    /// Public so an async caller can run it on a blocking thread: `open_clipboard`
    /// retries for up to 100 ms while another process holds the clipboard, and that
    /// sleep must never sit on a runtime thread (the agent's poll tick, its IPC frames
    /// and its watchdog live there).
    pub fn write_clipboard(text: Option<&str>) -> Result<(), ClipboardError> {
        write_clipboard_sequence(text).map(|_| ())
    }

    /// Captures the echo sequence while the clipboard is still exclusively open.
    pub fn write_clipboard_sequence(text: Option<&str>) -> Result<u32, ClipboardError> {
        let owner = WriteOwner::new()?;
        let _open = open_clipboard_for(Some(owner.0))?;
        write_open_clipboard(text)?;
        // SAFETY: capture the counter before another owner can replace this selection.
        Ok(unsafe { GetClipboardSequenceNumber() })
    }

    /// The body of [`write_clipboard`], with the clipboard already open.
    fn write_open_clipboard(text: Option<&str>) -> Result<(), ClipboardError> {
        // SAFETY: the clipboard is open on this thread.
        unsafe { EmptyClipboard() }.map_err(|_| ClipboardError::Gone)?;
        let Some(text) = text else {
            return Ok(());
        };

        let units = super::to_utf16(text);
        // SAFETY: UTF-16 units are initialized; their byte view lasts for this call.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                units.as_ptr().cast::<u8>(),
                std::mem::size_of_val(units.as_slice()),
            )
        };
        GlobalBlock::new(bytes)?.publish(CF_UNICODETEXT)
    }

    /// Eagerly publishes both PNG and alpha-preserving DIBV5, in one open session.
    /// Conversion and allocation finish before EmptyClipboard can discard the previous item.
    pub fn write_content_sequence(
        content: Option<&clipboard::ClipboardContent>,
    ) -> Result<u32, ClipboardError> {
        let png = match content {
            None => return write_clipboard_sequence(None),
            Some(clipboard::ClipboardContent::Text(text)) => {
                return write_clipboard_sequence(Some(text))
            }
            Some(clipboard::ClipboardContent::Image { png }) => png,
        };
        if png.len() as u64 > clipboard::MAX_IMAGE_BYTES {
            return Err(ClipboardError::Content(clipboard::ClipboardError::TooLarge));
        }
        let dib = image::png_to_dibv5(png).map_err(ClipboardError::Content)?;
        let png_format = register("PNG")?;
        let png_block = GlobalBlock::new(png)?;
        let dib_block = GlobalBlock::new(&dib)?;
        let owner = WriteOwner::new()?;
        let _open = open_clipboard_for(Some(owner.0))?;
        // SAFETY: the selection is exclusively open and both eager representations exist.
        unsafe { EmptyClipboard() }.map_err(|_| ClipboardError::Gone)?;
        if let Err(error) = png_block
            .publish(png_format)
            .and_then(|()| dib_block.publish(CF_DIBV5))
        {
            // Do not leave a partially published item after reporting a failed apply.
            unsafe {
                let _ = EmptyClipboard();
            }
            return Err(error);
        }
        Ok(unsafe { GetClipboardSequenceNumber() })
    }

    /// A movable global block is ours until SetClipboardData successfully takes ownership.
    struct GlobalBlock(Option<HGLOBAL>);

    impl GlobalBlock {
        fn new(bytes: &[u8]) -> Result<Self, ClipboardError> {
            // SAFETY: the allocation is initialized below before it is published.
            let block = Self(Some(
                unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
                    .map_err(|_| ClipboardError::Gone)?,
            ));
            let handle = block.0.expect("owned global block");
            let pointer = unsafe { GlobalLock(handle) };
            if pointer.is_null() {
                return Err(ClipboardError::Gone);
            }
            // SAFETY: the block has exactly this many writable bytes and does not overlap input.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
                let _ = GlobalUnlock(handle);
            }
            Ok(block)
        }

        fn publish(mut self, format: u32) -> Result<(), ClipboardError> {
            let handle = self.0.expect("owned global block");
            // SAFETY: caller opened the clipboard; the initialized block is movable and unlocked.
            unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) }
                .map_err(|_| ClipboardError::Gone)?;
            self.0 = None;
            Ok(())
        }
    }

    impl Drop for GlobalBlock {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                // SAFETY: an unpublished block is still owned by this adapter.
                unsafe {
                    let _ = GlobalFree(Some(handle));
                }
            }
        }
    }

    /// `CF_UNICODETEXT` is the documented predefined format number 13; the bindings'
    /// constant lives behind a feature this crate does not need for anything else.
    const CF_UNICODETEXT: u32 = 13;
    const CF_DIB: u32 = 8;
    const CF_DIBV5: u32 = 17;

    enum ReadSnapshot {
        Ready(Snapshot),
        Dib {
            bytes: Vec<u8>,
            markers: Vec<clipboard::Marker>,
            sequence: u32,
        },
    }

    /// Reads marker metadata before choosing or copying any image representation.
    fn read_open_clipboard(sequence: u32) -> Result<ReadSnapshot, ClipboardError> {
        let has_text = unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_ok();

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

        let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
        let markers = classify_names(&borrowed, history_allowed_zero);
        let png_format = register("PNG")?;
        let available = |format| unsafe { IsClipboardFormatAvailable(format) }.is_ok();
        let selected = choose_format(
            has_text,
            available(png_format),
            available(CF_DIBV5),
            available(CF_DIB),
            !markers.is_empty(),
        );
        let content = match selected {
            Some(ClipboardFormat::Text) => Some(clipboard::ClipboardContent::Text(read_text(
                CF_UNICODETEXT,
            )?)),
            Some(ClipboardFormat::Png) => Some(clipboard::ClipboardContent::Image {
                png: with_global_bytes(png_format, |bytes| {
                    if bytes.len() as u64 > clipboard::MAX_IMAGE_BYTES {
                        return Err(clipboard::ClipboardError::TooLarge);
                    }
                    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                        return Err(clipboard::ClipboardError::Invalid);
                    }
                    Ok(bytes.to_vec())
                })?,
            }),
            Some(format @ (ClipboardFormat::DibV5 | ClipboardFormat::Dib)) => {
                let format = if format == ClipboardFormat::DibV5 {
                    CF_DIBV5
                } else {
                    CF_DIB
                };
                let bytes = with_global_bytes(format, |bytes| {
                    image::dib_storage_bytes(bytes)
                        .map(|length| length.map(|end| bytes[..end].to_vec()))
                })?;
                return Ok(match bytes {
                    Some(bytes) => ReadSnapshot::Dib {
                        bytes,
                        markers,
                        sequence,
                    },
                    None => ReadSnapshot::Ready(Snapshot {
                        content: None,
                        markers,
                        sequence,
                    }),
                });
            }
            None => None,
        };
        Ok(ReadSnapshot::Ready(Snapshot {
            content,
            markers,
            sequence,
        }))
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

    fn with_global_bytes<T>(
        format: u32,
        read: impl FnOnce(&[u8]) -> Result<T, clipboard::ClipboardError>,
    ) -> Result<T, ClipboardError> {
        let handle = unsafe { GetClipboardData(format) }.map_err(|_| ClipboardError::Gone)?;
        if handle.is_invalid() {
            return Err(ClipboardError::Gone);
        }
        let handle = HGLOBAL(handle.0);
        let pointer = unsafe { GlobalLock(handle) };
        if pointer.is_null() {
            return Err(ClipboardError::Gone);
        }
        struct Locked(HGLOBAL);
        impl Drop for Locked {
            fn drop(&mut self) {
                // SAFETY: the same thread successfully locked this handle.
                unsafe {
                    let _ = GlobalUnlock(self.0);
                }
            }
        }
        let _lock = Locked(handle);
        let size = unsafe { GlobalSize(handle) };
        if size > isize::MAX as usize {
            return Err(ClipboardError::Content(clipboard::ClipboardError::Invalid));
        }
        // SAFETY: GlobalSize bounds the successfully locked allocation.
        let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) };
        read(bytes).map_err(ClipboardError::Content)
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
        // SAFETY: the locked allocation contains at most GlobalSize / 2 UTF-16 units.
        // The helper must find a terminator inside this slice, never beyond the block.
        let size = unsafe { GlobalSize(HGLOBAL(handle.0)) };
        let result = if size / 2 > isize::MAX as usize / 2 {
            Err(clipboard::ClipboardError::Invalid)
        } else {
            let units = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>(), size / 2) };
            decode_text(units)
        };
        // SAFETY: this thread successfully locked the handle above.
        unsafe {
            let _ = GlobalUnlock(HGLOBAL(handle.0));
        }
        result.map_err(ClipboardError::Content)
    }

    /// Reads a `DWORD` payload, as the history flag format carries.
    fn read_dword(format: u32) -> Result<Option<u32>, ClipboardError> {
        with_global_bytes(format, |bytes| {
            Ok(bytes
                .get(..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes"))))
        })
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
    fn bounded_text_checks_termination_and_utf8_size() {
        use clipboard::ClipboardError::{Invalid, TooLarge};
        assert_eq!(decode_text(&[]), Err(Invalid));
        assert_eq!(decode_text(&[0]), Ok(String::new()));
        assert_eq!(decode_text(&[65]), Err(Invalid));
        assert_eq!(decode_text(&[65, 0]), Ok("A".to_owned()));
        assert_eq!(decode_text(&[65, 0, 66]), Ok("A".to_owned()));
        let max = clipboard::MAX_BYTES as usize;
        let mut units = vec![65; max];
        units.push(0);
        assert_eq!(decode_text(&units).unwrap().len(), max);
        units.insert(0, 65);
        assert_eq!(decode_text(&units), Err(TooLarge));
        let mut wide = vec![0x0800; max / 3 + 1];
        wide.push(0);
        assert_eq!(decode_text(&wide), Err(TooLarge));
        assert_eq!(decode_text(&[0xD800, 0]), Ok("\u{fffd}".to_owned()));
        assert_eq!(decode_text(&[0xD83E, 0xDD80, 0]), Ok("🦀".to_owned()));
    }

    #[test]
    fn format_choice_keeps_text_first_and_never_reads_secret_images() {
        assert_eq!(
            choose_format(true, true, true, true, false),
            Some(ClipboardFormat::Text)
        );
        assert_eq!(
            choose_format(true, true, true, true, true),
            Some(ClipboardFormat::Text)
        );
        assert_eq!(
            choose_format(false, true, true, true, false),
            Some(ClipboardFormat::Png)
        );
        assert_eq!(
            choose_format(false, false, true, true, false),
            Some(ClipboardFormat::DibV5)
        );
        assert_eq!(
            choose_format(false, false, false, true, false),
            Some(ClipboardFormat::Dib)
        );
        assert_eq!(choose_format(false, false, false, false, false), None);
        assert_eq!(choose_format(false, true, true, true, true), None);
    }
}
