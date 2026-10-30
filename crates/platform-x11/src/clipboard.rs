//! The X11 clipboard: the `INCR` protocol as a pure machine, and (on Linux) the CLIPBOARD
//! selection behind it.
//!
//! Redirection for `SelectionRequest`/`SelectionNotify` is not a thing in X11: a selection is
//! a property on the *requestor's* window, so a payload larger than the requestor's maximum
//! request size is transferred by the ICCCM `INCR` convention — the owner answers with the
//! property type `INCR` and a 32-bit total size, then writes the data in chunks, each chunk
//! acknowledged by the requestor deleting the property, and a zero-length chunk ends it.
//!
//! That protocol is pure bookkeeping, so it lives here without x11rb and is unit-tested on
//! any host: the adapter below is the `open()`/property/property-notify plumbing around it.
//!
//! `x-kde-passwordManagerHint` with the value `secret` is KDE's way of marking an item the
//! password manager owns; `clipboard::Marker::KdeSecret` is what it maps to, and the
//! classification here is the same pure decision Windows has.

use std::ops::Range;

/// The default maximum request size this node will push through one property write.
///
/// ICCCM leaves the value to the requestor (`MAX-REQUEST-SIZE`); 64 KiB is what X clients
/// commonly advertise, and it keeps one property write inside the server's usual buffer.
pub const DEFAULT_MAX_REQUEST_SIZE: usize = 64 * 1024;

/// `x-kde-passwordManagerHint`: the property KDE's password managers set.
pub const KDE_PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// The value that means "this is a secret".
pub const KDE_SECRET: &str = "secret";

/// Why an `INCR` transfer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrError {
    /// The requestor was handed more bytes than the owner announced.
    Overrun,
    /// A chunk arrived after the transfer had already ended.
    AfterEnd,
    /// The announced total was zero: an empty selection is not an `INCR` transfer.
    Empty,
}

/// Whether a payload needs the `INCR` convention at all.
///
/// Exactly at the requestor's maximum it fits in one property write, so only a strictly
/// larger payload is streamed.
pub fn needs_incr(size: usize, max_request_size: usize) -> bool {
    size > max_request_size
}

/// The owner's half: which slice to write next, and when the transfer is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    total: usize,
    chunk: usize,
    sent: usize,
}

impl Owner {
    /// Starts an `INCR` transfer of `total` bytes, `chunk` bytes per property write.
    pub fn start(total: usize, chunk: usize) -> Result<Self, IncrError> {
        if total == 0 {
            return Err(IncrError::Empty);
        }
        Ok(Owner {
            total,
            chunk: chunk.max(1),
            sent: 0,
        })
    }

    /// The next slice to write, or an empty range when the transfer is finished.
    ///
    /// An empty range is the documented zero-length chunk: it is written once, and the
    /// requestor's delete of the property after it completes the transfer.
    pub fn next_slice(&mut self) -> Range<usize> {
        let start = self.sent;
        let end = (start + self.chunk).min(self.total);
        self.sent = end;
        start..end
    }

    /// Whether everything has already been handed over.
    pub fn is_finished(&self) -> bool {
        self.sent >= self.total
    }

    /// The total this transfer announced.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// The requestor's half: how much of the announced payload has arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requestor {
    total: usize,
    received: usize,
    finished: bool,
}

impl Requestor {
    /// Starts collecting an `INCR` payload that announced `total` bytes.
    pub fn start(total: usize) -> Result<Self, IncrError> {
        if total == 0 {
            return Err(IncrError::Empty);
        }
        Ok(Requestor {
            total,
            received: 0,
            finished: false,
        })
    }

    /// Takes one property write's worth of bytes.
    pub fn accept(&mut self, bytes: usize) -> Result<(), IncrError> {
        // A zero-length write is the end marker. It is legal once the announced total has
        // arrived *and* while it has not — that is how a truncating owner is noticed rather
        // than silently accepted.
        if bytes == 0 {
            self.finished = true;
            return Ok(());
        }
        if self.finished {
            return Err(IncrError::AfterEnd);
        }
        if self.received + bytes > self.total {
            return Err(IncrError::Overrun);
        }
        self.received += bytes;
        if self.received == self.total {
            self.finished = true;
        }
        Ok(())
    }

    /// Whether the end marker arrived.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// How many bytes are still expected.
    pub fn expected(&self) -> usize {
        self.total.saturating_sub(self.received)
    }

    /// Whether the announced total arrived in full.
    pub fn is_complete(&self) -> bool {
        self.finished && self.received == self.total
    }
}

/// Classifies the KDE hint's *value*, as read from the selection's property.
///
/// Pure: the adapter reads the property, this decides.
pub fn kde_hint_is_secret(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .map(|text| text.trim_matches('\0').eq_ignore_ascii_case(KDE_SECRET))
        .unwrap_or(false)
}

/// The targets this adapter advertises from `TARGETS`, in the order it answers them.
///
/// ICCCM section 2.6.2 requires `TIMESTAMP` (so a requestor can discover the acquisition
/// time and reject stale requests) and `TARGETS` itself; `UTF8_STRING` is what modern
/// clients paste as, and `TEXT` is the polymorphic fallback. These are names, not atoms:
/// interning them needs a connection, this list does not.
pub const SELECTION_TARGET_NAMES: [&str; 4] = ["TIMESTAMP", "TARGETS", "UTF8_STRING", "TEXT"];

/// PNG selections advertise no text conversion.
pub const IMAGE_TARGET_NAMES: [&str; 3] = ["TIMESTAMP", "TARGETS", "image/png"];

/// Fingerprint computed on the clipboard actor or blocking apply worker, never the input loop.
pub fn content_hash(content: &clipboard::ClipboardContent) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.kind().mime().hash(&mut hasher);
    content.as_bytes().hash(&mut hasher);
    hasher.finish()
}

