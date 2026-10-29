//! The Wayland clipboard through `ext-data-control-v1` (the compositor's own clipboard
//! manager interface).
//!
//! ADR-008 lists two routes on Wayland: `org.freedesktop.portal.Clipboard` attached to an
//! existing portal session, or `ext-data-control-v1` «там, где композитор его реализует
//! (KWin)» (`docs/04-decisions.md`). This module implements the second one, which is the one
//! a KVM needs:
//!
//! * a portal clipboard on an `InputCapture` session is granted **only while the session is
//!   `Active`** (`docs/work/wayland-facts.md` §3) — i.e. only while the *other* machine has
//!   the pointer, the opposite of "copy locally, then move over and paste";
//! * `ext-data-control-v1` is the privileged-client route: the compositor hands us the
//!   selection and accepts our own, with no dialog, no session, and no focus requirement.
//!   KWin implements it (MR !6606, Plasma 6.2+), and it is verified on the test KWin
//!   6.7.5: the global is advertised *and* `wl-paste --list-types` answers from a background
//!   client;
//! * Mutter implements neither `ext-` nor `wlr-data-control`, so on GNOME the portal stays
//!   the only route. That decision is [`crate::clipboard::availability`]; this build still
//!   does not drive the portal stream, and it does not pretend to (a route announced as
//!   available while unimplemented would be a lie, ADR-008).
//!
//! [`Clipboard`] is a handle to one background thread that owns the Wayland connection, the
//! way [`crate::overlay::EdgeOverlay`] does for the effect strip: `connect` returns only once
//! the global is bound (`None` when the compositor has none), and everything after that is a
//! command or a change over a channel. The thread never blocks on a transfer: reading a
//! selection and serving one both go to short-lived helper threads, because both pipes can
//! stall indefinitely (the peer may never read; a source may never answer).
//!
//! # Echo suppression (KVM-041)
//!
//! A selection this adapter *sets* comes back to it as a selection event: a data-control
//! device is told about every selection, its own included. An owner has no sequence number to
//! compare, so the mechanism is the content itself: the payload last applied is shared
//! between the source and `applied`, and one equal reading is swallowed. Comparison runs
//! on this dedicated adapter thread, never the agent session loop.

use std::sync::Arc;

use clipboard::{ClipboardContent, ContentKind, Marker};

/// Preferred text MIME type.
pub const TEXT_MIME: &str = clipboard::MIME;
/// Native PNG bytes are carried unchanged.
pub const IMAGE_MIME: &str = clipboard::IMAGE_MIME;

/// The other text MIME types a selection may offer, in the order they are preferred after
/// [`TEXT_MIME`].
///
/// `UTF8_STRING` is the X11-flavoured spelling a bridged application offers; `text/plain` is
/// the polymorphic fallback, used only when nothing better is offered. Decoding still
/// requires valid UTF-8, so a Latin-1 `text/plain` payload is refused rather than mangled.
pub const TEXT_FALLBACK_MIMES: [&str; 2] = ["UTF8_STRING", "text/plain"];

/// `x-kde-passwordManagerHint`: the MIME type KDE's password managers add to a copy they made.
pub const KDE_PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// The hint value that means "this is a secret".
pub const KDE_SECRET: &str = "secret";

/// One content item the local selection now holds.
///
/// Nothing here is logged: sizes and marker names only, never the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The content the selection offered.
    pub content: ClipboardContent,
    /// Every secret marker it carried.
    pub markers: Vec<Marker>,
}

/// The MIME type to ask a selection for: [`TEXT_MIME`], else the best fallback.
///
/// `None`: this selection offers no supported text MIME; PNG is chosen separately.
pub fn text_mime_of(mimes: &[String]) -> Option<&str> {
    let exact = |mime: &str| mimes.iter().find(|offered| offered.as_str() == mime);
    let ci = |mime: &str| {
        mimes
            .iter()
            .find(|offered| offered.eq_ignore_ascii_case(mime))
    };
    exact(TEXT_MIME)
        .or_else(|| ci(TEXT_MIME))
        .or_else(|| {
            TEXT_FALLBACK_MIMES
                .iter()
                .find_map(|mime| exact(mime).or_else(|| ci(mime)))
        })
        .map(String::as_str)
}

/// Text wins over PNG; unsupported selections are ignored.
pub fn content_mime_of(mimes: &[String]) -> Option<(&str, ContentKind)> {
    text_mime_of(mimes)
        .map(|mime| (mime, ContentKind::Text))
        .or_else(|| {
            mimes
                .iter()
                .find(|mime| mime.eq_ignore_ascii_case(IMAGE_MIME))
                .map(|mime| (mime.as_str(), ContentKind::Png))
        })
}

/// Whether a selection offers the KDE secret hint at all.
pub fn offers_kde_hint(mimes: &[String]) -> bool {
    mimes
        .iter()
        .any(|mime| mime.eq_ignore_ascii_case(KDE_PASSWORD_HINT))
}

/// Classifies the KDE hint's *value*, as read from the selection.
///
/// Pure: the adapter reads the value, this decides. The rule is the one
/// `platform_x11::clipboard::kde_hint_is_secret` applies to the same hint over X11, so a copy
/// is classified the same way whichever Linux backend this node runs.
pub fn kde_hint_is_secret(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .map(|text| text.trim_matches('\0').eq_ignore_ascii_case(KDE_SECRET))
        .unwrap_or(false)
}

/// The secret marker a KDE hint yields.
///
/// `value` is `None` when the hint was offered but its value could not be read (the source
/// never answered the transfer). That case is still marked: an unreadable hint is the one
/// place where precision and safety disagree, and a password leaving the machine is the worse
/// outcome (`ADR-008`: «Контент, помеченный менеджером паролей, по умолчанию не передаётся»).
pub fn kde_hint_marker(offered: bool, value: Option<&[u8]>) -> Option<Marker> {
    if !offered {
        return None;
    }
    match value {
        Some(bytes) => kde_hint_is_secret(bytes).then_some(Marker::KdeSecret),
        None => Some(Marker::KdeSecret),
    }
}

/// Why a payload was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    /// The source sent more than the limit: the payload is abandoned unread.
    TooLarge,
    /// The source did not answer within the transfer budget.
    Timeout,
    /// The pipe could not be read.
    Io,
}

