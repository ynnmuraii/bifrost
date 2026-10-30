//! Opt-in SMTC discovery and transport control on a dedicated WinRT MTA worker.
//!
//! Tokens identify live COM sessions, never applications. Change events are bounded
//! invalidation hints: after overflow, obtain a fresh snapshot. Metadata stays in
//! memory and is UTF-8 byte bounded; this module never logs it or injects keys.

use crate::Error;
use crate::{backend::map_error, backend_media::Pending};
use audio_backend::BackendError;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    core::{IUnknown, Interface, HSTRING},
    Foundation::TypedEventHandler,
    Media::Control::*,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

type Session = GlobalSystemMediaTransportControlsSession;
type Manager = GlobalSystemMediaTransportControlsSessionManager;

/// Opaque, process-local identity; tokens are never reused during a manager lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionToken(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaChange {
    Sessions,
    Current,
    Metadata(SessionToken),
    Playback(SessionToken),
    Timeline(SessionToken),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    pub play: bool,
    pub pause: bool,
    pub toggle: bool,
    pub next: bool,
    pub previous: bool,
    pub stop: bool,
    pub record: bool,
    pub fast_forward: bool,
    pub rewind: bool,
    pub channel_up: bool,
    pub channel_down: bool,
    pub shuffle: bool,
    pub repeat: bool,
    pub playback_rate: bool,
    pub seek: bool,
}

/// SMTC times are signed 100 ns units, not wall-clock milliseconds.
#[derive(Clone, Debug)]
pub struct Timeline {
    pub start: i64,
    pub end: i64,
    pub position: i64,
    pub min_seek: i64,
    pub max_seek: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct MediaSession {
    pub token: SessionToken,
    pub revision: u64,
    pub app_id: String,
    pub title: String,
    pub artist: String,
    /// Native SMTC playback status, including unknown future values.
    pub playback_status: i32,
    pub timeline: Timeline,
    pub controls: Controls,
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSession")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct MediaSnapshot {
    pub sessions: Vec<MediaSession>,
    pub current: Option<SessionToken>,
}

enum Request {
    Snapshot(SyncSender<Result<MediaSnapshot, Error>>),
    Command(SessionToken, MediaCommand, SyncSender<Result<bool, Error>>),
    BackendCommand(Arc<Pending>),
}

/// Dropping or shutting down joins the worker and revokes all subscriptions.
pub struct MediaManager {
    requests: SyncSender<Request>,
    events: Receiver<MediaChange>,
    overflow: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    pending: Arc<Mutex<Vec<Weak<Pending>>>>,
}

impl MediaManager {
    pub fn open() -> Result<Self, Error> {
        let (requests, rx) = mpsc::sync_channel(16);
        let (events_tx, events) = mpsc::sync_channel(64);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let overflow = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let pending = Arc::new(Mutex::new(Vec::new()));
        let worker_pending = pending.clone();
        let sink = EventSink {
            tx: events_tx,
            overflow: overflow.clone(),
        };
        let worker = thread::spawn(move || {
            let initialized = State::open(sink, worker_pending);
            match initialized {
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
                Ok(mut state) => {
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    while !stopped.load(Ordering::Acquire) {
                        match rx.recv_timeout(Duration::from_millis(50)) {
                            Ok(Request::Snapshot(reply)) => {
                                let _ = reply.send(state.snapshot());
                            }
                            Ok(Request::Command(token, command, reply)) => {
                                let _ = reply.send(state.command(token, command));
                            }
                            Ok(Request::BackendCommand(pending)) => {
                                let result = state.backend_command(&pending);
                                pending.finish(result);
                            }
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                if state.dirty.swap(false, Ordering::AcqRel)
                                    && state.reconcile().is_err()
                                {
                                    state.dirty.store(true, Ordering::Release);
                                    state.sink.overflow.store(true, Ordering::Release);
                                }
                            }
                        }
                    }
                }
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                requests,
                events,
                overflow,
                stop,
                worker: Some(worker),
                pending,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err(Error::WorkerStopped)
            }
        }
    }

