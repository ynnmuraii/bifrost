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

    pub(crate) fn current_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.manager
            .snapshot()
            .map(|snapshot| snapshot.current.map(|token| MediaSessionToken(token.0)))
            .map_err(map_error)
    }

    pub(crate) fn native_current_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.manager
            .native_current()
            .map(|current| current.map(|token| MediaSessionToken(token.0)))
            .map_err(map_error)
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

fn ms_floor(ticks: i64) -> Option<u64> {
    u64::try_from(ticks).ok().map(|ticks| ticks / 10_000)
}

fn ms_ceil(ticks: i64) -> Option<u64> {
    u64::try_from(ticks)
        .ok()
        .and_then(|ticks| ticks.checked_add(9_999))
        .map(|ticks| ticks / 10_000)
}

fn convert(session: media::MediaSession) -> Result<MediaInfo, BackendError> {
    // Normalize the native timeline into the item-relative ms domain used by the contract.
    // Untrusted native extents use checked differences; no saturating arithmetic.
    let start = session.timeline.start;
    let end = session.timeline.end;
    let duration_ticks = end.checked_sub(start).filter(|duration| *duration > 0);
    let duration_ms = duration_ticks.and_then(ms_floor);
    let position_ticks = session.timeline.position.checked_sub(start);
    let valid_position = duration_ticks.is_some_and(|duration| {
        position_ticks.is_some_and(|position| position >= 0 && position <= duration)
    });
    let position_ms = if valid_position {
        position_ticks.and_then(ms_floor)
    } else {
        None
    };
    let min_ticks = session
        .timeline
        .min_seek
        .checked_sub(start)
        .filter(|min| *min >= 0);
    let max_ticks = session
        .timeline
        .max_seek
        .checked_sub(start)
        .filter(|max| *max >= 0)
        .filter(|max| duration_ticks.is_some_and(|duration| *max <= duration));
    // Lower bound rounds up, upper bound rounds down, so the advertised range never
    // contains a sub-millisecond point that the native range excludes.
    let min_ms = min_ticks.and_then(ms_ceil);
    let max_ms = max_ticks.and_then(ms_floor);
    let range = min_ms
        .zip(max_ms)
        .filter(|(min, max)| min <= max)
        .filter(|(_, max)| duration_ms.is_some_and(|duration| *max <= duration));
    let seekable = session.controls.seek
        && duration_ms.is_some_and(|duration| duration > 0)
        && position_ms.is_some()
        && range.is_some();
    let (seek_min_ms, seek_max_ms) = range.unzip();
    let mut info = MediaInfo::new(
        session.app_id,
        session.title,
        session.artist,
        MediaSessionToken(session.token.0),
        session.revision,
        match session.playback_status {
            4 => PlaybackState::Playing,
            5 => PlaybackState::Paused,
            3 => PlaybackState::Stopped,
            // Closed, opened, changing and unknown values do not assert stopped playback.
            _ => PlaybackState::Unknown,
        },
        MediaCommands {
            play: session.controls.play,
            pause: session.controls.pause,
            toggle: session.controls.toggle,
            next: session.controls.next,
            previous: session.controls.previous,
            seek: seekable,
        },
    )?;
    info.duration_ms = duration_ms;
    info.position_ms = position_ms;
    if seekable {
        info.seek_min_ms = seek_min_ms;
        info.seek_max_ms = seek_max_ms;
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

    fn timeline_session(
        start: i64,
        end: i64,
        position: i64,
        min_seek: i64,
        max_seek: i64,
        seek: bool,
    ) -> media::MediaSession {
        media::MediaSession {
            token: media::SessionToken(1),
            revision: 1,
            app_id: String::new(),
            title: String::new(),
            artist: String::new(),
            playback_status: 4,
            controls: media::Controls {
                seek,
                ..media::Controls::default()
            },
            timeline: media::Timeline {
                start,
                end,
                position,
                min_seek,
                max_seek,
                updated_at: 0,
            },
        }
    }

    #[test]
    fn seek_range_rounds_lower_up_and_upper_down() {
        // 100 ms origin, 1000 ms duration, native range [0.5 ms, 999.5 ms].
        let info = convert(timeline_session(
            1_000_000, 11_000_000, 1_000_000, 1_005_000, 10_995_000, true,
        ))
        .unwrap();
        assert_eq!(info.duration_ms, Some(1_000));
        assert_eq!(info.position_ms, Some(0));
        assert!(info.commands.seek);
        assert_eq!(info.seek_min_ms, Some(1));
        assert_eq!(info.seek_max_ms, Some(999));
    }

    #[test]
    fn empty_intersection_disables_seek_without_clamping_or_losing_metadata() {
        // Native min 1.5 ms (ceil 2) > native max 1.4 ms (floor 1): no advertised range.
        let info = convert(timeline_session(
            1_000_000, 11_000_000, 1_000_000, 1_015_000, 1_014_000, true,
        ))
        .unwrap();
        assert_eq!(info.duration_ms, Some(1_000));
        assert_eq!(info.position_ms, Some(0));
        assert!(!info.commands.seek);
        assert_eq!(info.seek_min_ms, None);
        assert_eq!(info.seek_max_ms, None);
    }

    #[test]
    fn invalid_native_position_is_not_floored_into_a_fake_zero() {
        let info = convert(timeline_session(
            1_000_000, 11_000_000, 11_000_001, 1_000_000, 11_000_000, true,
        ))
        .unwrap();
        assert_eq!(info.duration_ms, Some(1_000));
        assert_eq!(info.position_ms, None);
        assert!(!info.commands.seek);
    }

    #[test]
    fn untrusted_extent_overflow_disables_seek_without_panic() {
        let info = convert(timeline_session(
            i64::MIN,
            i64::MAX,
            0,
            i64::MIN,
            i64::MAX,
            true,
        ))
        .unwrap();
        assert_eq!(info.duration_ms, None);
        assert_eq!(info.position_ms, None);
        assert!(!info.commands.seek);
        assert_eq!(info.seek_min_ms, None);
        assert_eq!(info.seek_max_ms, None);
    }

    #[test]
    fn max_beyond_end_or_sub_millisecond_duration_disables_seek() {
        let beyond = convert(timeline_session(0, 10_000_000, 0, 0, 10_010_000, true)).unwrap();
        assert!(!beyond.commands.seek);

        let sub_ms = convert(timeline_session(0, 5_000, 0, 0, 5_000, true)).unwrap();
        assert_eq!(sub_ms.duration_ms, Some(0));
        assert!(!sub_ms.commands.seek);
    }

    #[test]
    fn playback_conversion_does_not_invent_a_stopped_state() {
        for (native, expected) in [
            (0, PlaybackState::Unknown),
            (1, PlaybackState::Unknown),
            (2, PlaybackState::Unknown),
            (3, PlaybackState::Stopped),
            (4, PlaybackState::Playing),
            (5, PlaybackState::Paused),
            (i32::MAX, PlaybackState::Unknown),
        ] {
            let session = media::MediaSession {
                token: media::SessionToken(1),
                revision: 1,
                app_id: String::new(),
                title: String::new(),
                artist: String::new(),
                playback_status: native,
                controls: media::Controls::default(),
                timeline: media::Timeline {
                    start: 0,
                    end: 0,
                    position: 0,
                    min_seek: 0,
                    max_seek: 0,
                    updated_at: 0,
                },
            };
            assert_eq!(convert(session).unwrap().state, expected);
        }
    }
}