/// Text conversion wins even when PNG is also offered. Secret images are never requested.
#[cfg(any(target_os = "linux", test))]
fn read_content<E: From<clipboard::ClipboardError>>(
    mut read: impl FnMut(&str) -> Result<Option<Vec<u8>>, E>,
) -> Result<(Option<clipboard::ClipboardContent>, Vec<clipboard::Marker>), E> {
    // The adapter maps TARGETS atoms to these names. Only absent TARGETS uses legacy probes.
    let targets = read("TARGETS")?;
    let offers = |name: &[u8]| {
        targets.as_ref().is_some_and(|targets| {
            targets
                .split(|byte| *byte == 0)
                .any(|target| target == name)
        })
    };
    let utf8 = targets.is_none() || offers(b"UTF8_STRING");
    let text_target = targets.is_none() || offers(b"TEXT");
    let text = if utf8 { read("UTF8_STRING")? } else { None };
    let text = if text.is_none() && text_target {
        read("TEXT")?
    } else {
        text
    };
    let markers = if read(KDE_PASSWORD_HINT)?
        .as_deref()
        .is_some_and(kde_hint_is_secret)
    {
        vec![clipboard::Marker::KdeSecret]
    } else {
        Vec::new()
    };
    if utf8 || text_target {
        let content = text
            .map(|bytes| {
                if bytes.len() > 2 * clipboard::MAX_BYTES as usize {
                    return Err(clipboard::ClipboardError::TooLarge);
                }
                String::from_utf8(bytes)
                    .map(clipboard::ClipboardContent::Text)
                    .map_err(|_| clipboard::ClipboardError::Invalid)
            })
            .transpose()?;
        if content.is_none() && targets.is_some() {
            return Err(clipboard::ClipboardError::Invalid.into());
        }
        return Ok((content, markers));
    }
    if !markers.is_empty() {
        // Report the secret copy so core invalidates older carry without retaining image bytes.
        return Ok((
            Some(clipboard::ClipboardContent::Text(String::new())),
            markers,
        ));
    }
    if !offers(b"image/png") {
        return Ok((None, markers));
    }
    let png = read("image/png")?.ok_or(clipboard::ClipboardError::Invalid)?;
    let content = Some(clipboard::ClipboardContent::from_bytes(
        clipboard::ContentKind::Png,
        png,
    )?);
    Ok((content, markers))
}

#[cfg(any(target_os = "linux", test))]
fn within_cap(received: usize, additional: usize, cap: usize) -> bool {
    additional <= cap.saturating_sub(received) && received <= cap
}

#[cfg(any(target_os = "linux", test))]
fn read_expired(now: std::time::Instant, deadline: std::time::Instant) -> bool {
    now >= deadline
}

/// The chunk size this node writes per property for its own `INCR` transfers.
///
/// ICCCM section 2.7.2 says each chunk "should be less than the maximum-request-size in
/// the connection handshake", so the requestor's maximum is the natural chunk; a zero would
/// make the transfer spin forever, so it is floored at one byte.
pub fn incr_chunk_size(max_request_size: usize) -> usize {
    max_request_size.max(1)
}