    pub(crate) fn snapshot_async(
        &self,
    ) -> Result<Receiver<Result<MediaSnapshot, Error>>, BackendError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.requests.try_send(Request::Snapshot(tx)).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => BackendError::Busy,
            mpsc::TrySendError::Disconnected(_) => BackendError::ServerUnavailable,
        })?;
        Ok(rx)
    }

    pub fn snapshot(&self) -> Result<MediaSnapshot, Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.requests
            .send(Request::Snapshot(tx))
            .map_err(|_| Error::WorkerStopped)?;
        rx.recv().map_err(|_| Error::WorkerStopped)?
    }

    /// Returns false for a removed token, disabled control, or a rejected command.
    pub fn command(&self, token: SessionToken, command: MediaCommand) -> Result<bool, Error> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.requests
            .send(Request::Command(token, command, tx))
            .map_err(|_| Error::WorkerStopped)?;
        rx.recv().map_err(|_| Error::WorkerStopped)?
    }

    pub(crate) fn admit(&self, pending: Arc<Pending>) -> Result<(), BackendError> {
        let mut registered = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        registered.retain(|job| job.upgrade().is_some_and(|job| job.outcome().is_none()));
        self.requests
            .try_send(Request::BackendCommand(pending.clone()))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => BackendError::Busy,
                mpsc::TrySendError::Disconnected(_) => BackendError::ServerUnavailable,
            })?;
        registered.push(Arc::downgrade(&pending));
        Ok(())
    }

    pub(crate) fn worker_stopped(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished())
    }

    pub fn events(&self) -> &Receiver<MediaChange> {
        &self.events
    }
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::AcqRel)
    }
    pub fn shutdown(mut self) {
        self.close();
    }
    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for MediaManager {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Clone)]
struct EventSink {
    tx: SyncSender<MediaChange>,
    overflow: Arc<AtomicBool>,
}
impl EventSink {
    fn emit(&self, event: MediaChange) {
        if self.tx.try_send(event).is_err() {
            self.overflow.store(true, Ordering::Release);
        }
    }
}

struct WinRt;
impl WinRt {
    fn open() -> Result<Self, Error> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }
}
impl Drop for WinRt {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

// SMTC providers are external processes: never wait indefinitely for async completion.
macro_rules! complete {
    ($operation:expr) => {{
        let operation = $operation?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while operation.Status()?.0 == 0 {
            if Instant::now() >= deadline {
                let _ = operation.Cancel();
                return Err(Error::Unsupported("SMTC operation timed out"));
            }
            thread::sleep(Duration::from_millis(5));
        }
        operation.GetResults()?
    }};
}

struct Entry {
    identity: IUnknown,
    session: Session,
    token: SessionToken,
    revision: Arc<AtomicU64>,
    metadata: Option<i64>,
    playback: Option<i64>,
    timeline: Option<i64>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(token) = self.metadata {
            let _ = self.session.RemoveMediaPropertiesChanged(token);
        }
        if let Some(token) = self.playback {
            let _ = self.session.RemovePlaybackInfoChanged(token);
        }
        if let Some(token) = self.timeline {
            let _ = self.session.RemoveTimelinePropertiesChanged(token);
        }
    }
}

struct State {
    manager: Manager,
    entries: Vec<Entry>,
    next: u64,
    sessions_changed: Option<i64>,
    current_changed: Option<i64>,
    sink: EventSink,
    dirty: Arc<AtomicBool>,
    pending: Arc<Mutex<Vec<Weak<Pending>>>>,
    // Last field: all COM objects/subscriptions are released before apartment teardown.
    _apartment: WinRt,
}
impl State {
    fn open(sink: EventSink, pending: Arc<Mutex<Vec<Weak<Pending>>>>) -> Result<Self, Error> {
        let apartment = WinRt::open()?;
        let manager = complete!(Manager::RequestAsync());
        let mut state = Self {
            manager,
            entries: Vec::new(),
            next: 1,
            sessions_changed: None,
            current_changed: None,
            sink,
            dirty: Arc::new(AtomicBool::new(false)),
            pending,
            _apartment: apartment,
        };
        let sink = state.sink.clone();
        let dirty = state.dirty.clone();
        state.sessions_changed = Some(state.manager.SessionsChanged(&TypedEventHandler::new(
            move |_, _| {
                dirty.store(true, Ordering::Release);
                sink.emit(MediaChange::Sessions);
                Ok(())
            },
        ))?);
        let sink = state.sink.clone();
        state.current_changed = Some(state.manager.CurrentSessionChanged(
            &TypedEventHandler::new(move |_, _| {
                sink.emit(MediaChange::Current);
                Ok(())
            }),
        )?);
        state.reconcile()?;
        Ok(state)
    }

