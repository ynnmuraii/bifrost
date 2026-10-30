//! Admission and completion storage for the existing WinRT worker (no second worker).
use crate::{backend::map_error, media};
use audio_backend::{
    BackendError, Event, MediaCommandRequest, MediaCommands, MediaInfo, MediaSessionToken,
    PlaybackState, EVENT_CAPACITY, MEDIA_COMMAND_TIMEOUT_MS,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    sync::{
        mpsc::{Receiver, TryRecvError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const CAPACITY: usize = 16;

pub(crate) struct Pending {
    pub(crate) request: MediaCommandRequest,
    deadline: Instant,
    result: Mutex<Option<Result<(), BackendError>>>,
}

impl Pending {
    fn new(request: MediaCommandRequest) -> Self {
        Self {
            request,
            deadline: Instant::now() + Duration::from_millis(MEDIA_COMMAND_TIMEOUT_MS.into()),
            result: Mutex::new(None),
        }
    }

    pub(crate) fn finish(&self, result: Result<(), BackendError>) {
        let mut stored = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if stored.is_none() {
            *stored = Some(if Instant::now() >= self.deadline {
                Err(BackendError::Timeout)
            } else {
                result
            });
        }
    }

    // Serialize dispatch with cancellation: after cancel returns, a queued job cannot start.
    pub(crate) fn start<T>(
        &self,
        dispatch: impl FnOnce() -> Result<T, BackendError>,
    ) -> Option<Result<T, BackendError>> {
        let mut stored = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if stored.is_none() && Instant::now() >= self.deadline {
            *stored = Some(Err(BackendError::Timeout));
        }
        if stored.is_some() {
            None
        } else {
            Some(dispatch())
        }
    }

    pub(crate) fn outcome(&self) -> Option<Result<(), BackendError>> {
        let mut stored = self.result.lock().unwrap_or_else(|e| e.into_inner());
        if stored.is_none() && Instant::now() >= self.deadline {
            *stored = Some(Err(BackendError::Timeout));
        }
        *stored
    }
}

struct SnapshotRequest {
    generation: u64,
    force: bool,
    response: Receiver<Result<media::MediaSnapshot, crate::Error>>,
}

pub(crate) struct BackendMedia {
    manager: media::MediaManager,
    pending: VecDeque<Arc<Pending>>,
    cache: RefCell<Vec<MediaInfo>>,
    cache_generation: Cell<u64>,
    snapshot: Option<SnapshotRequest>,
    events: VecDeque<Event>,
    dirty: bool,
    current_dirty: bool,
    stopped_reported: bool,
}

impl BackendMedia {
    pub(crate) fn open() -> Result<Self, BackendError> {
        Ok(Self {
            manager: media::MediaManager::open().map_err(map_error)?,
            pending: VecDeque::with_capacity(CAPACITY),
            cache: RefCell::new(Vec::new()),
            cache_generation: Cell::new(0),
            snapshot: None,
            events: VecDeque::with_capacity(EVENT_CAPACITY),
            dirty: false,
            current_dirty: false,
            stopped_reported: false,
        })
    }

    pub(crate) fn sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        let sessions = self
            .manager
            .snapshot()
            .map_err(map_error)?
            .sessions
            .into_iter()
            .map(convert)
            .collect::<Result<Vec<_>, _>>()?;
        *self.cache.borrow_mut() = sessions.clone();
        self.cache_generation
            .set(self.cache_generation.get().wrapping_add(1));
        Ok(sessions)
    }

    pub(crate) fn admit(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        if self.pending.len() == CAPACITY {
            return Err(BackendError::Busy);
        }
        let pending = Arc::new(Pending::new(request));
        self.manager.admit(pending.clone())?;
        self.pending.push_back(pending);
        Ok(())
    }

    pub(crate) fn cancel(&mut self) {
        for pending in &self.pending {
            pending.finish(Err(BackendError::Cancelled));
        }
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        if self.manager.worker_stopped() && !self.stopped_reported {
            self.stopped_reported = true;
            self.snapshot = None;
            for pending in &self.pending {
                pending.finish(Err(BackendError::ServerUnavailable));
            }
            self.enqueue(Event::ResyncRequired);
        }
        if let Some(index) = self.pending.iter().position(|p| p.outcome().is_some()) {
            let pending = self.pending.remove(index)?;
            return Some(Event::MediaCommandCompleted {
                request_id: pending.request.request_id,
                result: pending.outcome()?,
            });
        }
        if self.manager.take_overflow() {
            self.dirty = true;
            self.enqueue(Event::ResyncRequired);
        }
        for _ in 0..EVENT_CAPACITY {
            let Ok(change) = self.manager.events().try_recv() else {
                break;
            };
            self.dirty = true;
            if change == media::MediaChange::Current {
                self.current_dirty = true;
            }
        }
        let response = self
            .snapshot
            .as_ref()
            .map(|request| request.response.try_recv());
        match response {
            Some(Ok(result)) => {
                let SnapshotRequest {
                    generation, force, ..
                } = self.snapshot.take()?;
                if generation != self.cache_generation.get() {
                    // An explicit synchronous read superseded this in-flight snapshot.
                    self.dirty = true;
                    self.current_dirty |= force;
                } else {
                    match result.map_err(map_error).and_then(|snapshot| {
                        snapshot
                            .sessions
                            .into_iter()
                            .map(convert)
                            .collect::<Result<Vec<_>, _>>()
                    }) {
                        Ok(sessions) => self.apply_snapshot(sessions, force),
                        Err(_) => self.enqueue(Event::ResyncRequired),
                    }
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.snapshot = None;
                self.enqueue(Event::ResyncRequired);
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
        if self.dirty && self.snapshot.is_none() && !self.stopped_reported {
            match self.manager.snapshot_async() {
                Ok(rx) => {
                    self.snapshot = Some(SnapshotRequest {
                        generation: self.cache_generation.get(),
                        force: self.current_dirty,
                        response: rx,
                    });
                    self.dirty = false;
                    self.current_dirty = false;
                }
                Err(BackendError::Busy) => {}
                Err(_) => {
                    self.dirty = false;
                    self.enqueue(Event::ResyncRequired);
                }
            }
        }
        self.events.pop_front()
    }

    fn enqueue(&mut self, event: Event) {
        if self.events.len() == EVENT_CAPACITY {
            self.events.clear();
            self.events.push_back(Event::ResyncRequired);
        } else {
            self.events.push_back(event);
        }
    }

    fn apply_snapshot(&mut self, sessions: Vec<MediaInfo>, force: bool) {
        let old = self.cache.take();
        // Current-session identity has no shared representation. Re-emit unchanged infos on
        // current changes, without inventing a revision or exposing application identity.
        for info in &sessions {
            if force || !old.iter().any(|previous| previous == info) {
                self.enqueue(Event::MediaChanged {
                    session: info.session,
                    info: Some(info.clone()),
                });
            }
        }
        for previous in &old {
            if !sessions.iter().any(|info| info.session == previous.session) {
                self.enqueue(Event::MediaChanged {
                    session: previous.session,
                    info: None,
                });
            }
        }
        self.cache.replace(sessions);
    }
}

impl Drop for BackendMedia {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn convert(session: media::MediaSession) -> Result<MediaInfo, BackendError> {
    let mut info = MediaInfo::new(
        session.app_id,
        session.title,
        session.artist,
        MediaSessionToken(session.token.0),
        session.revision,
        match session.playback_status {
            4 => PlaybackState::Playing,
            5 => PlaybackState::Paused,
            // The shared contract has no Unknown: Closed, Opened, Changing and future values
            // cannot safely imply active playback.
            _ => PlaybackState::Stopped,
        },
        MediaCommands {
            play: session.controls.play,
            pause: session.controls.pause,
            toggle: session.controls.toggle,
            next: session.controls.next,
            previous: session.controls.previous,
        },
    )?;
    // A zero/negative extent does not establish that the provider exposes a timeline.
    if session.timeline.end > session.timeline.start {
        info.duration_ms =
            u64::try_from(session.timeline.end.saturating_sub(session.timeline.start) / 10_000)
                .ok();
        info.position_ms = u64::try_from(
            session
                .timeline
                .position
                .saturating_sub(session.timeline.start)
                / 10_000,
        )
        .ok();
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> MediaCommandRequest {
        MediaCommandRequest {
            request_id: 7,
            session: MediaSessionToken(1),
            expected_revision: 2,
            command: audio_backend::MediaCommand::Play,
        }
    }
    #[test]
    fn cancelled_queue_never_dispatches_or_completes_twice() {
        let pending = Pending::new(request());
        pending.finish(Err(BackendError::Cancelled));
        assert!(pending
            .start(|| -> Result<(), BackendError> { panic!("cancelled dispatch") })
            .is_none());
        pending.finish(Ok(()));
        assert_eq!(pending.outcome(), Some(Err(BackendError::Cancelled)));
    }
    #[test]
    fn deadline_includes_queue_waiting() {
        let mut pending = Pending::new(request());
        pending.deadline = Instant::now() - Duration::from_millis(1);
        assert!(pending
            .start(|| -> Result<(), BackendError> { panic!("expired dispatch") })
            .is_none());
        pending.finish(Ok(()));
        assert_eq!(pending.outcome(), Some(Err(BackendError::Timeout)));
    }
    #[test]
    fn completed_result_survives_cancel() {
        let pending = Pending::new(request());
        pending.finish(Ok(()));
        pending.finish(Err(BackendError::Cancelled));
        assert_eq!(pending.outcome(), Some(Ok(())));
    }
}