/// Whether one reading is the echo of `applied`, and whether that echo was the last one.
///
/// Cleared after one sighting, like the X11 adapter: a *later* copy of the same content by the
/// user is then reported, which is the safe direction (a stale echo would instead swallow a
/// real copy).
pub fn swallow_echo(
    applied: &mut Option<Arc<ClipboardContent>>,
    content: &ClipboardContent,
) -> bool {
    if applied.as_deref() != Some(content) {
        return false;
    }
    *applied = None;
    true
}

#[cfg(target_os = "linux")]
mod glue {
    //! The `ext-data-control-v1` client: one thread, one connection, two helper threads.

    use std::future::Future;
    use std::io::{Read, Write};
    use std::os::fd::{AsFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    use rustix::io::Errno;
    use tokio::sync::oneshot;
    use wayland_client::backend::{ReadEventsGuard, WaylandError};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_callback::{self, WlCallback};
    use wayland_client::protocol::wl_registry::{self, WlRegistry};
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols::ext::data_control::v1::client::{
        ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
        ext_data_control_manager_v1::ExtDataControlManagerV1,
        ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
        ext_data_control_source_v1::{self, ExtDataControlSourceV1},
    };

    use super::{
        content_mime_of, kde_hint_marker, offers_kde_hint, swallow_echo, Change, ClipboardContent,
        ContentKind, PayloadError, KDE_PASSWORD_HINT,
    };

    /// How long [`Clipboard::connect`] waits for the global to be bound.
    const READY_TIMEOUT: Duration = Duration::from_secs(2);
    /// How long the loop blocks on the compositor before checking for a command again.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    /// Leaves room for the input hold deadline without blocking the agent runtime thread.
    const APPLY_TIMEOUT: Duration = Duration::from_millis(120);
    /// How long one payload read waits for the source to answer.
    const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long the (tiny) hint value waits after the payload.
    const HINT_TIMEOUT: Duration = Duration::from_millis(250);
    /// Largest hint value this adapter reads: the value is a word, not a document.
    const HINT_BYTES: u64 = 64;

    /// One thing the caller asked the thread to do.
    enum Command {
        /// Own the selection with this content (`None`: give the selection up).
        Set {
            content: Option<ClipboardContent>,
            applied: oneshot::Sender<Result<(), String>>,
        },
        Quit,
    }

    type ServedRead = (u64, Result<Change, clipboard::ClipboardError>);

    /// Handle to the clipboard thread.
    ///
    /// Dropping it stops the thread, which also gives the selection back to the compositor.
    pub struct Clipboard {
        commands: Sender<Command>,
        changes: Receiver<Result<Change, clipboard::ClipboardError>>,
        disconnected: bool,
    }

    impl Clipboard {
        /// Connects and binds `ext_data_control_manager_v1`.
        ///
        /// `None` when there is no compositor connection, or the compositor does not implement
        /// the protocol (GNOME): this node then has no clipboard and says so in `AgentReady`
        /// rather than announcing one it cannot honour (ADR-008).
        #[must_use]
        pub fn connect() -> Option<Self> {
            let (ready_tx, ready_rx) = mpsc::sync_channel::<bool>(1);
            let (command_tx, command_rx) = mpsc::channel();
            let (change_tx, change_rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("clipboard-data-control".into())
                .spawn(move || run(&command_rx, &change_tx, ready_tx));
            if let Err(error) = spawned {
                tracing::info!(%error, "no clipboard: cannot spawn the data-control thread");
                return None;
            }
            match ready_rx.recv_timeout(READY_TIMEOUT) {
                Ok(true) => Some(Self {
                    commands: command_tx,
                    changes: change_rx,
                    disconnected: false,
                }),
                Ok(false) => None,
                Err(_) => {
                    tracing::info!(
                        "no clipboard: the compositor did not answer within {READY_TIMEOUT:?}"
                    );
                    None
                }
            }
        }

        /// The next change the *user* made, if the selection informed us of one.
        ///
        /// Never blocks and never waits for the platform: the session polls this from its own
        /// tick, and its watchdog and deadline live on that tick too.
        pub fn poll_change(&mut self) -> Result<Option<Change>, clipboard::ClipboardError> {
            if self.disconnected {
                return Ok(None);
            }
            match self.changes.try_recv() {
                Ok(change) => change.map(Some),
                Err(TryRecvError::Empty) => Ok(None),
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    Err(clipboard::ClipboardError::Invalid)
                }
            }
        }

        /// Owns the selection, or gives it up with `None`.
        ///
        /// Success requires the compositor's display-sync reply after the selection request,
        /// not merely enqueueing a command on the worker thread. Owned bytes move to that
        /// thread without copying or comparing large payloads on the session loop.
        pub fn begin_set_content(
            &self,
            content: Option<ClipboardContent>,
        ) -> impl Future<Output = Result<(), String>> + Send + 'static {
            let (applied, confirmation) = oneshot::channel();
            let sent = self.commands.send(Command::Set { content, applied });
            async move {
                sent.map_err(|_| "the data-control clipboard thread is gone".to_owned())?;
                match tokio::time::timeout(APPLY_TIMEOUT, confirmation).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err(
                        "the data-control clipboard stopped before confirming selection".to_owned(),
                    ),
                    Err(_) => Err(
                        "the compositor did not confirm the clipboard selection in time".to_owned(),
                    ),
                }
            }
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            let _ = self.commands.send(Command::Quit);
        }
    }

    /// Thread body: connect, bind, report readiness, then serve until told to quit.
    fn run(
        commands: &Receiver<Command>,
        changes: &Sender<Result<Change, clipboard::ClipboardError>>,
        ready: SyncSender<bool>,
    ) {
        let mut app = match App::connect() {
            Ok(app) => app,
            Err(error) => {
                tracing::info!(%error, "no clipboard: ext_data_control_manager_v1 is unusable");
                let _ = ready.send(false);
                return;
            }
        };
        let _ = ready.send(true);
        drop(ready);
        if let Err(error) = app.serve(commands, changes) {
            tracing::warn!(%error, "the data-control clipboard stopped");
            let _ = changes.send(Err(clipboard::ClipboardError::Invalid));
        }
    }

    fn to_timespec(duration: Duration) -> Timespec {
        Timespec {
            tv_sec: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(duration.subsec_nanos()),
        }
    }

    /// One offer the device announced, with the MIME types it carries.
    struct Offer {
        offer: ExtDataControlOfferV1,
        mimes: Vec<String>,
        /// The payload of this offer was already asked for.
        requested: bool,
    }

    /// Bound globals plus what the event handlers report back to the loop.
    struct State {
        /// Kept alive: it is the seat every `get_data_device` is made for.
        _seat: WlSeat,
        offers: Vec<Offer>,
        selection: Option<ExtDataControlOfferV1>,
        selection_generation: u64,
        /// The source this adapter owns, so a later `Set` can replace (and destroy) it.
        owned: Option<ExtDataControlSourceV1>,
        /// Echo comparison stays on the dedicated adapter thread, never the session loop.
        applied: Option<Arc<ClipboardContent>>,
        /// The device is gone (`finished`): the adapter stops serving.
        finished: bool,
        /// Replies wait for display-sync callbacks ordered after each selection request.
        pending_applies: Vec<(WlCallback, oneshot::Sender<Result<(), String>>)>,
        /// Serves paste requests without holding up Wayland dispatch.
        writer: TransferWriter,
    }

    impl State {
        fn offer_mut(&mut self, object: &ExtDataControlOfferV1) -> Option<&mut Offer> {
            let id = object.id();
            self.offers.iter_mut().find(|offer| offer.offer.id() == id)
        }

        fn fail_applies(&mut self, detail: &str) {
            for (_, applied) in self.pending_applies.drain(..) {
                let _ = applied.send(Err(detail.to_owned()));
            }
        }
    }

    delegate_noop!(State: ExtDataControlManagerV1);
    delegate_noop!(State: ignore WlSeat);

    impl Dispatch<WlCallback, ()> for State {
        fn event(
            state: &mut Self,
            callback: &WlCallback,
            event: wl_callback::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let wl_callback::Event::Done { .. } = event {
                if let Some(index) = state
                    .pending_applies
                    .iter()
                    .position(|(pending, _)| pending == callback)
                {
                    let (_, applied) = state.pending_applies.swap_remove(index);
                    let result = if state.finished {
                        Err(
                            "the data-control device stopped before confirming selection"
                                .to_owned(),
                        )
                    } else {
                        Ok(())
                    };
                    let _ = applied.send(result);
                }
            }
        }
    }

    impl Dispatch<WlRegistry, GlobalListContents> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtDataControlOfferV1, ()> for State {
        fn event(
            state: &mut Self,
            offer: &ExtDataControlOfferV1,
            event: ext_data_control_offer_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            // `data_offer` precedes these and `selection` follows them, so by the time the loop
            // looks at the selection its MIME list is complete.
            if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
                if let Some(entry) = state.offer_mut(offer) {
                    entry.mimes.push(mime_type);
                }
            }
        }
    }

    impl Dispatch<ExtDataControlSourceV1, Arc<ClipboardContent>> for State {
        fn event(
            state: &mut Self,
            source: &ExtDataControlSourceV1,
            event: ext_data_control_source_v1::Event,
            content: &Arc<ClipboardContent>,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                ext_data_control_source_v1::Event::Send { fd, mime_type } => {
                    if state
                        .owned
                        .as_ref()
                        .is_some_and(|owned| owned.id() == source.id())
                        && mime_type == content.kind().mime()
                    {
                        state.writer.send(fd, content);
                    }
                }
                ext_data_control_source_v1::Event::Cancelled
                    if state
                        .owned
                        .as_ref()
                        .is_some_and(|owned| owned.id() == source.id()) =>
                {
                    state.writer.cancel();
                    if let Some(replaced) = state.owned.take() {
                        replaced.destroy();
                    }
                }
                _ => {}
            }
        }
    }

    impl Dispatch<ExtDataControlDeviceV1, ()> for State {
        fn event(
            state: &mut Self,
            _: &ExtDataControlDeviceV1,
            event: ext_data_control_device_v1::Event,
            (): &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            match event {
                ext_data_control_device_v1::Event::DataOffer { id } => state.offers.push(Offer {
                    offer: id,
                    mimes: Vec::new(),
                    requested: false,
                }),
                ext_data_control_device_v1::Event::Selection { id } => {
                    state.selection_generation = state.selection_generation.wrapping_add(1);
                    // The previous selection's offer is dead the moment a new one arrives.
                    if let Some(previous) = state.selection.take() {
                        if let Some(index) = state
                            .offers
                            .iter()
                            .position(|offer| offer.offer.id() == previous.id())
                        {
                            let stale = state.offers.remove(index);
                            stale.offer.destroy();
                        }
                    }
                    state.selection = id;
                }
                ext_data_control_device_v1::Event::Finished => {
                    if let Some(selection) = state.selection.take() {
                        selection.destroy();
                    }
                    state.finished = true;
                }
                ext_data_control_device_v1::Event::PrimarySelection { id: Some(id) } => {
                    // PRIMARY is deliberately not carried (`docs/03-architecture.md`:
                    // «Clipboard: только CLIPBOARD, без PRIMARY»), so the offer is released
                    // here rather than leaked.
                    id.destroy();
                }
                _ => {}
            }
        }

        // `data_offer` carries a `new_id`: without this specialization wayland-client panics
        // the first time a selection is announced ("Missing event_created_child
        // specialization"). The offer's user data is the unit this adapter's `Dispatch` for
        // `ExtDataControlOfferV1` expects; the MIME types are read out of `State` by id.
        wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
            ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
        ]);
    }

    struct App {
        connection: Connection,
        queue: EventQueue<State>,
        qh: QueueHandle<State>,
        state: State,
        manager: ExtDataControlManagerV1,
        device: ExtDataControlDeviceV1,
        /// Readings from the helper threads, and the end they are given.
        served_tx: Sender<ServedRead>,
        served_rx: Receiver<ServedRead>,
    }

    impl App {
        fn connect() -> Result<Self, Box<dyn std::error::Error>> {
            let connection = Connection::connect_to_env()?;
            let (globals, queue) = registry_queue_init::<State>(&connection)?;
            let qh = queue.handle();
            let manager = globals
                .bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ())
                .map_err(|error| format!("ext_data_control_manager_v1 unavailable: {error}"))?;
            let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=1, ())?;
            let device = manager.get_data_device(&seat, &qh, ());
            let (served_tx, served_rx) = mpsc::channel();
            Ok(Self {
                connection,
                queue,
                qh,
                state: State {
                    _seat: seat,
                    offers: Vec::new(),
                    selection: None,
                    selection_generation: 0,
                    owned: None,
                    applied: None,
                    finished: false,
                    pending_applies: Vec::new(),
                    writer: TransferWriter::new()?,
                },
                manager,
                device,
                served_tx,
                served_rx,
            })
        }

        /// Serves until the caller quits, the device finishes, or the connection dies.
        fn serve(
            &mut self,
            commands: &Receiver<Command>,
            changes: &Sender<Result<Change, clipboard::ClipboardError>>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            let result = self.serve_loop(commands, changes);
            let detail = match &result {
                Ok(()) => {
                    "the data-control clipboard stopped before confirming selection".to_owned()
                }
                Err(error) => format!("the data-control clipboard connection failed: {error}"),
            };
            self.state.fail_applies(&detail);
            result
        }

        fn serve_loop(
            &mut self,
            commands: &Receiver<Command>,
            changes: &Sender<Result<Change, clipboard::ClipboardError>>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            loop {
                self.state
                    .pending_applies
                    .retain(|(_, applied)| !applied.is_closed());
                if !self.apply_commands(commands) || self.state.finished {
                    self.release();
                    return Ok(());
                }
                self.queue.dispatch_pending(&mut self.state)?;
                self.forward_served(changes);
                self.request_pending();
                self.queue.flush()?;
                let Some(guard) = self.queue.prepare_read() else {
                    continue;
                };
                if !wait(guard) {
                    self.release();
                    return Ok(());
                }
            }
        }

        /// Hands the helper threads' readings to the caller, applying the echo rule.
        fn forward_served(&mut self, changes: &Sender<Result<Change, clipboard::ClipboardError>>) {
            forward_served(
                &self.served_rx,
                self.state.selection_generation,
                &mut self.state.applied,
                changes,
            );
        }

        /// Returns `false` when the thread must stop.
        fn apply_commands(&mut self, commands: &Receiver<Command>) -> bool {
            loop {
                match commands.try_recv() {
                    Ok(Command::Set { content, applied }) => {
                        if applied.is_closed() {
                            continue;
                        }
                        if content.as_ref().is_some_and(|content| {
                            content.len() as u64 > content.kind().hard_max_bytes()
                                || (content.kind() == ContentKind::Png
                                    && !content.as_bytes().starts_with(b"\x89PNG\r\n\x1a\n"))
                        }) {
                            let _ = applied.send(Err("invalid clipboard payload".to_owned()));
                            continue;
                        }
                        self.set_selection(content);
                        let callback = self.connection.display().sync(&self.qh, ());
                        self.state.pending_applies.push((callback, applied));
                    }
                    Ok(Command::Quit) | Err(TryRecvError::Disconnected) => return false,
                    Err(TryRecvError::Empty) => return true,
                }
            }
        }

        /// Takes the selection for `content`, or gives it up.
        fn set_selection(&mut self, content: Option<ClipboardContent>) {
            self.state.writer.cancel();
            // A source is single-use: `set_selection` with a source that was already given
            // once is the `used_source` protocol error, so every `Set` creates a new one and
            // the previous one is destroyed here (`Cancelled` may have taken it already).
            if let Some(previous) = self.state.owned.take() {
                previous.destroy();
            }
            match content {
                Some(content) => {
                    let content = Arc::new(content);
                    let source = self
                        .manager
                        .create_data_source(&self.qh, Arc::clone(&content));
                    source.offer(content.kind().mime().to_owned());
                    self.device.set_selection(Some(&source));
                    self.state.owned = Some(source);
                    self.state.applied = Some(content);
                }
                None => {
                    self.device.set_selection(None);
                    self.state.applied = None;
                }
            }
        }

        /// Gives the selection back and drops the objects this adapter created.
        fn release(&mut self) {
            self.state.writer.cancel();
            if let Some(source) = self.state.owned.take() {
                source.destroy();
            }
            self.device.set_selection(None);
            self.device.destroy();
            self.manager.destroy();
        }

        /// Asks for the payload of a selection the loop has not requested yet.
        fn request_pending(&mut self) {
            let Some(selection) = self.state.selection.clone() else {
                return;
            };
            let Some(offer) = self
                .state
                .offers
                .iter_mut()
                .find(|offer| offer.offer.id() == selection.id())
            else {
                return;
            };
            if offer.requested {
                return;
            }
            let Some((mime, kind)) =
                content_mime_of(&offer.mimes).map(|(mime, kind)| (mime.to_owned(), kind))
            else {
                return;
            };
            let hint = offers_kde_hint(&offer.mimes);
            offer.requested = true;
            let offer = offer.offer.clone();
            let served = self.served_tx.clone();
            request(
                &offer,
                &mime,
                kind,
                hint,
                self.state.selection_generation,
                served,
            );
        }
    }

    fn forward_served(
        served: &Receiver<ServedRead>,
        current_generation: u64,
        applied: &mut Option<Arc<ClipboardContent>>,
        changes: &Sender<Result<Change, clipboard::ClipboardError>>,
    ) {
        while let Ok((generation, result)) = served.try_recv() {
            if generation != current_generation {
                continue;
            }
            if let Ok(change) = &result {
                if change.markers.is_empty() && swallow_echo(applied, &change.content) {
                    continue;
                }
            }
            if changes.send(result).is_err() {
                return;
            }
        }
    }

    /// Issues bounded reads off the event loop. The secret hint is consumed before any
    /// payload bytes, so secret PNG selections are dropped without reading their image.
    fn request(
        offer: &ExtDataControlOfferV1,
        mime: &str,
        kind: ContentKind,
        hint: bool,
        generation: u64,
        served: Sender<ServedRead>,
    ) {
        let hint_read = if hint {
            match rustix::pipe::pipe() {
                Ok((read, write)) => {
                    offer.receive(KDE_PASSWORD_HINT.to_owned(), write.as_fd());
                    drop(write);
                    Some(read)
                }
                Err(error) => {
                    tracing::warn!(%error, "no clipboard: cannot create the hint pipe");
                    None
                }
            }
        } else {
            None
        };
        let offer = offer.clone();
        let mime = mime.to_owned();
        let failed = served.clone();
        let spawned = std::thread::Builder::new()
            .name("clipboard-read".into())
            .spawn(move || {
                let value = hint_read
                    .as_ref()
                    .and_then(|read| read_bounded(read, HINT_BYTES, HINT_TIMEOUT).ok());
                let markers: Vec<_> = kde_hint_marker(hint, value.as_deref())
                    .into_iter()
                    .collect();
                let result = read_content(
                    || {
                        let (read, write) = rustix::pipe::pipe().ok()?;
                        offer.receive(mime, write.as_fd());
                        drop(write);
                        Some(read)
                    },
                    kind,
                    &markers,
                    TRANSFER_TIMEOUT,
                )
                .map(|content| Change { content, markers });
                let _ = served.send((generation, result));
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "no clipboard: cannot spawn the transfer thread");
            let _ = failed.send((generation, Err(clipboard::ClipboardError::Invalid)));
        }
    }

    /// PNG secret checks precede even its receive request; report marker-only invalidation.
    fn read_content(
        receive: impl FnOnce() -> Option<OwnedFd>,
        kind: ContentKind,
        markers: &[clipboard::Marker],
        timeout: Duration,
    ) -> Result<ClipboardContent, clipboard::ClipboardError> {
        if kind == ContentKind::Png && !markers.is_empty() {
            return Ok(ClipboardContent::Text(String::new()));
        }
        let read = receive().ok_or(clipboard::ClipboardError::Invalid)?;
        let limit = match kind {
            ContentKind::Text => 2 * clipboard::MAX_BYTES,
            ContentKind::Png => clipboard::MAX_IMAGE_BYTES,
        };
        let bytes = read_bounded(&read, limit, timeout).map_err(|error| match error {
            PayloadError::TooLarge => clipboard::ClipboardError::TooLarge,
            PayloadError::Timeout | PayloadError::Io => clipboard::ClipboardError::Invalid,
        })?;
        match kind {
            ContentKind::Text => String::from_utf8(bytes)
                .map(ClipboardContent::Text)
                .map_err(|_| clipboard::ClipboardError::Invalid),
            ContentKind::Png => ClipboardContent::from_bytes(kind, bytes),
        }
    }

    /// Reads up to `limit` bytes, giving up when the source has not closed the pipe in time.
    fn read_bounded(
        read: &OwnedFd,
        limit: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, PayloadError> {
        let deadline = Instant::now() + timeout;
        let mut file = std::fs::File::from(read.try_clone().map_err(|_| PayloadError::Io)?);
        let mut out = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PayloadError::Timeout);
            }
            let mut fds = [PollFd::new(&file, PollFlags::IN)];
            match poll(&mut fds, Some(&to_timespec(remaining))) {
                Ok(0) => return Err(PayloadError::Timeout),
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => return Err(PayloadError::Io),
            }
            match file.read(&mut chunk) {
                Ok(0) => return Ok(out),
                Ok(count) => {
                    if out.len() as u64 + count as u64 > limit {
                        return Err(PayloadError::TooLarge);
                    }
                    out.extend_from_slice(&chunk[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return Err(PayloadError::Io),
            }
        }
    }

    struct Transfer {
        fd: OwnedFd,
        content: Arc<ClipboardContent>,
        generation: u64,
    }

    /// One bounded, cancellable paste worker, joined when the Wayland state is dropped.
    struct TransferWriter {
        requests: SyncSender<Transfer>,
        stopping: Arc<AtomicBool>,
        generation: Arc<AtomicU64>,
        thread: Option<JoinHandle<()>>,
    }

    impl TransferWriter {
        fn new() -> std::io::Result<Self> {
            let (requests, receiver) = mpsc::sync_channel::<Transfer>(4);
            let stopping = Arc::new(AtomicBool::new(false));
            let generation = Arc::new(AtomicU64::new(0));
            let stop = Arc::clone(&stopping);
            let current = Arc::clone(&generation);
            let thread = std::thread::Builder::new()
                .name("clipboard-paste".into())
                .spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match receiver.recv_timeout(POLL_INTERVAL) {
                            Ok(transfer) => serve(
                                transfer.fd,
                                transfer.content.as_bytes(),
                                &stop,
                                &current,
                                transfer.generation,
                                Instant::now() + TRANSFER_TIMEOUT,
                            ),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })?;
            Ok(Self {
                requests,
                stopping,
                generation,
                thread: Some(thread),
            })
        }

        fn send(&self, fd: OwnedFd, content: &Arc<ClipboardContent>) {
            let transfer = Transfer {
                fd,
                content: Arc::clone(content),
                generation: self.generation.load(Ordering::SeqCst),
            };
            // Closing an excess request's fd refuses that paste without blocking dispatch.
            let _ = self.requests.try_send(transfer);
        }

        fn cancel(&self) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for TransferWriter {
        fn drop(&mut self) {
            self.stopping.store(true, Ordering::SeqCst);
            self.cancel();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve(
        fd: OwnedFd,
        bytes: &[u8],
        stopping: &AtomicBool,
        current: &AtomicU64,
        generation: u64,
        deadline: Instant,
    ) {
        let flags = rustix::fs::fcntl_getfl(&fd)
            .and_then(|flags| rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK));
        if flags.is_err() {
            return;
        }
        let mut file = std::fs::File::from(fd);
        let mut offset = 0;
        while offset < bytes.len() {
            if stopping.load(Ordering::SeqCst)
                || current.load(Ordering::SeqCst) != generation
                || Instant::now() >= deadline
            {
                return;
            }
            match file.write(&bytes[offset..]) {
                Ok(0) => break,
                Ok(count) => offset += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let mut fds = [PollFd::new(&file, PollFlags::OUT)];
                    match poll(&mut fds, Some(&to_timespec(remaining.min(POLL_INTERVAL)))) {
                        Ok(_) | Err(Errno::INTR) => {}
                        Err(_) => return,
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "the selection transfer was cut short");
                    break;
                }
            }
        }
    }

    /// Blocks on the compositor socket for at most [`POLL_INTERVAL`].
    ///
    /// Returns `false` when the connection is gone.
    fn wait(guard: ReadEventsGuard) -> bool {
        let fd = guard.connection_fd();
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let timeout = to_timespec(POLL_INTERVAL);
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(Errno::INTR) => return true,
            Err(_) => return false,
        }
        if !fds[0]
            .revents()
            .intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR)
        {
            return true;
        }
        match guard.read() {
            Ok(_) => true,
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }

    #[cfg(test)]
    mod apply_tests {
        use super::*;

        fn observed_pipe(
            bytes: Vec<u8>,
            kind: ContentKind,
        ) -> Result<ClipboardContent, clipboard::ClipboardError> {
            let (read, write) = rustix::pipe::pipe().unwrap();
            let writer = std::thread::spawn(move || {
                let _ = std::fs::File::from(write).write_all(&bytes);
            });
            let result = read_content(|| Some(read), kind, &[], Duration::from_secs(5));
            writer.join().unwrap();
            result
        }

        fn observed_clipboard() -> (Clipboard, Sender<Result<Change, clipboard::ClipboardError>>) {
            let (commands, _) = mpsc::channel();
            let (changes, receiver) = mpsc::channel();
            (
                Clipboard {
                    commands,
                    changes: receiver,
                    disconnected: false,
                },
                changes,
            )
        }

        #[test]
        fn disconnected_actor_reports_one_terminal_invalidation_only() {
            let (mut clipboard, changes) = observed_clipboard();
            drop(changes);
            assert_eq!(
                clipboard.poll_change(),
                Err(clipboard::ClipboardError::Invalid)
            );
            for _ in 0..100 {
                assert_eq!(clipboard.poll_change(), Ok(None));
            }
        }

        #[test]
        fn changed_selection_errors_survive_worker_forwarding_and_poll() {
            let (mut clipboard, changes) = observed_clipboard();
            let (worker, served) = mpsc::channel();
            let mut applied = None;
            let first = Change {
                content: ClipboardContent::Text("A".into()),
                markers: vec![],
            };
            worker.send((1, Ok(first.clone()))).unwrap();
            forward_served(&served, 1, &mut applied, &changes);
            assert_eq!(clipboard.poll_change(), Ok(Some(first)));
            for (result, error) in [
                (
                    observed_pipe(b"invalid PNG".to_vec(), ContentKind::Png),
                    clipboard::ClipboardError::Invalid,
                ),
                (
                    observed_pipe(vec![0xff], ContentKind::Text),
                    clipboard::ClipboardError::Invalid,
                ),
                (
                    read_content(|| None, ContentKind::Png, &[], Duration::from_secs(1)),
                    clipboard::ClipboardError::Invalid,
                ),
                (
                    {
                        let mut bytes = vec![0; clipboard::MAX_IMAGE_BYTES as usize + 1];
                        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
                        observed_pipe(bytes, ContentKind::Png)
                    },
                    clipboard::ClipboardError::TooLarge,
                ),
            ] {
                assert_eq!(result, Err(error));
                worker.send((2, Err(error))).unwrap();
                forward_served(&served, 2, &mut applied, &changes);
                assert_eq!(clipboard.poll_change(), Err(error));
                assert_eq!(clipboard.poll_change(), Ok(None));
            }
        }

        #[test]
        fn large_text_copy_is_reported_instead_of_silently_preserving_previous_carry() {
            let text = vec![b'x'; 1536 * 1024];
            let content = observed_pipe(text.clone(), ContentKind::Text).unwrap();
            let (mut clipboard, changes) = observed_clipboard();
            changes
                .send(Ok(Change {
                    content,
                    markers: vec![],
                }))
                .unwrap();
            assert_eq!(
                clipboard.poll_change().unwrap().unwrap().content.as_bytes(),
                text
            );
        }

        #[test]
        fn stale_read_results_cannot_replace_latest_generation_or_drop_current_error() {
            let (mut clipboard, changes) = observed_clipboard();
            let (worker, served) = mpsc::channel();
            worker
                .send((1, Err(clipboard::ClipboardError::TooLarge)))
                .unwrap();
            worker
                .send((2, Err(clipboard::ClipboardError::Invalid)))
                .unwrap();
            worker
                .send((
                    1,
                    Ok(Change {
                        content: ClipboardContent::Text("stale".into()),
                        markers: vec![],
                    }),
                ))
                .unwrap();
            forward_served(&served, 2, &mut None, &changes);
            assert_eq!(
                clipboard.poll_change(),
                Err(clipboard::ClipboardError::Invalid)
            );
            assert_eq!(clipboard.poll_change(), Ok(None));
        }

        #[test]
        fn stalled_paste_does_not_block_dispatch_or_writer_shutdown() {
            let (read, write) = rustix::pipe::pipe().expect("paste pipe");
            rustix::pipe::fcntl_setpipe_size(&write, 4096).expect("small pipe");
            rustix::io::write(&write, &[0u8; 4096]).expect("fill paste pipe");
            let (dispatch, dispatched) = mpsc::channel();
            let (shutdown, stopped) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let writer = TransferWriter::new().expect("paste worker");
                writer.send(
                    write,
                    &Arc::new(ClipboardContent::Text("x".repeat(128 * 1024))),
                );
                dispatch.send(()).unwrap();
                drop(writer);
                shutdown.send(()).unwrap();
            });
            let responsive = dispatched.recv_timeout(Duration::from_secs(1)).is_ok();
            let cancelled = responsive && stopped.recv_timeout(Duration::from_secs(1)).is_ok();
            // Release even the broken synchronous writer before asserting: no hung test thread.
            drop(read);
            worker.join().expect("dispatch thread");
            assert!(
                responsive,
                "a stalled paste must not block the next dispatch"
            );
            assert!(
                cancelled,
                "shutdown must cancel and join the transfer worker"
            );
        }

        #[test]
        fn stalled_paste_has_a_total_deadline() {
            let (_read, write) = rustix::pipe::pipe().unwrap();
            rustix::pipe::fcntl_setpipe_size(&write, 4096).unwrap();
            rustix::io::write(&write, &[0u8; 4096]).unwrap();
            let started = Instant::now();
            serve(
                write,
                b"pending",
                &AtomicBool::new(false),
                &AtomicU64::new(0),
                0,
                started + Duration::from_millis(30),
            );
            assert!(started.elapsed() < Duration::from_millis(500));
        }

        #[test]
        fn cancelled_paste_does_not_block_a_fresh_transfer() {
            let writer = TransferWriter::new().unwrap();
            let (_read, write) = rustix::pipe::pipe().unwrap();
            rustix::pipe::fcntl_setpipe_size(&write, 4096).unwrap();
            rustix::io::write(&write, &[0u8; 4096]).unwrap();
            writer.send(write, &Arc::new(ClipboardContent::Text("stalled".into())));
            writer.cancel();
            let (read, write) = rustix::pipe::pipe().unwrap();
            writer.send(write, &Arc::new(ClipboardContent::Text("fresh".into())));
            let mut fds = [PollFd::new(&read, PollFlags::IN)];
            assert!(poll(&mut fds, Some(&to_timespec(Duration::from_secs(1)))).unwrap() > 0);
            let mut bytes = [0u8; 5];
            assert_eq!(rustix::io::read(&read, &mut bytes).unwrap(), 5);
            assert_eq!(&bytes, b"fresh");
        }

        #[test]
        fn png_source_serves_the_original_bytes() {
            let png = b"\x89PNG\r\n\x1a\nnative bytes\xff\0".to_vec();
            let content = Arc::new(ClipboardContent::Image { png: png.clone() });
            assert_eq!(content.kind().mime(), "image/png");
            let writer = TransferWriter::new().unwrap();
            let (read, write) = rustix::pipe::pipe().unwrap();
            writer.send(write, &content);
            assert_eq!(
                read_bounded(&read, clipboard::MAX_IMAGE_BYTES, Duration::from_secs(1)),
                Ok(png)
            );
        }

        #[test]
        fn secret_png_is_not_requested_and_reports_marker_only_invalidation() {
            for value in [Some(b"secret".as_slice()), None] {
                let markers: Vec<_> = kde_hint_marker(true, value).into_iter().collect();
                let (read, write) = rustix::pipe::pipe().unwrap();
                let png = b"\x89PNG\r\n\x1a\nsecret";
                rustix::io::write(&write, png).unwrap();
                drop(write);
                let mut requested = false;
                assert_eq!(
                    read_content(
                        || {
                            requested = true;
                            None
                        },
                        ContentKind::Png,
                        &markers,
                        Duration::from_secs(1),
                    ),
                    Ok(ClipboardContent::Text(String::new()))
                );
                assert!(!requested, "secret PNG receive must not be requested");
                assert_eq!(
                    read_bounded(&read, 64, Duration::from_secs(1)),
                    Ok(png.to_vec()),
                    "secret check must leave all PNG bytes unread"
                );
            }
        }

        #[test]
        fn png_pipe_checks_signature_and_preserves_native_bytes() {
            for bytes in [
                b"\x89PNG\r\n\x1a\noriginal\xff".as_slice(),
                b"not a PNG".as_slice(),
            ] {
                let (read, write) = rustix::pipe::pipe().unwrap();
                rustix::io::write(&write, bytes).unwrap();
                drop(write);
                let content =
                    read_content(|| Some(read), ContentKind::Png, &[], Duration::from_secs(1));
                if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    assert_eq!(content.unwrap().as_bytes(), bytes);
                } else {
                    assert_eq!(content, Err(clipboard::ClipboardError::Invalid));
                }
            }
        }

        #[test]
        fn text_keeps_secret_markers_and_requires_utf8() {
            for bytes in ["привет".as_bytes(), b"\xff".as_slice(), b"".as_slice()] {
                let (read, write) = rustix::pipe::pipe().unwrap();
                rustix::io::write(&write, bytes).unwrap();
                drop(write);
                let content = read_content(
                    || Some(read),
                    ContentKind::Text,
                    &[clipboard::Marker::KdeSecret],
                    Duration::from_secs(1),
                );
                assert_eq!(
                    content,
                    std::str::from_utf8(bytes)
                        .map(|text| ClipboardContent::Text(text.to_owned()))
                        .map_err(|_| clipboard::ClipboardError::Invalid)
                );
            }
        }

        #[test]
        fn png_pipe_enforces_the_32_mib_cap() {
            assert_eq!(ContentKind::Png.hard_max_bytes(), 32 * 1024 * 1024);
            for extra in [0, 1] {
                let (read, write) = rustix::pipe::pipe().unwrap();
                let total = clipboard::MAX_IMAGE_BYTES as usize + extra;
                let source = std::thread::spawn(move || {
                    let mut file = std::fs::File::from(write);
                    file.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
                    let chunk = [0u8; 8192];
                    let mut left = total - 8;
                    while left > 0 {
                        let count = left.min(chunk.len());
                        if file.write_all(&chunk[..count]).is_err() {
                            break;
                        }
                        left -= count;
                    }
                });
                let result = read_bounded(
                    &read,
                    ContentKind::Png.hard_max_bytes(),
                    Duration::from_secs(5),
                );
                drop(read);
                source.join().unwrap();
                if extra == 0 {
                    let content = ClipboardContent::from_bytes(ContentKind::Png, result.unwrap())
                        .expect("PNG at hard cap");
                    assert_eq!(content.len(), total);
                } else {
                    assert_eq!(result, Err(PayloadError::TooLarge));
                }
            }
        }

        #[test]
        fn stalled_png_pipe_times_out() {
            let (read, _write) = rustix::pipe::pipe().unwrap();
            assert_eq!(
                read_bounded(
                    &read,
                    ContentKind::Png.hard_max_bytes(),
                    Duration::from_millis(10),
                ),
                Err(PayloadError::Timeout)
            );
        }

        #[tokio::test]
        async fn png_apply_moves_owned_bytes_and_waits_for_confirmation() {
            let (clipboard, worker) = fake_clipboard();
            let png = b"\x89PNG\r\n\x1a\nowned".to_vec();
            let pointer = png.as_ptr();
            let apply = clipboard.begin_set_content(Some(ClipboardContent::Image { png }));
            let Command::Set { content, applied } = worker.try_recv().unwrap() else {
                panic!("expected selection command");
            };
            assert_eq!(content.unwrap().as_bytes().as_ptr(), pointer);
            tokio::pin!(apply);
            assert!(futures_util::poll!(&mut apply).is_pending());
            applied.send(Ok(())).unwrap();
            assert_eq!(apply.await, Ok(()));
        }

        fn fake_clipboard() -> (Clipboard, Receiver<Command>) {
            let (commands, receiver) = mpsc::channel();
            let (_, changes) = mpsc::channel();
            (
                Clipboard {
                    commands,
                    changes,
                    disconnected: false,
                },
                receiver,
            )
        }

        #[tokio::test]
        async fn set_waits_for_worker_confirmation_not_command_enqueue() {
            let (clipboard, worker) = fake_clipboard();
            let apply = clipboard
                .begin_set_content(Some(ClipboardContent::Text("clipboard fixture".into())));
            tokio::pin!(apply);
            assert!(futures_util::poll!(&mut apply).is_pending());
            let Command::Set { content, applied } = worker.try_recv().expect("queued command")
            else {
                panic!("expected selection command");
            };
            assert_eq!(
                content,
                Some(ClipboardContent::Text("clipboard fixture".into()))
            );
            assert!(
                futures_util::poll!(&mut apply).is_pending(),
                "dequeue is not compositor confirmation"
            );
            applied.send(Ok(())).expect("waiting caller");
            assert_eq!(apply.await, Ok(()));
        }

        #[tokio::test]
        async fn set_propagates_worker_failure_and_disconnection() {
            for explicit_failure in [false, true] {
                let (clipboard, worker) = fake_clipboard();
                let apply = clipboard.begin_set_content(None);
                tokio::pin!(apply);
                assert!(futures_util::poll!(&mut apply).is_pending());
                let Command::Set { content, applied } = worker.try_recv().expect("queued command")
                else {
                    panic!("expected selection command");
                };
                assert!(content.is_none());
                if explicit_failure {
                    applied
                        .send(Err("display connection failed".to_owned()))
                        .expect("waiting caller");
                } else {
                    drop(applied);
                }
                assert!(apply.await.is_err());
            }
            let (clipboard, worker) = fake_clipboard();
            drop(worker);
            assert!(clipboard
                .begin_set_content(Some(ClipboardContent::Text("not queued".into())))
                .await
                .is_err());
        }

        #[tokio::test]
        async fn unconfirmed_selection_times_out_and_cannot_acknowledge_success_later() {
            let (clipboard, worker) = fake_clipboard();
            let apply = clipboard
                .begin_set_content(Some(ClipboardContent::Text("clipboard fixture".into())));
            tokio::pin!(apply);
            assert!(futures_util::poll!(&mut apply).is_pending());
            let Command::Set { applied, .. } = worker.try_recv().expect("queued command") else {
                panic!("expected selection command");
            };
            assert!(apply.await.is_err());
            assert!(applied.is_closed());
            assert!(applied.send(Ok(())).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
pub use glue::Clipboard;

#[cfg(test)]
mod tests {
    use super::*;

    fn mimes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_payload_mime_is_the_wire_mime() {
        // A test, not a re-export: the wire carries exactly one text type (`ADR-008`), and a
        // Wayland offer has to be asked for that same type.
        assert_eq!(TEXT_MIME, "text/plain;charset=utf-8");
        assert_eq!(TEXT_MIME, clipboard::MIME);
    }

    #[test]
    fn the_exact_mime_wins_over_every_fallback() {
        assert_eq!(
            text_mime_of(&mimes(&["text/plain", "UTF8_STRING", TEXT_MIME])),
            Some(TEXT_MIME)
        );
        assert_eq!(
            text_mime_of(&mimes(&["text/plain;charset=UTF-8"])),
            Some("text/plain;charset=UTF-8")
        );
        assert_eq!(
            text_mime_of(&mimes(&["text/html", "UTF8_STRING", "text/plain"])),
            Some("UTF8_STRING")
        );
        assert_eq!(text_mime_of(&mimes(&["TEXT"])), None);
        assert_eq!(text_mime_of(&mimes(&["text/html", "image/png"])), None);
        assert_eq!(text_mime_of(&[]), None);
    }

    #[test]
    fn text_wins_then_png_and_other_selections_are_ignored() {
        assert_eq!(
            content_mime_of(&mimes(&[IMAGE_MIME, "text/plain"])),
            Some(("text/plain", ContentKind::Text))
        );
        assert_eq!(
            content_mime_of(&mimes(&["text/html", IMAGE_MIME])),
            Some((IMAGE_MIME, ContentKind::Png))
        );
        assert_eq!(
            content_mime_of(&mimes(&["image/jpeg", "text/uri-list"])),
            None
        );
        assert_eq!(content_mime_of(&[]), None);
    }

    #[test]
    fn the_kde_hint_marks_a_secret_and_only_from_its_value() {
        assert!(!offers_kde_hint(&mimes(&[TEXT_MIME])));
        assert!(offers_kde_hint(&mimes(&[TEXT_MIME, KDE_PASSWORD_HINT])));
        assert!(kde_hint_is_secret(b"secret"));
        assert!(kde_hint_is_secret(b"SECRET"));
        assert!(kde_hint_is_secret(b"secret\0"));
        assert!(!kde_hint_is_secret(b"clipboard"));
        assert!(!kde_hint_is_secret(b""));
        assert!(!kde_hint_is_secret(&[0xff]));
        assert_eq!(kde_hint_marker(false, Some(b"secret")), None);
        assert_eq!(
            kde_hint_marker(true, Some(b"secret")),
            Some(Marker::KdeSecret)
        );
        assert_eq!(kde_hint_marker(true, Some(b"clipboard")), None);
        // The hint was offered but its value never arrived: the safe reading, not the silent one.
        assert_eq!(kde_hint_marker(true, None), Some(Marker::KdeSecret));
    }

    #[test]
    fn an_echo_is_swallowed_once_and_a_real_copy_never_is() {
        for content in [
            ClipboardContent::Text("copied on this machine".into()),
            ClipboardContent::Image {
                png: b"\x89PNG\r\n\x1a\nunchanged".to_vec(),
            },
        ] {
            let mut applied = Some(Arc::new(content.clone()));
            assert!(!swallow_echo(
                &mut applied,
                &ClipboardContent::Text("other".into())
            ));
            assert_eq!(applied.as_deref(), Some(&content));
            assert!(swallow_echo(&mut applied, &content));
            assert_eq!(applied, None);
            assert!(!swallow_echo(&mut applied, &content));
        }
    }
}