    fn reconcile(&mut self) -> Result<(), Error> {
        let sessions = self.manager.GetSessions()?;
        let mut live = Vec::new();
        for session in sessions {
            let identity: IUnknown = session.cast()?;
            if !self.entries.iter().any(|entry| entry.identity == identity) {
                let token = SessionToken(self.next);
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or(Error::Unsupported("SMTC token space exhausted"))?;
                let mut entry = Entry {
                    identity: identity.clone(),
                    session,
                    token,
                    revision: Arc::new(AtomicU64::new(1)),
                    metadata: None,
                    playback: None,
                    timeline: None,
                };
                let sink = self.sink.clone();
                let revision = entry.revision.clone();
                entry.metadata = Some(entry.session.MediaPropertiesChanged(
                    &TypedEventHandler::new(move |_, _| {
                        advance_revision(&revision);
                        sink.emit(MediaChange::Metadata(token));
                        Ok(())
                    }),
                )?);
                let sink = self.sink.clone();
                let revision = entry.revision.clone();
                entry.playback = Some(entry.session.PlaybackInfoChanged(
                    &TypedEventHandler::new(move |_, _| {
                        advance_revision(&revision);
                        sink.emit(MediaChange::Playback(token));
                        Ok(())
                    }),
                )?);
                let sink = self.sink.clone();
                let revision = entry.revision.clone();
                entry.timeline = Some(entry.session.TimelinePropertiesChanged(
                    &TypedEventHandler::new(move |_, _| {
                        advance_revision(&revision);
                        sink.emit(MediaChange::Timeline(token));
                        Ok(())
                    }),
                )?);
                self.entries.push(entry);
            }
            live.push(identity);
        }
        self.entries.retain(|entry| live.contains(&entry.identity));
        // Invalidate queued jobs as soon as disappearance is observed, not only on dispatch.
        let registered = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for pending in registered.iter().filter_map(Weak::upgrade) {
            if !self
                .entries
                .iter()
                .any(|entry| entry.token.0 == pending.request.session.0)
            {
                pending.finish(Err(BackendError::DeviceGone));
            }
        }
        Ok(())
    }

    fn snapshot(&mut self) -> Result<MediaSnapshot, Error> {
        self.reconcile()?;
        // The WinRT method legitimately returns S_OK + null for an empty desktop.
        // The generated non-null wrapper turns that into an Error with S_OK.
        let mut pointer = std::ptr::null_mut();
        unsafe {
            (Interface::vtable(&self.manager).GetCurrentSession)(
                Interface::as_raw(&self.manager),
                &mut pointer,
            )
            .ok()?;
        }
        let current = if pointer.is_null() {
            None
        } else {
            let session = unsafe { Session::from_raw(pointer) };
            let identity: IUnknown = session.cast()?;
            self.entries
                .iter()
                .find(|entry| entry.identity == identity)
                .map(|entry| entry.token)
        };
        let mut sessions = Vec::with_capacity(self.entries.len());
        for entry in &self.entries {
            // Capture before reads: a concurrent native change invalidates this snapshot's revision.
            let revision = entry.revision.load(Ordering::Acquire);
            let metadata = complete!(entry.session.TryGetMediaPropertiesAsync());
            let playback = entry.session.GetPlaybackInfo()?;
            let timeline = entry.session.GetTimelineProperties()?;
            sessions.push(MediaSession {
                token: entry.token,
                revision,
                app_id: bounded_utf8(&entry.session.SourceAppUserModelId()?, 128),
                title: bounded_utf8(&metadata.Title()?, 256),
                artist: bounded_utf8(&metadata.Artist()?, 256),
                playback_status: playback.PlaybackStatus()?.0,
                controls: controls(&entry.session)?,
                timeline: Timeline {
                    start: timeline.StartTime()?.Duration,
                    end: timeline.EndTime()?.Duration,
                    position: timeline.Position()?.Duration,
                    min_seek: timeline.MinSeekTime()?.Duration,
                    max_seek: timeline.MaxSeekTime()?.Duration,
                    updated_at: timeline.LastUpdatedTime()?.UniversalTime,
                },
            });
        }
        Ok(MediaSnapshot { sessions, current })
    }