/// Decodes a clipboard payload as UTF-8, or `None` when it is not text.
///
/// A payload that is not valid UTF-8 is not text; never lossily rewrite native bytes.
pub fn decode_text_payload(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// The X11 `CLIPBOARD` adapter: ownership, change observation and the `INCR` wire dance.
///
/// The machine above owns the *arithmetic* of an `INCR` transfer; this owns the X plumbing
/// around it. One background thread keeps a single connection pumping, so a
/// `SelectionRequest` from another client (a paste of our own selection) is served while the
/// caller does something else, and `wait`/`read`/`set` are messages to that thread. The
/// public API is `&self` because the thread holds the mutable state.
#[cfg(target_os = "linux")]
mod x11_impl {
    use std::fmt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use clipboard::{ClipboardContent, Marker};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::{
        self, Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
        EventMask, PropMode, Property, SelectionNotifyEvent, SelectionRequestEvent, Timestamp,
        Window, WindowClass,
    };
    use x11rb::protocol::Event;
    use x11rb::rust_connection::RustConnection;

    use super::{
        content_hash, incr_chunk_size, needs_incr, read_content, read_expired, within_cap, Owner,
        Requestor, DEFAULT_MAX_REQUEST_SIZE, IMAGE_TARGET_NAMES, KDE_PASSWORD_HINT,
        SELECTION_TARGET_NAMES,
    };

    /// How long one `read` step waits for the X reply it needs.
    const READ_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long the pump blocks on the connection before checking for a command again.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    /// The atom value `None`.
    const NONE_ATOM: Atom = 0;
    /// The X protocol's `CurrentTime`.
    const CURRENT_TIME: Timestamp = 0;

    /// What one clipboard read found.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Snapshot {
        /// Native text or untouched PNG, absent for unsupported/secret image selections.
        pub content: Option<ClipboardContent>,
        /// Fingerprint computed by the actor, so polling remains constant-time.
        pub hash: Option<u64>,
        /// Every secret marker the item carried.
        pub markers: Vec<Marker>,
    }

    impl Snapshot {
        fn new(content: Option<ClipboardContent>, markers: Vec<Marker>) -> Self {
            let hash = content.as_ref().map(content_hash);
            Self {
                content,
                hash,
                markers,
            }
        }
    }

    /// Why an adapter operation failed.
    #[derive(Debug)]
    pub enum ClipboardError {
        /// Opening the display named by `$DISPLAY` failed.
        Connect(x11rb::errors::ConnectError),
        /// The X connection broke, or a request could not be written to it.
        Connection(x11rb::errors::ConnectionError),
        /// The server answered a request with an X protocol error.
        Reply(x11rb::errors::ReplyError),
        /// The other side did not answer in time.
        Timeout,
        /// A newer operation or shutdown cancelled this read.
        Cancelled,
        /// The changed selection is oversized or contains invalid bytes.
        Content(clipboard::ClipboardError),
        /// The server lacks something this adapter requires.
        Unsupported(String),
        /// The adapter reached a state it cannot continue from.
        Internal(String),
    }

    impl fmt::Display for ClipboardError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                ClipboardError::Connect(err) => write!(f, "cannot connect to the X display: {err}"),
                ClipboardError::Connection(err) => write!(f, "X connection error: {err}"),
                ClipboardError::Reply(err) => write!(f, "X request failed: {err}"),
                ClipboardError::Timeout => {
                    f.write_str("the selection owner did not answer in time")
                }
                ClipboardError::Cancelled => f.write_str("the clipboard read was cancelled"),
                ClipboardError::Content(error) => f.write_str(error.code()),
                ClipboardError::Unsupported(what) => write!(f, "unsupported X server: {what}"),
                ClipboardError::Internal(what) => write!(f, "clipboard adapter error: {what}"),
            }
        }
    }

    impl std::error::Error for ClipboardError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                ClipboardError::Connect(err) => Some(err),
                ClipboardError::Connection(err) => Some(err),
                ClipboardError::Reply(err) => Some(err),
                ClipboardError::Timeout
                | ClipboardError::Cancelled
                | ClipboardError::Content(_)
                | ClipboardError::Unsupported(_)
                | ClipboardError::Internal(_) => None,
            }
        }
    }

    impl From<x11rb::errors::ConnectError> for ClipboardError {
        fn from(err: x11rb::errors::ConnectError) -> Self {
            ClipboardError::Connect(err)
        }
    }

    impl From<x11rb::errors::ConnectionError> for ClipboardError {
        fn from(err: x11rb::errors::ConnectionError) -> Self {
            ClipboardError::Connection(err)
        }
    }

    impl From<x11rb::errors::ReplyError> for ClipboardError {
        fn from(err: x11rb::errors::ReplyError) -> Self {
            ClipboardError::Reply(err)
        }
    }

    impl From<clipboard::ClipboardError> for ClipboardError {
        fn from(error: clipboard::ClipboardError) -> Self {
            Self::Content(error)
        }
    }

    /// A change to the CLIPBOARD selection's owner was seen.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Change;

    /// The reply channel of one command.
    type ReadReply = Sender<Result<Snapshot, ClipboardError>>;
    type SetReply = Sender<Result<(), ClipboardError>>;

    /// What the caller sends the background thread.
    enum Command {
        Read(ReadReply, u64),
        Set(ClipboardContent, SetReply),
        Shutdown,
    }

    /// The atoms one adapter works with, interned once at connect.
    #[derive(Debug, Clone, Copy)]
    struct Atoms {
        clipboard: Atom,
        timestamp: Atom,
        targets: Atom,
        utf8_string: Atom,
        text: Atom,
        png: Atom,
        incr: Atom,
        kde_hint: Atom,
        /// Where a text reply is asked for.
        read_property: Atom,
        /// Where the KDE hint reply is asked for.
        hint_property: Atom,
        /// A scratch property used to read the server clock.
        time_property: Atom,
    }

    /// An `INCR` transfer we are streaming to one requestor.
    struct ActiveOwner {
        requestor: Window,
        property: Atom,
        target: Atom,
        payload: Arc<ClipboardContent>,
        owner: Owner,
    }

    /// The thread's mutable state.
    struct State {
        /// The payload we last published; shared with any active INCR transfer.
        owned: Option<Arc<ClipboardContent>>,
        /// The time we acquired ownership, answered as `TIMESTAMP`.
        ownership_time: Timestamp,
        /// The `INCR` transfer currently in flight, if any.
        active: Option<ActiveOwner>,
        generation: Arc<AtomicU64>,
        operation: u64,
    }

    /// The CLIPBOARD selection owner + observer for one X connection.
    pub struct Clipboard {
        commands: Sender<Command>,
        changes: Receiver<Change>,
        thread: Option<JoinHandle<()>>,
        generation: Arc<AtomicU64>,
    }

    /// A cancellable clipboard read; polling never waits for the selection owner.
    pub struct PendingRead {
        reply: Receiver<Result<Snapshot, ClipboardError>>,
        generation: Arc<AtomicU64>,
        operation: u64,
    }

    impl PendingRead {
        /// Returns the completed snapshot, or `None` while the actor is still reading.
        pub fn poll(&self) -> Result<Option<Snapshot>, ClipboardError> {
            match self.reply.try_recv() {
                Ok(result) => result.map(Some),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => Err(gone()),
            }
        }
    }

    impl Drop for PendingRead {
        fn drop(&mut self) {
            let _ = self.generation.compare_exchange(
                self.operation,
                self.operation.wrapping_add(1),
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    impl Clipboard {
        /// Opens `$DISPLAY`, creates an unmapped window and attaches to the CLIPBOARD
        /// selection's change notifications.
        pub fn connect() -> Result<Self, ClipboardError> {
            let (conn, screen_num) = x11rb::connect(None)?;
            let root = conn.setup().roots[screen_num].root;

            let window = conn.generate_id().map_err(|err| {
                ClipboardError::Internal(format!("the server has no window id to give: {err}"))
            })?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?;

            let version = conn
                .xfixes_query_version(5, 0)
                .map_err(map_extension_error)?
                .reply()?;
            if version.major_version < 5 {
                return Err(ClipboardError::Unsupported(format!(
                    "XFixes 5 is required to observe selection changes, the server offers {}.{}",
                    version.major_version, version.minor_version
                )));
            }

            let atoms = Atoms {
                clipboard: intern(&conn, "CLIPBOARD")?,
                timestamp: intern(&conn, SELECTION_TARGET_NAMES[0])?,
                targets: intern(&conn, SELECTION_TARGET_NAMES[1])?,
                utf8_string: intern(&conn, SELECTION_TARGET_NAMES[2])?,
                text: intern(&conn, SELECTION_TARGET_NAMES[3])?,
                png: intern(&conn, IMAGE_TARGET_NAMES[2])?,
                incr: intern(&conn, "INCR")?,
                kde_hint: intern(&conn, KDE_PASSWORD_HINT)?,
                read_property: intern(&conn, "BIFROST_CLIPBOARD_SELECTION")?,
                hint_property: intern(&conn, "BIFROST_CLIPBOARD_KDE_HINT")?,
                time_property: intern(&conn, "BIFROST_CLIPBOARD_TIME")?,
            };

            conn.xfixes_select_selection_input(
                window,
                atoms.clipboard,
                SelectionEventMask::SET_SELECTION_OWNER,
            )?;
            // Flush before returning: the subscription must be live before the caller can
            // observe anything, or a change right after `connect` is lost.
            conn.flush()?;

            let (command_tx, command_rx) = mpsc::channel();
            let (change_tx, change_rx) = mpsc::channel();
            let generation = Arc::new(AtomicU64::new(0));
            let worker_generation = Arc::clone(&generation);
            let thread = std::thread::Builder::new()
                .name("bifrost-x11-clipboard".to_owned())
                .spawn(move || {
                    run(
                        conn,
                        window,
                        atoms,
                        change_tx,
                        command_rx,
                        worker_generation,
                    )
                })
                .map_err(|err| {
                    ClipboardError::Internal(format!("cannot start the clipboard thread: {err}"))
                })?;

            Ok(Clipboard {
                commands: command_tx,
                changes: change_rx,
                thread: Some(thread),
                generation,
            })
        }

        /// Blocks up to `timeout` for the next CLIPBOARD owner change.
        ///
        /// A served `SelectionRequest` is *not* reported: the thread handles those without
        /// the caller noticing, which is what lets a paste happen while the caller is idle.
        pub fn wait(&self, timeout: Duration) -> Option<Change> {
            self.changes.recv_timeout(timeout).ok()
        }

        /// Reads text or PNG, including INCR, and classifies the KDE secret hint.
        pub fn read(&self) -> Result<Snapshot, ClipboardError> {
            let pending = self.start_read()?;
            pending.reply.recv().map_err(|_| gone())?
        }

        /// Starts a read on the existing actor without blocking the caller.
        pub fn start_read(&self) -> Result<PendingRead, ClipboardError> {
            let operation = self
                .generation
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1);
            let (tx, reply) = mpsc::channel();
            self.send(Command::Read(tx, operation))?;
            Ok(PendingRead {
                reply,
                generation: Arc::clone(&self.generation),
                operation,
            })
        }

        /// Takes ownership and serves native targets, using INCR for large payloads.
        pub fn set(&self, content: ClipboardContent) -> Result<(), ClipboardError> {
            self.start_set(content)?.recv().map_err(|_| gone())?
        }

        /// Enqueues an owned write; only the returned receiver waits for native confirmation.
        pub fn start_set(
            &self,
            content: ClipboardContent,
        ) -> Result<mpsc::Receiver<Result<(), ClipboardError>>, ClipboardError> {
            self.generation.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel();
            self.send(Command::Set(content, tx))?;
            Ok(rx)
        }

        /// Clones only the command handle, never the clipboard payload or input backend.
        pub fn owned_setter(
            &self,
        ) -> impl FnOnce(ClipboardContent) -> Result<(), ClipboardError> + Send + 'static {
            let commands = self.commands.clone();
            let generation = Arc::clone(&self.generation);
            move |content| {
                generation.fetch_add(1, Ordering::SeqCst);
                let (tx, rx) = mpsc::channel();
                commands
                    .send(Command::Set(content, tx))
                    .map_err(|_| gone())?;
                rx.recv().map_err(|_| gone())?
            }
        }

        fn send(&self, command: Command) -> Result<(), ClipboardError> {
            self.commands.send(command).map_err(|_| gone())
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            // Tell the thread to release the selection and end. It never blocks for long:
            // the pump wakes on a 20 ms tick even when the connection stays quiet.
            self.generation.fetch_add(1, Ordering::SeqCst);
            let _ = self.commands.send(Command::Shutdown);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn gone() -> ClipboardError {
        ClipboardError::Internal("the clipboard thread is gone".to_owned())
    }

    fn map_extension_error(err: x11rb::errors::ConnectionError) -> ClipboardError {
        match err {
            x11rb::errors::ConnectionError::UnsupportedExtension => {
                ClipboardError::Unsupported("this server has no XFixes extension".to_owned())
            }
            other => ClipboardError::Connection(other),
        }
    }

    fn intern(conn: &RustConnection, name: &str) -> Result<Atom, ClipboardError> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    }

    /// The thread: pump the connection, answer requests, execute commands.
    fn run(
        mut conn: RustConnection,
        window: Window,
        atoms: Atoms,
        changes: Sender<Change>,
        commands: Receiver<Command>,
        generation: Arc<AtomicU64>,
    ) {
        let mut state = State {
            owned: None,
            ownership_time: 0,
            active: None,
            operation: generation.load(Ordering::SeqCst),
            generation,
        };
        loop {
            // Serve everything already queued first: a paste must not wait for the command
            // channel, and a change must reach `wait` promptly.
            let mut live = true;
            loop {
                match conn.poll_for_event() {
                    Ok(Some(event)) => {
                        if handle_event(&mut conn, window, &atoms, &mut state, &changes, event)
                            .is_err()
                        {
                            live = false;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        live = false;
                        break;
                    }
                }
            }
            if !live {
                break;
            }

            match commands.try_recv() {
                Ok(Command::Read(reply, operation)) => {
                    state.operation = operation;
                    let result = read_selection(&mut conn, window, &atoms, &mut state, &changes);
                    let _ = reply.send(result);
                }
                Ok(Command::Set(text, reply)) => {
                    state.operation = state.generation.load(Ordering::SeqCst);
                    let result =
                        take_ownership(&mut conn, window, &atoms, &mut state, &changes, text);
                    let _ = reply.send(result);
                }
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => break,
                Err(TryRecvError::Empty) => {}
            }

            if wait_readable(&conn, POLL_INTERVAL).is_err() {
                break;
            }
        }

        // Release the selection and destroy the window that owned it; the server then frees
        // both and XFixes tells every other client the selection is unowned.
        let _ = conn.set_selection_owner(AtomEnum::NONE, atoms.clipboard, CURRENT_TIME);
        let _ = conn.destroy_window(window);
        let _ = conn.flush();
    }

    fn handle_event(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
        event: Event,
    ) -> Result<(), ClipboardError> {
        match event {
            Event::XfixesSelectionNotify(event) => {
                if event.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER
                    && event.selection == atoms.clipboard
                {
                    let _ = changes.send(Change);
                }
            }
            Event::SelectionRequest(request) => serve_request(conn, window, atoms, state, request)?,
            Event::SelectionClear(event) => {
                if event.selection == atoms.clipboard && event.owner == window {
                    state.owned = None;
                }
            }
            // An `INCR` acknowledgement arrives on the *requestor's* window, which is why
            // the transfer is matched by (window, atom) and not by `window == ours`.
            Event::PropertyNotify(event) if event.state == Property::DELETE => {
                advance_transfer(conn, state, event.window, event.atom)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Answers one `SelectionRequest` (ICCCM section 2.2).
    fn serve_request(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        request: SelectionRequestEvent,
    ) -> Result<(), ClipboardError> {
        if request.owner != window || request.selection != atoms.clipboard {
            return Ok(());
        }
        // ICCCM 2.2: "all properties used to reply to SelectionRequest events must be
        // placed on the requestor window", not on ours.
        let target_window = request.requestor;
        // An obsolete requestor sends property None; ICCCM says to use the target atom as
        // the property name then.
        let property = if request.property == NONE_ATOM {
            request.target
        } else {
            request.property
        };

        if request.target == atoms.targets {
            let image = state
                .owned
                .as_ref()
                .is_some_and(|content| content.kind() == clipboard::ContentKind::Png);
            let targets: &[Atom] = if image {
                &[atoms.timestamp, atoms.targets, atoms.png]
            } else {
                &[
                    atoms.timestamp,
                    atoms.targets,
                    atoms.utf8_string,
                    atoms.text,
                ]
            };
            let mut data = Vec::with_capacity(targets.len() * 4);
            for atom in targets {
                data.extend_from_slice(&atom.to_ne_bytes());
            }
            change_property(
                conn,
                target_window,
                property,
                AtomEnum::ATOM.into(),
                32,
                &data,
            )?;
            return notify(conn, atoms, request, property);
        }

        if request.target == atoms.timestamp {
            change_property(
                conn,
                target_window,
                property,
                AtomEnum::INTEGER.into(),
                32,
                &state.ownership_time.to_ne_bytes(),
            )?;
            return notify(conn, atoms, request, property);
        }

        let supported = state
            .owned
            .as_ref()
            .is_some_and(|content| match content.as_ref() {
                ClipboardContent::Text(_) => {
                    request.target == atoms.utf8_string || request.target == atoms.text
                }
                ClipboardContent::Image { .. } => request.target == atoms.png,
            });
        if supported {
            let content = Arc::clone(state.owned.as_ref().expect("supported payload"));
            let payload = content.as_bytes();
            if needs_incr(payload.len(), DEFAULT_MAX_REQUEST_SIZE) {
                let total = payload.len() as u32;
                change_property(
                    conn,
                    target_window,
                    property,
                    atoms.incr,
                    32,
                    &total.to_ne_bytes(),
                )?;
                // The chunks live on the requestor's window; PropertyChangeMask there is
                // how the owner learns the requestor consumed each one (ICCCM 2.2 / 2.7.2).
                conn.change_window_attributes(
                    target_window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?;
                let owner = Owner::start(payload.len(), incr_chunk_size(DEFAULT_MAX_REQUEST_SIZE))
                    .map_err(|err| {
                        ClipboardError::Internal(format!("cannot start an INCR transfer: {err:?}"))
                    })?;
                state.active = Some(ActiveOwner {
                    requestor: target_window,
                    property,
                    target: request.target,
                    payload: content,
                    owner,
                });
            } else {
                change_property(conn, target_window, property, request.target, 8, payload)?;
            }
            return notify(conn, atoms, request, property);
        }

        // Refuse every target not belonging to this payload.
        notify(conn, atoms, request, NONE_ATOM)
    }

    /// Writes the next `INCR` chunk after the requestor deleted the property.
    fn advance_transfer(
        conn: &mut RustConnection,
        state: &mut State,
        window: Window,
        atom: Atom,
    ) -> Result<(), ClipboardError> {
        let Some(mut active) = state.active.take() else {
            return Ok(());
        };
        if active.requestor != window || active.property != atom {
            state.active = Some(active);
            return Ok(());
        }
        let slice = active.owner.next_slice();
        let chunk = &active.payload.as_bytes()[slice.clone()];
        change_property(
            conn,
            active.requestor,
            active.property,
            active.target,
            8,
            chunk,
        )?;
        // An empty slice is the documented zero-length end marker: the transfer is over.
        if !slice.is_empty() {
            state.active = Some(active);
        }
        Ok(())
    }

    /// Sends the `SelectionNotify` that acknowledges a `SelectionRequest`.
    fn notify(
        conn: &mut RustConnection,
        atoms: &Atoms,
        request: SelectionRequestEvent,
        property: Atom,
    ) -> Result<(), ClipboardError> {
        let event = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: atoms.clipboard,
            target: request.target,
            property,
        };
        conn.send_event(false, request.requestor, EventMask::NO_EVENT, event)?;
        conn.flush()?;
        Ok(())
    }

    /// Writes a property with the mode and element count the X server needs.
    fn change_property(
        conn: &mut RustConnection,
        window: Window,
        property: Atom,
        type_: Atom,
        format: u8,
        data: &[u8],
    ) -> Result<(), ClipboardError> {
        let elements = match format {
            32 => data.len() / 4,
            16 => data.len() / 2,
            _ => data.len(),
        };
        conn.change_property(
            PropMode::REPLACE,
            window,
            property,
            type_,
            format,
            elements as u32,
            data,
        )?;
        conn.flush()?;
        Ok(())
    }

    /// Takes CLIPBOARD and remembers the validated native payload.
    fn take_ownership(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
        content: ClipboardContent,
    ) -> Result<(), ClipboardError> {
        if content.len() as u64 > content.kind().hard_max_bytes() {
            return Err(ClipboardError::Internal(
                "clipboard payload exceeds its hard cap".to_owned(),
            ));
        }
        if matches!(&content, ClipboardContent::Image { png } if !png.starts_with(b"\x89PNG\r\n\x1a\n"))
        {
            return Err(ClipboardError::Internal("invalid PNG signature".to_owned()));
        }
        let time = server_time(conn, window, atoms, state, changes)?;
        conn.set_selection_owner(window, atoms.clipboard, time)?;
        conn.flush()?;
        // ICCCM 2.1: `SetSelectionOwner` can merely *appear* to succeed, so confirm it.
        let owner = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
        if owner != window {
            return Err(ClipboardError::Internal(
                "the server did not hand us CLIPBOARD ownership".to_owned(),
            ));
        }
        state.owned = Some(Arc::new(content));
        state.ownership_time = time;
        state.active = None;
        Ok(())
    }

    /// Reads the current server time.
    ///
    /// ICCCM section 2.1: a zero-length append to a property reports the server clock in
    /// the matching `PropertyNotify`; it is the standard way to learn a timestamp for an
    /// action no event triggered, and the timestamp `TIMESTAMP` must answer.
    fn server_time(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
    ) -> Result<Timestamp, ClipboardError> {
        conn.change_property(
            PropMode::APPEND,
            window,
            atoms.time_property,
            AtomEnum::STRING,
            8,
            0,
            &[],
        )?;
        conn.flush()?;
        let deadline = Instant::now() + READ_TIMEOUT;
        let event = pump_until(conn, window, atoms, state, changes, deadline, |event| {
            matches!(
                event,
                Event::PropertyNotify(property)
                    if property.window == window && property.atom == atoms.time_property
            )
        })?;
        match event {
            Event::PropertyNotify(property) => Ok(property.time),
            _ => Err(ClipboardError::Internal(
                "the server clock reply was not a property notification".to_owned(),
            )),
        }
    }

    /// Reads the current owner's `target`, including an `INCR` transfer; `None` when the
    /// owner cannot convert it.
    fn read_selection(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
    ) -> Result<Snapshot, ClipboardError> {
        // Reading our own selection avoids a requestor/owner round trip to ourselves.
        let owner = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
        if owner == window {
            return Ok(Snapshot::new(state.owned.as_deref().cloned(), Vec::new()));
        }
        let (content, markers) = read_content(|name| -> Result<_, ClipboardError> {
            let (target, property) = match name {
                "UTF8_STRING" => (atoms.utf8_string, atoms.read_property),
                "TEXT" => (atoms.text, atoms.read_property),
                KDE_PASSWORD_HINT => (atoms.kde_hint, atoms.hint_property),
                "TARGETS" => (atoms.targets, atoms.read_property),
                "image/png" => (atoms.png, atoms.read_property),
                _ => unreachable!("known clipboard target"),
            };
            let bytes = read_target(conn, window, atoms, state, changes, target, property)?;
            if target == atoms.targets {
                return Ok(bytes.map(|bytes| {
                    let mut names = Vec::new();
                    for (atom, name) in [
                        (atoms.utf8_string, "UTF8_STRING"),
                        (atoms.text, "TEXT"),
                        (atoms.png, "image/png"),
                    ] {
                        if bytes
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .any(|word| u32::from_ne_bytes(*word) == atom)
                        {
                            names.extend_from_slice(name.as_bytes());
                            names.push(0);
                        }
                    }
                    names
                }));
            }
            Ok(bytes)
        })?;
        Ok(Snapshot::new(content, markers))
    }

    /// `ConvertSelection` for one target and reads the reply, `INCR` included.
    fn read_target(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
        target: Atom,
        property: Atom,
    ) -> Result<Option<Vec<u8>>, ClipboardError> {
        let cap = if target == atoms.png {
            clipboard::MAX_IMAGE_BYTES as usize
        } else if target == atoms.targets {
            DEFAULT_MAX_REQUEST_SIZE
        } else {
            2 * clipboard::MAX_BYTES as usize
        };
        conn.convert_selection(window, atoms.clipboard, target, property, CURRENT_TIME)?;
        conn.flush()?;

        let deadline = Instant::now() + READ_TIMEOUT;
        let event = pump_until(conn, window, atoms, state, changes, deadline, |event| {
            matches!(
                event,
                Event::SelectionNotify(notify)
                    if notify.requestor == window
                        && notify.selection == atoms.clipboard
                        && notify.target == target
            )
        })?;
        let notify = match event {
            Event::SelectionNotify(notify) => notify,
            _ => {
                return Err(ClipboardError::Internal(
                    "unexpected event while reading".to_owned(),
                ))
            }
        };
        if notify.property == NONE_ATOM {
            return Ok(None);
        }

        // Deleting the reply property is also what starts an `INCR` transfer: the owner
        // begins writing chunks only once it sees this deletion (ICCCM 2.7.2).
        let (type_, announced) = get_property_all(conn, window, notify.property, true, cap)?;
        if type_ != atoms.incr {
            return Ok(Some(announced));
        }

        // ICCCM says the INCR property holds a 32-bit lower bound on the payload, but the
        // real terminator is the zero-length property, and owners exist (xclip) that publish
        // a zero-length INCR property with no bound at all. An absent or empty bound is
        // therefore accounted as "unknown" and only caught by the end marker.
        let total = match announced_total(&announced) {
            Some(total) if total > 0 => total,
            _ => u32::MAX as usize,
        };
        if announced_total(&announced).is_some_and(|total| !within_cap(0, total, cap)) {
            return Err(clipboard::ClipboardError::TooLarge.into());
        }
        let mut requestor = Requestor::start(total).map_err(|err| {
            ClipboardError::Internal(format!("cannot start the INCR requestor: {err:?}"))
        })?;

        let mut payload = Vec::new();
        loop {
            pump_until(conn, window, atoms, state, changes, deadline, |event| {
                matches!(
                    event,
                    Event::PropertyNotify(property)
                        if property.window == window
                            && property.atom == notify.property
                            && property.state == Property::NEW_VALUE
                )
            })?;
            let (_, chunk) =
                get_property_all(conn, window, notify.property, true, cap - payload.len())?;
            if chunk.is_empty() {
                requestor.accept(0).map_err(|err| {
                    ClipboardError::Internal(format!("malformed INCR end: {err:?}"))
                })?;
                break;
            }
            requestor.accept(chunk.len()).map_err(|err| {
                ClipboardError::Internal(format!("the owner sent a malformed INCR stream: {err:?}"))
            })?;
            payload.extend_from_slice(&chunk);
            if requestor.is_finished() {
                break;
            }
        }
        Ok(Some(payload))
    }

    /// Reads a whole property and, when asked, deletes it.
    ///
    /// A zero-length first read reports only the size, so a chunk larger than a single
    /// request still comes back in one piece.
    fn get_property_all(
        conn: &mut RustConnection,
        window: Window,
        property: Atom,
        delete: bool,
        cap: usize,
    ) -> Result<(Atom, Vec<u8>), ClipboardError> {
        let probe = conn
            .get_property(false, window, property, AtomEnum::ANY, 0, 0)?
            .reply()?;
        let bytes_after = probe.bytes_after as usize;
        if !within_cap(0, bytes_after, cap) {
            return Err(clipboard::ClipboardError::TooLarge.into());
        }
        if bytes_after == 0 {
            if delete {
                conn.delete_property(window, property)?;
                conn.flush()?;
            }
            return Ok((probe.type_, Vec::new()));
        }
        let units = bytes_after.div_ceil(4) as u32;
        let reply = conn
            .get_property(delete, window, property, AtomEnum::ANY, 0, units)?
            .reply()?;
        if reply.bytes_after != 0 || !within_cap(0, reply.value.len(), cap) {
            return Err(clipboard::ClipboardError::TooLarge.into());
        }
        Ok((reply.type_, reply.value))
    }

    /// Loops the connection until `matches` accepts an event or the deadline passes.
    ///
    /// Every other event is handed to `handle_event`, so a paste of our own selection is
    /// still served while we are waiting on someone else.
    fn pump_until<F>(
        conn: &mut RustConnection,
        window: Window,
        atoms: &Atoms,
        state: &mut State,
        changes: &Sender<Change>,
        deadline: Instant,
        matches: F,
    ) -> Result<Event, ClipboardError>
    where
        F: Fn(&Event) -> bool,
    {
        loop {
            if state.operation != state.generation.load(Ordering::SeqCst) {
                return Err(ClipboardError::Cancelled);
            }
            let now = Instant::now();
            if read_expired(now, deadline) {
                return Err(ClipboardError::Timeout);
            }
            if let Some(event) = conn.poll_for_event()? {
                if matches(&event) {
                    return Ok(event);
                }
                handle_event(conn, window, atoms, state, changes, event)?;
                continue;
            }
            wait_readable(conn, (deadline - now).min(POLL_INTERVAL))?;
        }
    }

    /// Blocks until the X connection is readable or `timeout` elapses.
    ///
    /// The fd is polled rather than `wait_for_event`-ing, exactly like `capture.rs`, so the
    /// thread can also notice a command without an extra wake-up fd.
    fn wait_readable(conn: &RustConnection, timeout: Duration) -> Result<(), ClipboardError> {
        let timespec = rustix::event::Timespec {
            tv_sec: timeout.as_secs() as rustix::event::Secs,
            tv_nsec: u64::from(timeout.subsec_nanos()) as rustix::event::Nsecs,
        };
        let mut fds = [rustix::event::PollFd::new(
            conn.stream(),
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(&mut fds, Some(&timespec)).map_err(|err| {
            ClipboardError::Internal(format!("polling the X connection failed: {err}"))
        })?;
        Ok(())
    }

    /// The 32-bit word an `INCR` property starts with, when it has one.
    fn announced_total(property: &[u8]) -> Option<usize> {
        let word: [u8; 4] = property.get(..4)?.try_into().ok()?;
        Some(u32::from_ne_bytes(word) as usize)
    }

    #[cfg(test)]
    mod pending_read_tests {
        use super::*;

        #[test]
        fn dropping_an_old_read_cannot_cancel_a_newer_operation() {
            let generation = Arc::new(AtomicU64::new(2));
            let old = PendingRead {
                reply: mpsc::channel().1,
                generation: Arc::clone(&generation),
                operation: 1,
            };
            drop(old);
            assert_eq!(generation.load(Ordering::SeqCst), 2);
            let current = PendingRead {
                reply: mpsc::channel().1,
                generation: Arc::clone(&generation),
                operation: 2,
            };
            drop(current);
            assert_eq!(generation.load(Ordering::SeqCst), 3);
        }
    }
}

#[cfg(target_os = "linux")]
pub use x11_impl::{Change, Clipboard, ClipboardError, PendingRead, Snapshot};

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_read(
        offered: &[(&str, &[u8])],
    ) -> (
        Option<clipboard::ClipboardContent>,
        Vec<clipboard::Marker>,
        Vec<String>,
    ) {
        let mut requested = Vec::new();
        let (content, markers) = read_content(|name| {
            requested.push(name.to_owned());
            Ok::<_, clipboard::ClipboardError>(
                offered
                    .iter()
                    .find(|(target, _)| *target == name)
                    .map(|(_, bytes)| bytes.to_vec()),
            )
        })
        .unwrap();
        (content, markers, requested)
    }

    #[test]
    fn changed_selection_reports_large_text_and_propagates_unreadable_payload() {
        let text = vec![b'x'; 1536 * 1024];
        let (content, _, _) = fake_read(&[("TARGETS", b"UTF8_STRING"), ("UTF8_STRING", &text)]);
        assert_eq!(content.unwrap().as_bytes(), text);
        let result = read_content(|name| match name {
            "TARGETS" => Ok(Some(b"image/png".to_vec())),
            "image/png" => Err(clipboard::ClipboardError::Invalid),
            _ => Ok(None),
        });
        assert_eq!(result, Err(clipboard::ClipboardError::Invalid));
    }

    #[test]
    fn target_choice_prefers_utf8_then_text_and_never_fetches_other_formats() {
        for text_target in ["UTF8_STRING", "TEXT"] {
            let (content, markers, requested) = fake_read(&[
                (text_target, b"text"),
                ("TARGETS", b"UTF8_STRING\0TEXT\0image/png"),
                ("image/png", b"\x89PNG\r\n\x1a\n"),
            ]);
            assert_eq!(
                content,
                Some(clipboard::ClipboardContent::Text("text".to_owned()))
            );
            assert!(markers.is_empty());
            assert!(!requested.iter().any(|name| name == "image/png"));
        }
        let (content, _, requested) = fake_read(&[
            ("TARGETS", b"UTF8_STRING\0TEXT\0image/png"),
            ("UTF8_STRING", b"preferred"),
            ("TEXT", b"fallback"),
        ]);
        assert_eq!(
            content,
            Some(clipboard::ClipboardContent::Text("preferred".to_owned()))
        );
        assert_eq!(requested, ["TARGETS", "UTF8_STRING", KDE_PASSWORD_HINT]);
        let (content, _, requested) = fake_read(&[(
            "TARGETS",
            b"image/jpeg\0image/bmp\0text/html\0text/uri-list",
        )]);
        assert!(content.is_none());
        assert_eq!(requested, ["TARGETS", KDE_PASSWORD_HINT]);
        for payload in [None, Some(b"\xff".as_slice())] {
            let mut offered = vec![
                ("TARGETS", b"UTF8_STRING\0image/png".as_slice()),
                ("image/png", b"\x89PNG\r\n\x1a\n".as_slice()),
            ];
            if let Some(payload) = payload {
                offered.push(("UTF8_STRING", payload));
            }
            let mut requested = Vec::new();
            let result = read_content(|name| {
                requested.push(name.to_owned());
                Ok::<_, clipboard::ClipboardError>(
                    offered
                        .iter()
                        .find(|(target, _)| *target == name)
                        .map(|(_, bytes)| bytes.to_vec()),
                )
            });
            assert_eq!(result, Err(clipboard::ClipboardError::Invalid));
            assert!(!requested.iter().any(|name| name == "image/png"));
        }
    }

    #[test]
    fn png_selection_checks_secret_before_requesting_any_image_bytes() {
        let (content, markers, requested) = fake_read(&[
            (KDE_PASSWORD_HINT, b"secret"),
            ("TARGETS", b"image/png"),
            ("image/png", b"\x89PNG\r\n\x1a\n"),
        ]);
        assert_eq!(
            content,
            Some(clipboard::ClipboardContent::Text(String::new()))
        );
        assert_eq!(markers, [clipboard::Marker::KdeSecret]);
        assert_eq!(requested, ["TARGETS", KDE_PASSWORD_HINT]);
        let (text, markers, _) = fake_read(&[
            ("UTF8_STRING", b"secret text"),
            (KDE_PASSWORD_HINT, b"secret"),
        ]);
        assert_eq!(
            text,
            Some(clipboard::ClipboardContent::Text("secret text".to_owned()))
        );
        assert_eq!(markers, [clipboard::Marker::KdeSecret]);
    }

    #[test]
    fn png_is_untouched_and_signature_is_the_only_format_validation() {
        let png = b"\x89PNG\r\n\x1a\nnot decoded";
        let (content, _, requested) = fake_read(&[("TARGETS", b"image/png"), ("image/png", png)]);
        assert_eq!(
            content,
            Some(clipboard::ClipboardContent::Image { png: png.to_vec() })
        );
        assert_eq!(requested, ["TARGETS", KDE_PASSWORD_HINT, "image/png"]);
        for invalid in [b"".as_slice(), b"\x89PNG", b"JPEG"] {
            assert_eq!(
                read_content(|name| Ok::<_, clipboard::ClipboardError>(match name {
                    "TARGETS" => Some(b"image/png".to_vec()),
                    "image/png" => Some(invalid.to_vec()),
                    _ => None,
                })),
                Err(clipboard::ClipboardError::Invalid)
            );
        }
        assert_eq!(IMAGE_TARGET_NAMES, ["TIMESTAMP", "TARGETS", "image/png"]);
    }

    #[test]
    fn large_png_owner_and_requestor_round_trip_without_a_display() {
        let mut png = vec![0xa5; 2 * 1024 * 1024];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut owner = Owner::start(png.len(), DEFAULT_MAX_REQUEST_SIZE).unwrap();
        let mut requestor = Requestor::start(png.len()).unwrap();
        let mut received = Vec::new();
        loop {
            let slice = owner.next_slice();
            requestor.accept(slice.len()).unwrap();
            received.extend_from_slice(&png[slice.clone()]);
            if slice.is_empty() {
                break;
            }
        }
        assert!(requestor.is_complete());
        assert_eq!(received, png);
    }

    #[test]
    fn png_cap_and_timeout_do_not_grow_during_incr() {
        let cap = clipboard::MAX_IMAGE_BYTES as usize;
        assert!(within_cap(0, cap, cap));
        assert!(within_cap(cap - 1, 1, cap));
        assert!(!within_cap(cap, 1, cap));
        assert!(!within_cap(0, cap + 1, cap));
        assert!(!within_cap(usize::MAX, 1, cap));
        assert!(!within_cap(0, cap, clipboard::MAX_BYTES as usize));
        let now = std::time::Instant::now();
        let deadline = now + std::time::Duration::from_secs(10);
        assert!(!read_expired(now, deadline));
        assert!(!read_expired(
            deadline - std::time::Duration::from_nanos(1),
            deadline
        ));
        assert!(read_expired(deadline, deadline));
        assert!(read_expired(
            deadline + std::time::Duration::from_secs(1),
            deadline
        ));
        let mut oversized = vec![0; cap + 1];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert_eq!(
            read_content(|name| Ok::<_, clipboard::ClipboardError>(match name {
                "TARGETS" => Some(b"image/png".to_vec()),
                "image/png" => Some(oversized.clone()),
                _ => None,
            })),
            Err(clipboard::ClipboardError::TooLarge)
        );
    }

    #[test]
    fn the_threshold_is_exclusive() {
        assert!(!needs_incr(1000, DEFAULT_MAX_REQUEST_SIZE));
        assert!(!needs_incr(
            DEFAULT_MAX_REQUEST_SIZE,
            DEFAULT_MAX_REQUEST_SIZE
        ));
        assert!(needs_incr(
            DEFAULT_MAX_REQUEST_SIZE + 1,
            DEFAULT_MAX_REQUEST_SIZE
        ));
    }

    #[test]
    fn the_owners_slices_cover_exactly_the_payload() {
        let payload = 300 * 1024;
        let mut owner = Owner::start(payload, 256 * 1024).expect("started");
        let mut covered = 0;
        let mut slices = Vec::new();
        loop {
            let slice = owner.next_slice();
            if slice.is_empty() {
                break;
            }
            assert!(slice.end - slice.start <= 256 * 1024, "chunk too big");
            covered += slice.end - slice.start;
            slices.push(slice);
        }
        assert_eq!(covered, payload);
        assert_eq!(slices.len(), 2, "300 KiB in 256 KiB chunks: two writes");
        assert!(owner.is_finished());
    }

    #[test]
    fn an_incr_transfer_must_not_be_empty() {
        assert_eq!(Owner::start(0, 1024).err(), Some(IncrError::Empty));
        assert_eq!(Requestor::start(0).err(), Some(IncrError::Empty));
    }

    #[test]
    fn the_requestor_accepts_chunks_and_a_zero_length_end_marker() {
        let mut requestor = Requestor::start(300 * 1024).expect("started");
        assert_eq!(requestor.expected(), 300 * 1024);
        requestor.accept(256 * 1024).expect("first chunk");
        assert!(!requestor.is_finished());
        assert_eq!(requestor.expected(), 44 * 1024);
        requestor.accept(44 * 1024).expect("second chunk");
        assert!(requestor.is_finished());
        assert!(requestor.is_complete());
        // The explicit end marker after the full payload is part of the protocol.
        requestor.accept(0).expect("end marker");
        assert!(requestor.is_complete());
        // Anything after the end is refused.
        assert_eq!(requestor.accept(1).err(), Some(IncrError::AfterEnd));
    }

    #[test]
    fn an_owner_that_sends_more_than_it_announced_is_refused() {
        let mut requestor = Requestor::start(1024).expect("started");
        assert_eq!(requestor.accept(2048).err(), Some(IncrError::Overrun));
        // A truncated transfer ends with the marker but is not complete.
        requestor.accept(512).expect("partial");
        requestor.accept(0).expect("end marker");
        assert!(requestor.is_finished());
        assert!(
            !requestor.is_complete(),
            "512 of 1024 bytes is not complete"
        );
    }

    #[test]
    fn a_long_transfer_round_trips_byte_for_byte() {
        // The machine's two halves drive one another, which is what the adapter does with
        // X properties.
        let payload: Vec<u8> = (0..(300 * 1024)).map(|byte| (byte % 251) as u8).collect();
        let mut owner = Owner::start(payload.len(), 64 * 1024).expect("owner");
        let mut requestor = Requestor::start(payload.len()).expect("requestor");
        let mut collected = Vec::with_capacity(payload.len());
        loop {
            let slice = owner.next_slice();
            let end = slice.is_empty();
            collected.extend_from_slice(&payload[slice.clone()]);
            requestor
                .accept(slice.end - slice.start)
                .expect("the requestor takes the chunk");
            if end {
                break;
            }
        }
        assert_eq!(collected, payload);
        assert!(requestor.is_complete());
        assert!(owner.is_finished());
    }

    #[test]
    fn the_kde_hint_marks_a_secret_only_with_the_documented_value() {
        assert!(kde_hint_is_secret(b"secret"));
        assert!(kde_hint_is_secret(b"SECRET\0"));
        assert!(!kde_hint_is_secret(b" secret "), "the value is exact");
        assert!(!kde_hint_is_secret(b""));
        assert!(!kde_hint_is_secret(b"normal"));
        // Not UTF-8: never a secret, and never a panic.
        assert!(!kde_hint_is_secret(&[0xff, 0xfe]));
    }

    #[test]
    fn the_advertised_targets_are_the_required_ones_in_order() {
        // TIMESTAMP and TARGETS are ICCCM-required; the two text targets are what a paste
        // asks for. The adapter interns exactly these names.
        assert_eq!(
            SELECTION_TARGET_NAMES,
            ["TIMESTAMP", "TARGETS", "UTF8_STRING", "TEXT"]
        );
    }

    #[test]
    fn the_incr_chunk_size_is_the_requestors_maximum_and_never_zero() {
        assert_eq!(
            incr_chunk_size(DEFAULT_MAX_REQUEST_SIZE),
            DEFAULT_MAX_REQUEST_SIZE
        );
        assert_eq!(
            incr_chunk_size(0),
            1,
            "a zero-byte chunk would never make progress"
        );
    }

    #[test]
    fn a_payload_decodes_as_text_only_when_it_is_utf8() {
        assert_eq!(
            decode_text_payload("héllo".as_bytes()),
            Some("héllo".to_owned())
        );
        assert_eq!(decode_text_payload(b""), Some(String::new()));
        assert_eq!(
            decode_text_payload(&[0xff, 0xfe, 0x00]),
            None,
            "binary is not text"
        );
    }
}