    fn command(&mut self, token: SessionToken, command: MediaCommand) -> Result<bool, Error> {
        self.reconcile()?;
        let Some(entry) = self.entries.iter().find(|entry| entry.token == token) else {
            return Ok(false);
        };
        let session = &entry.session;
        let controls = controls(session)?;
        let enabled = match command {
            MediaCommand::Play => controls.play,
            MediaCommand::Pause => controls.pause,
            MediaCommand::Toggle => controls.toggle,
            MediaCommand::Next => controls.next,
            MediaCommand::Previous => controls.previous,
        };
        if !enabled {
            return Ok(false);
        }
        Ok(match command {
            MediaCommand::Play => complete!(session.TryPlayAsync()),
            MediaCommand::Pause => complete!(session.TryPauseAsync()),
            MediaCommand::Toggle => complete!(session.TryTogglePlayPauseAsync()),
            MediaCommand::Next => complete!(session.TrySkipNextAsync()),
            MediaCommand::Previous => complete!(session.TrySkipPreviousAsync()),
        })
    }

    fn backend_command(&mut self, pending: &Pending) -> Result<(), BackendError> {
        if let Some(result) = pending.outcome() {
            return result;
        }
        self.reconcile().map_err(map_error)?;
        let token = SessionToken(pending.request.session.0);
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.token == token)
            .ok_or(BackendError::DeviceGone)?;
        if !revision_matches(&entry.revision, pending.request.expected_revision) {
            return Err(BackendError::StaleRevision);
        }
        let session = entry.session.clone();
        let revision = entry.revision.clone();
        let available = controls(&session).map_err(map_error)?;
        let enabled = match pending.request.command {
            audio_backend::MediaCommand::Play => available.play,
            audio_backend::MediaCommand::Pause => available.pause,
            audio_backend::MediaCommand::Toggle => available.toggle,
            audio_backend::MediaCommand::Next => available.next,
            audio_backend::MediaCommand::Previous => available.previous,
        };
        if !enabled {
            return Err(BackendError::Unsupported);
        }
        let operation = match pending.start(|| {
            if !revision_matches(&revision, pending.request.expected_revision) {
                return Err(BackendError::StaleRevision);
            }
            match pending.request.command {
                audio_backend::MediaCommand::Play => session.TryPlayAsync(),
                audio_backend::MediaCommand::Pause => session.TryPauseAsync(),
                audio_backend::MediaCommand::Toggle => session.TryTogglePlayPauseAsync(),
                audio_backend::MediaCommand::Next => session.TrySkipNextAsync(),
                audio_backend::MediaCommand::Previous => session.TrySkipPreviousAsync(),
            }
            .map_err(|error| map_error(Error::Windows(error)))
        }) {
            Some(operation) => operation?,
            None => {
                return pending
                    .outcome()
                    .unwrap_or(Err(BackendError::ServerUnavailable))
            }
        };
        // Cancellation/timeout cannot prove whether an already dispatched OS command took effect.
        let result = loop {
            if let Some(result) = pending.outcome() {
                break result;
            }
            if let Err(error) = self.reconcile() {
                break Err(map_error(error));
            }
            if !self.entries.iter().any(|entry| entry.token == token) {
                break Err(BackendError::DeviceGone);
            }
            match operation.Status() {
                Ok(status) if status.0 == 0 => thread::sleep(Duration::from_millis(5)),
                Ok(_) => {
                    break operation
                        .GetResults()
                        .map_err(|error| map_error(Error::Windows(error)))
                        .and_then(|accepted| {
                            if accepted {
                                Ok(())
                            } else {
                                Err(BackendError::Unsupported)
                            }
                        })
                }
                Err(error) => break Err(map_error(Error::Windows(error))),
            }
        };
        if result.is_err() {
            let _ = operation.Cancel();
        }
        result
    }
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(token) = self.sessions_changed {
            let _ = self.manager.RemoveSessionsChanged(token);
        }
        if let Some(token) = self.current_changed {
            let _ = self.manager.RemoveCurrentSessionChanged(token);
        }
        self.entries.clear();
    }
}

fn controls(session: &Session) -> Result<Controls, Error> {
    let c = session.GetPlaybackInfo()?.Controls()?;
    Ok(Controls {
        play: c.IsPlayEnabled()?,
        pause: c.IsPauseEnabled()?,
        toggle: c.IsPlayPauseToggleEnabled()?,
        next: c.IsNextEnabled()?,
        previous: c.IsPreviousEnabled()?,
        stop: c.IsStopEnabled()?,
        record: c.IsRecordEnabled()?,
        fast_forward: c.IsFastForwardEnabled()?,
        rewind: c.IsRewindEnabled()?,
        channel_up: c.IsChannelUpEnabled()?,
        channel_down: c.IsChannelDownEnabled()?,
        shuffle: c.IsShuffleEnabled()?,
        repeat: c.IsRepeatEnabled()?,
        playback_rate: c.IsPlaybackRateEnabled()?,
        seek: c.IsPlaybackPositionEnabled()?,
    })
}

fn bounded_utf8(value: &HSTRING, max: usize) -> String {
    let mut result = String::with_capacity(max.min(value.len()));
    for ch in char::decode_utf16(value.iter().copied()) {
        let ch = ch.unwrap_or(char::REPLACEMENT_CHARACTER);
        if result.len() + ch.len_utf8() > max {
            break;
        }
        result.push(ch);
    }
    result
}

fn advance_revision(revision: &AtomicU64) {
    // Saturation would let a changed session reuse its revision; exhaustion is fail-closed.
    let _ = revision.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        value.checked_add(1)
    });
}

fn revision_matches(revision: &AtomicU64, expected: u64) -> bool {
    let actual = revision.load(Ordering::Acquire);
    actual != u64::MAX && actual == expected
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changes_invalidate_revision_and_exhaustion_fails_closed() {
        let revision = AtomicU64::new(1);
        assert!(revision_matches(&revision, 1));
        advance_revision(&revision);
        assert!(!revision_matches(&revision, 1));
        assert!(revision_matches(&revision, 2));
        revision.store(u64::MAX - 1, Ordering::Release);
        advance_revision(&revision);
        advance_revision(&revision);
        assert!(!revision_matches(&revision, u64::MAX));
    }

    #[test]
    fn metadata_debug_is_redacted() {
        let session = MediaSession {
            token: SessionToken(1),
            revision: 1,
            app_id: "private-app".into(),
            title: "private-title".into(),
            artist: "private-artist".into(),
            playback_status: 0,
            timeline: Timeline {
                start: 0,
                end: 0,
                position: 0,
                min_seek: 0,
                max_seek: 0,
                updated_at: 0,
            },
            controls: Controls::default(),
        };
        let debug = format!(
            "{:?}",
            MediaSnapshot {
                sessions: vec![session],
                current: None
            }
        );
        assert!(!debug.contains("private-"));
    }

    #[test]
    fn utf8_bounds_preserve_codepoints() {
        assert_eq!(bounded_utf8(&HSTRING::from("ab🎵é"), 5), "ab");
        assert_eq!(bounded_utf8(&HSTRING::from("ab🎵é"), 6), "ab🎵");
        assert_eq!(
            bounded_utf8(&HSTRING::from("é".repeat(200)), 256).len(),
            256
        );
        assert_eq!(bounded_utf8(&HSTRING::from("x"), 0), "");
        assert_eq!(bounded_utf8(&HSTRING::from_wide(&[0xd800, 0x61]), 3), "�");
    }
}
