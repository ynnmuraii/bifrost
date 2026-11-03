use audio_backend::{
    Backend, BackendError, Event, MediaCommand, MediaCommandRequest, MediaInfo, MediaSessionToken,
    PlaybackState, MEDIA_COMMAND_TIMEOUT_MS,
};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 16;
const MAX_SESSIONS: usize = 32;
const CHANGE_INTERVAL: Duration = Duration::from_millis(100);
const POSITION_INTERVAL: Duration = Duration::from_secs(1);
const COMMAND_TIMEOUT: Duration = Duration::from_millis(MEDIA_COMMAND_TIMEOUT_MS as u64);

struct Pending {
    native_id: u64,
    request_id: u64,
    deadline: Instant,
}

pub(super) struct Controls {
    enabled: bool,
    dirty: bool,
    sessions: Vec<MediaInfo>,
    current: Option<MediaSessionToken>,
    last_artwork: Option<(MediaSessionToken, [u8; 32])>,
    revision: u64,
    last_refresh: Option<Instant>,
    next_native_id: u64,
    pending: Vec<Pending>,
}

impl Controls {
    pub(super) fn new() -> Self {
        Self {
            enabled: false,
            dirty: false,
            sessions: Vec::new(),
            current: None,
            last_artwork: None,
            revision: 0,
            last_refresh: None,
            next_native_id: 0,
            pending: Vec::with_capacity(MAX_PENDING),
        }
    }

    pub(super) fn handles(message: &Msg) -> bool {
        matches!(message, Msg::SetMediaWatch(_) | Msg::MediaCommand(_))
    }

    pub(super) fn command<B: Backend>(
        &mut self,
        backend: &mut B,
        request_id: u64,
        message: Msg,
        active_source: bool,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        if !active_source {
            self.disable(backend, emit);
        }
        match message {
            Msg::SetMediaWatch(watch) => {
                if !watch.enabled {
                    if !self.enabled {
                        self.publish_empty(emit);
                    } else {
                        self.disable(backend, emit);
                    }
                    return Ok(());
                }
                if !active_source {
                    return Err(BackendError::Stopped);
                }
                self.enabled = true;
                if let Err(error) = self.refresh(backend, Instant::now(), true, emit) {
                    self.disable(backend, emit);
                    return Err(error);
                }
                Ok(())
            }
            Msg::MediaCommand(command) => {
                if !self.enabled || !active_source {
                    complete(request_id, w::MediaCommandStatus::Cancelled, emit);
                    return Ok(());
                }
                // Core metadata can lag a native player switch. Re-read the
                // current token immediately before admitting a command.
                match backend.current_media_session() {
                    Ok(Some(token)) if token.0 == command.token => {}
                    Ok(_) => {
                        complete(request_id, w::MediaCommandStatus::StaleRevision, emit);
                        return Ok(());
                    }
                    Err(error) => {
                        complete(request_id, status(Err(error)), emit);
                        return Ok(());
                    }
                }
                if (command.command == w::MediaCommandKind::Seek as i32)
                    != command.position_ms.is_some()
                {
                    complete(request_id, w::MediaCommandStatus::Unsupported, emit);
                    return Ok(());
                }
                let kind = match w::MediaCommandKind::try_from(command.command) {
                    Ok(w::MediaCommandKind::Play) => MediaCommand::Play,
                    Ok(w::MediaCommandKind::Pause) => MediaCommand::Pause,
                    Ok(w::MediaCommandKind::Toggle) => MediaCommand::Toggle,
                    Ok(w::MediaCommandKind::Next) => MediaCommand::Next,
                    Ok(w::MediaCommandKind::Previous) => MediaCommand::Previous,
                    Ok(w::MediaCommandKind::Seek) => MediaCommand::SeekTo {
                        position_ms: command.position_ms.expect("seek payload checked"),
                    },
                    _ => {
                        complete(request_id, w::MediaCommandStatus::Unsupported, emit);
                        return Ok(());
                    }
                };
                if self.pending.len() == MAX_PENDING {
                    complete(request_id, w::MediaCommandStatus::Cancelled, emit);
                    return Ok(());
                }
                let Some(native_id) = self.next_native_id.checked_add(1) else {
                    complete(request_id, w::MediaCommandStatus::Cancelled, emit);
                    return Ok(());
                };
                self.next_native_id = native_id;
                let deadline = Instant::now() + COMMAND_TIMEOUT;
                match backend.media_command(MediaCommandRequest {
                    request_id: native_id,
                    session: MediaSessionToken(command.token),
                    expected_revision: command.expected_revision,
                    command: kind,
                }) {
                    Ok(()) => self.pending.push(Pending {
                        native_id,
                        request_id,
                        deadline,
                    }),
                    Err(error) => complete(request_id, status(Err(error)), emit),
                }
                Ok(())
            }
            _ => Err(BackendError::InvalidArgument),
        }
    }

    pub(super) fn event<B: Backend>(
        &mut self,
        backend: &mut B,
        event: &Event,
        active_source: bool,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        if !active_source {
            self.disable(backend, emit);
            return;
        }
        match event {
            Event::MediaCommandCompleted { request_id, result } => {
                if let Some(index) = self.pending.iter().position(|p| p.native_id == *request_id) {
                    let pending = self.pending.remove(index);
                    let result = if Instant::now() >= pending.deadline {
                        w::MediaCommandStatus::Timeout
                    } else {
                        status(*result)
                    };
                    complete(pending.request_id, result, emit);
                }
            }
            // Never cache native event payloads: these also arrive without media consent.
            Event::MediaChanged { .. } | Event::ResyncRequired if self.enabled => {
                self.dirty = true;
            }
            _ => {}
        }
    }

    pub(super) fn tick<B: Backend>(
        &mut self,
        backend: &mut B,
        active_source: bool,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.tick_at(backend, active_source, Instant::now(), emit);
    }

    fn tick_at<B: Backend>(
        &mut self,
        backend: &mut B,
        active_source: bool,
        now: Instant,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        if !active_source {
            self.disable(backend, emit);
            return;
        }
        if !self.enabled {
            return;
        }
        if self.pending.iter().any(|pending| now >= pending.deadline) {
            // The backend exposes all-request cancellation, not per-request cancellation.
            backend.cancel_media_commands();
            for pending in self.pending.drain(..) {
                let result = if now >= pending.deadline {
                    w::MediaCommandStatus::Timeout
                } else {
                    w::MediaCommandStatus::Cancelled
                };
                complete(pending.request_id, result, emit);
            }
        }
        let elapsed = self
            .last_refresh
            .map(|last| now.saturating_duration_since(last));
        if elapsed.is_none_or(|elapsed| {
            elapsed >= POSITION_INTERVAL || (self.dirty && elapsed >= CHANGE_INTERVAL)
        }) && self.refresh(backend, now, false, emit).is_err()
        {
            self.disable(backend, emit);
        }
    }

    pub(super) fn shutdown<B: Backend>(
        &mut self,
        backend: &mut B,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.disable(backend, emit);
    }

    fn disable<B: Backend>(&mut self, backend: &mut B, emit: &mut impl FnMut(u64, Msg)) {
        if !self.enabled && self.pending.is_empty() {
            return;
        }
        self.enabled = false;
        self.dirty = false;
        self.last_refresh = None;
        self.sessions.clear();
        self.current = None;
        backend.cancel_media_commands();
        for pending in self.pending.drain(..) {
            complete(pending.request_id, w::MediaCommandStatus::Cancelled, emit);
        }
        self.publish_empty(emit);
    }

    fn publish_empty(&mut self, emit: &mut impl FnMut(u64, Msg)) {
        self.last_artwork = None;
        self.revision = self.revision.saturating_add(1);
        emit(
            0,
            Msg::MediaSessions(w::MediaSessions {
                sessions: Vec::new(),
                current_token: None,
                revision: self.revision,
            }),
        );
    }

    fn refresh<B: Backend>(
        &mut self,
        backend: &B,
        now: Instant,
        force: bool,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        let sessions = backend.media_sessions()?;
        let current = backend.current_media_session()?;
        self.publish_snapshot(sessions, current, now, force, emit)
    }

    fn publish_snapshot(
        &mut self,
        sessions: Vec<MediaInfo>,
        current: Option<MediaSessionToken>,
        now: Instant,
        force: bool,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        let mut bounded = sessions;
        let mut tokens = [MediaSessionToken(0); MAX_SESSIONS];
        let mut count = 0;
        bounded.retain(|info| {
            if count == MAX_SESSIONS
                || info.session.0 == 0
                || info.revision == 0
                || tokens[..count].contains(&info.session)
            {
                return false;
            }
            tokens[count] = info.session;
            count += 1;
            true
        });
        let current = current.filter(|token| bounded.iter().any(|info| info.session == *token));
        self.last_refresh = Some(now);
        self.dirty = false;
        if !force && self.sessions == bounded && self.current == current {
            return Ok(());
        }
        self.revision = self.revision.checked_add(1).ok_or(BackendError::Busy)?;
        self.sessions = bounded;
        self.current = current;
        let sessions = self
            .sessions
            .iter()
            .map(|info| w::MediaInfo {
                artwork_id: info
                    .artwork
                    .as_ref()
                    .map_or_else(Vec::new, |art| art.id.to_vec()),
                token: info.session.0,
                revision: info.revision,
                app_id: bounded_text(info.app(), 128),
                title: bounded_text(info.title(), 256),
                subtitle: bounded_text(info.subtitle(), 256),
                position_ms: info.position_ms,
                duration_ms: info.duration_ms,
                can_seek: Some(info.seek_range().is_some()),
                seek_min_ms: info.seek_range().map(|(min, _)| min),
                seek_max_ms: info.seek_range().map(|(_, max)| max),
                state: match info.state {
                    PlaybackState::Playing => w::PlaybackState::Playing,
                    PlaybackState::Paused => w::PlaybackState::Paused,
                    PlaybackState::Stopped => w::PlaybackState::Stopped,
                    PlaybackState::Unknown => w::PlaybackState::Unknown,
                } as i32,
                commands: u32::from(info.commands.play)
                    | (u32::from(info.commands.pause) << 1)
                    | (u32::from(info.commands.toggle) << 2)
                    | (u32::from(info.commands.next) << 3)
                    | (u32::from(info.commands.previous) << 4),
            })
            .collect();
        emit(
            0,
            Msg::MediaSessions(w::MediaSessions {
                sessions,
                current_token: current.map(|token| token.0),
                revision: self.revision,
            }),
        );
        let current_artwork = self
            .current
            .and_then(|token| self.sessions.iter().find(|info| info.session == token))
            .and_then(|info| info.artwork.as_ref().map(|art| (info, art)));
        let identity = current_artwork.map(|(info, art)| (info.session, art.id));
        if identity != self.last_artwork {
            self.last_artwork = identity;
            if let Some((info, art)) = current_artwork {
                emit(
                    0,
                    Msg::MediaArtwork(w::MediaArtwork {
                        token: info.session.0,
                        revision: info.revision,
                        artwork_id: art.id.to_vec(),
                        data: art.data.to_vec(),
                        width: art.width,
                        height: art.height,
                    }),
                );
            }
        }
        Ok(())
    }
}

fn bounded_text(value: &str, max: usize) -> String {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn status(result: Result<(), BackendError>) -> w::MediaCommandStatus {
    match result {
        Ok(()) => w::MediaCommandStatus::Ok,
        Err(BackendError::StaleRevision | BackendError::DeviceGone) => {
            w::MediaCommandStatus::StaleRevision
        }
        Err(BackendError::Timeout) => w::MediaCommandStatus::Timeout,
        Err(
            BackendError::Unsupported
            | BackendError::PermissionDenied
            | BackendError::ServerUnavailable
            | BackendError::InvalidArgument,
        ) => w::MediaCommandStatus::Unsupported,
        Err(_) => w::MediaCommandStatus::Cancelled,
    }
}

fn complete(request_id: u64, result: w::MediaCommandStatus, emit: &mut impl FnMut(u64, Msg)) {
    emit(
        request_id,
        Msg::MediaCommandResult(w::MediaCommandResult {
            result: result as i32,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_backend::{
        dry::{DryBackend, ScriptEvent},
        Capabilities, MediaCommands,
    };

    fn backend() -> DryBackend {
        DryBackend::new(Capabilities {
            media: true,
            ..Capabilities::default()
        })
    }

    fn info(token: u64, revision: u64) -> MediaInfo {
        let mut info = MediaInfo::new(
            "player".into(),
            "private title".into(),
            "private subtitle".into(),
            MediaSessionToken(token),
            revision,
            PlaybackState::Playing,
            MediaCommands {
                play: true,
                ..MediaCommands::default()
            },
        )
        .unwrap();
        info.position_ms = Some(42);
        info
    }

    fn seed(backend: &mut DryBackend) {
        backend
            .apply(ScriptEvent::SetMedia {
                session: MediaSessionToken(1),
                info: Some(info(1, 1)),
            })
            .unwrap();
        backend
            .apply(ScriptEvent::SetCurrentMediaSession {
                session: Some(MediaSessionToken(1)),
            })
            .unwrap();
    }

    fn watch(
        controls: &mut Controls,
        backend: &mut DryBackend,
        enabled: bool,
        active: bool,
        output: &mut Vec<(u64, Msg)>,
    ) -> Result<(), BackendError> {
        controls.command(
            backend,
            100,
            Msg::SetMediaWatch(w::SetMediaWatch { enabled }),
            active,
            &mut |id, message| output.push((id, message)),
        )
    }

    fn command(
        controls: &mut Controls,
        backend: &mut DryBackend,
        id: u64,
        revision: u64,
        kind: w::MediaCommandKind,
        output: &mut Vec<(u64, Msg)>,
    ) {
        controls
            .command(
                backend,
                id,
                Msg::MediaCommand(w::MediaCommand {
                    token: 1,
                    expected_revision: revision,
                    command: kind as i32,
                    position_ms: None,
                }),
                true,
                &mut |id, message| output.push((id, message)),
            )
            .unwrap();
    }

    fn poll(controls: &mut Controls, backend: &mut DryBackend, output: &mut Vec<(u64, Msg)>) {
        while let Some(event) = backend.poll_event() {
            controls.event(backend, &event, true, &mut |id, message| {
                output.push((id, message));
            });
        }
    }

    fn results(output: &[(u64, Msg)]) -> Vec<(u64, i32)> {
        output
            .iter()
            .filter_map(|(id, message)| match message {
                Msg::MediaCommandResult(result) => Some((*id, result.result)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn explicit_watch_requires_active_source_and_off_events_keep_no_metadata() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        assert!(Controls::handles(&Msg::SetMediaWatch(w::SetMediaWatch {
            enabled: true,
        })));
        assert_eq!(
            watch(&mut controls, &mut backend, true, false, &mut output),
            Err(BackendError::Stopped)
        );
        poll(&mut controls, &mut backend, &mut output);
        controls.tick(&mut backend, true, &mut |id, msg| output.push((id, msg)));
        assert!(output.is_empty());
        assert!(controls.sessions.is_empty());
        assert!(controls.current.is_none());
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        let (id, Msg::MediaSessions(snapshot)) = &output[0] else {
            panic!("expected media snapshot");
        };
        assert_eq!(*id, 0);
        assert_eq!(snapshot.current_token, Some(1));
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.sessions[0].title, "private title");
        assert_eq!(snapshot.sessions[0].position_ms, Some(42));
        assert!(!format!("{:?}", controls.sessions[0]).contains("private"));
        controls.tick(&mut backend, false, &mut |id, msg| output.push((id, msg)));
        assert!(!controls.enabled);
        assert!(controls.sessions.is_empty());
        assert!(controls.current.is_none());
        let (_, Msg::MediaSessions(snapshot)) = output.last().unwrap() else {
            panic!("expected clearing snapshot");
        };
        assert!(snapshot.sessions.is_empty());
        assert!(snapshot.current_token.is_none());
        assert_eq!(snapshot.revision, 2);
        let count = output.len();
        controls.tick(&mut backend, true, &mut |id, msg| output.push((id, msg)));
        assert_eq!(output.len(), count);
    }

    #[test]
    fn snapshots_bound_sessions_current_identity_and_utf8_fields() {
        let mut controls = Controls::new();
        let mut output = Vec::new();
        let sessions = (1..=40).map(|token| info(token, 1)).collect();
        controls
            .publish_snapshot(
                sessions,
                Some(MediaSessionToken(40)),
                Instant::now(),
                true,
                &mut |id, msg| output.push((id, msg)),
            )
            .unwrap();
        let (_, Msg::MediaSessions(snapshot)) = &output[0] else {
            panic!("expected snapshot");
        };
        assert_eq!(snapshot.sessions.len(), 32);
        assert!(snapshot.current_token.is_none());
        assert!(snapshot.sessions.iter().all(|info| info.revision > 0));
        assert_eq!(bounded_text(&"é".repeat(200), 255).len(), 254);
        assert_eq!(bounded_text(&"x".repeat(300), 256).len(), 256);
    }

    #[test]
    fn dry_commands_complete_only_after_external_pump_and_validate_revision() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        command(
            &mut controls,
            &mut backend,
            7,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        assert!(results(&output).is_empty());
        assert!(backend.last_media_command().is_none());
        backend.pump_control();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![(7, w::MediaCommandStatus::Ok as i32)]
        );
        command(
            &mut controls,
            &mut backend,
            8,
            0,
            w::MediaCommandKind::Play,
            &mut output,
        );
        command(
            &mut controls,
            &mut backend,
            9,
            2,
            w::MediaCommandKind::Next,
            &mut output,
        );
        backend.pump_control();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![
                (7, w::MediaCommandStatus::Ok as i32),
                (8, w::MediaCommandStatus::StaleRevision as i32),
                (9, w::MediaCommandStatus::Unsupported as i32),
            ]
        );
    }

    #[test]
    fn seek_executes_position_and_preserves_current_token_revision_source_guards() {
        let mut backend = backend();
        let mut media = info(1, 1);
        media.commands.seek = true;
        media.duration_ms = Some(100);
        media.seek_min_ms = Some(0);
        media.seek_max_ms = Some(100);
        backend
            .apply(ScriptEvent::SetMedia {
                session: media.session,
                info: Some(media),
            })
            .unwrap();
        backend
            .apply(ScriptEvent::SetCurrentMediaSession {
                session: Some(MediaSessionToken(1)),
            })
            .unwrap();
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        for (id, token, revision, position, active, expected) in [
            (1, 1, 1, 0, true, w::MediaCommandStatus::Ok),
            (2, 0, 2, 10, true, w::MediaCommandStatus::StaleRevision),
            (3, 1, 1, 10, true, w::MediaCommandStatus::StaleRevision),
            (4, 1, 2, 101, true, w::MediaCommandStatus::Unsupported),
            (5, 1, 2, 10, false, w::MediaCommandStatus::Cancelled),
        ] {
            controls
                .command(
                    &mut backend,
                    id,
                    Msg::MediaCommand(w::MediaCommand {
                        token,
                        expected_revision: revision,
                        command: w::MediaCommandKind::Seek as i32,
                        position_ms: Some(position),
                    }),
                    active,
                    &mut |id, message| output.push((id, message)),
                )
                .unwrap();
            backend.pump_control();
            poll(&mut controls, &mut backend, &mut output);
            assert!(results(&output).contains(&(id, expected as i32)));
            assert_eq!(backend.media_sessions().unwrap()[0].position_ms, Some(0));
        }
    }

    #[test]
    fn timeout_and_disable_cancel_without_late_double_completion() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        command(
            &mut controls,
            &mut backend,
            11,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        let deadline = controls.pending[0].deadline;
        controls.tick_at(&mut backend, true, deadline, &mut |id, msg| {
            output.push((id, msg))
        });
        backend.pump_control();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![(11, w::MediaCommandStatus::Timeout as i32)]
        );
        assert!(backend.last_media_command().is_none());
        // Reusing a wire request ID cannot match the earlier native completion.
        command(
            &mut controls,
            &mut backend,
            11,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        controls.event(
            &mut backend,
            &Event::MediaCommandCompleted {
                request_id: 1,
                result: Ok(()),
            },
            true,
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(results(&output).len(), 1);
        watch(&mut controls, &mut backend, false, true, &mut output).unwrap();
        backend.pump_control();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![
                (11, w::MediaCommandStatus::Timeout as i32),
                (11, w::MediaCommandStatus::Cancelled as i32),
            ]
        );
        assert!(controls.sessions.is_empty());
        assert!(backend.last_media_command().is_none());
    }

    #[test]
    fn notifications_coalesce_and_positions_refresh_at_one_hz() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        let now = controls.last_refresh.unwrap();
        let mut changed = info(1, 2);
        changed.position_ms = Some(84);
        backend
            .apply(ScriptEvent::SetMedia {
                session: MediaSessionToken(1),
                info: Some(changed),
            })
            .unwrap();
        poll(&mut controls, &mut backend, &mut output);
        for _ in 0..100 {
            controls.event(
                &mut backend,
                &Event::ResyncRequired,
                true,
                &mut |id, msg| output.push((id, msg)),
            );
        }
        controls.tick_at(&mut backend, true, now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert_eq!(output.len(), 1);
        controls.tick_at(&mut backend, true, now + CHANGE_INTERVAL, &mut |id, msg| {
            output.push((id, msg))
        });
        assert_eq!(output.len(), 2);
        assert!(!controls.dirty);
        // Position-only changes are discovered by the periodic query without an event.
        let mut position = info(1, 3);
        position.position_ms = Some(126);
        backend
            .apply(ScriptEvent::SetMedia {
                session: MediaSessionToken(1),
                info: Some(position),
            })
            .unwrap();
        controls.tick_at(
            &mut backend,
            true,
            now + CHANGE_INTERVAL + POSITION_INTERVAL,
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(output.len(), 3);
        let (_, Msg::MediaSessions(snapshot)) = output.last().unwrap() else {
            panic!("expected snapshot");
        };
        assert_eq!(snapshot.sessions[0].position_ms, Some(126));
        controls.tick_at(
            &mut backend,
            true,
            now + CHANGE_INTERVAL + POSITION_INTERVAL * 2,
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(output.len(), 3);
    }

    #[test]
    fn availability_loss_and_shutdown_cancel_and_erase() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        command(
            &mut controls,
            &mut backend,
            12,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        backend
            .apply(ScriptEvent::Availability(Some(
                BackendError::ServerUnavailable,
            )))
            .unwrap();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![(12, w::MediaCommandStatus::Unsupported as i32)]
        );
        let now = controls.last_refresh.unwrap() + POSITION_INTERVAL;
        controls.tick_at(&mut backend, true, now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert!(!controls.enabled);
        assert!(controls.sessions.is_empty());
        backend.apply(ScriptEvent::Availability(None)).unwrap();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        command(
            &mut controls,
            &mut backend,
            13,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        poll(&mut controls, &mut backend, &mut output);
        assert!(controls.sessions.is_empty());
        assert_eq!(
            results(&output).last(),
            Some(&(13, w::MediaCommandStatus::Cancelled as i32))
        );
    }

    #[test]
    fn admission_is_bounded_and_sync_errors_are_correlated() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        for id in 1..=17 {
            command(
                &mut controls,
                &mut backend,
                id,
                1,
                w::MediaCommandKind::Play,
                &mut output,
            );
        }
        assert_eq!(controls.pending.len(), 16);
        assert_eq!(
            results(&output),
            vec![(17, w::MediaCommandStatus::Cancelled as i32)]
        );
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(results(&output).len(), 17);
        backend
            .apply(ScriptEvent::Availability(Some(
                BackendError::PermissionDenied,
            )))
            .unwrap();
        // An already enabled watch still maps synchronous admission failure to a result.
        controls.enabled = true;
        command(
            &mut controls,
            &mut backend,
            18,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        assert_eq!(
            results(&output).last(),
            Some(&(18, w::MediaCommandStatus::Unsupported as i32))
        );
        assert!(controls.pending.is_empty());
    }

    #[test]
    fn disappeared_session_is_stale_and_active_loss_cancels_pending() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        command(
            &mut controls,
            &mut backend,
            20,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        backend
            .apply(ScriptEvent::SetMedia {
                session: MediaSessionToken(1),
                info: None,
            })
            .unwrap();
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output),
            vec![(20, w::MediaCommandStatus::StaleRevision as i32)]
        );
        backend = self::backend();
        seed(&mut backend);
        command(
            &mut controls,
            &mut backend,
            21,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        controls.event(
            &mut backend,
            &Event::ResyncRequired,
            false,
            &mut |id, msg| output.push((id, msg)),
        );
        poll(&mut controls, &mut backend, &mut output);
        assert_eq!(
            results(&output).last(),
            Some(&(21, w::MediaCommandStatus::Cancelled as i32))
        );
        assert!(controls.sessions.is_empty());
        assert!(controls.current.is_none());
        assert!(!controls.enabled);
    }
    #[test]
    fn command_rechecks_current_player_without_waiting_for_metadata_push() {
        let mut backend = backend();
        seed(&mut backend);
        let mut controls = Controls::new();
        let mut output = Vec::new();
        watch(&mut controls, &mut backend, true, true, &mut output).unwrap();
        backend
            .apply(ScriptEvent::SetMedia {
                session: MediaSessionToken(2),
                info: Some(info(2, 1)),
            })
            .unwrap();
        backend
            .apply(ScriptEvent::SetCurrentMediaSession {
                session: Some(MediaSessionToken(2)),
            })
            .unwrap();
        command(
            &mut controls,
            &mut backend,
            21,
            1,
            w::MediaCommandKind::Play,
            &mut output,
        );
        assert_eq!(
            results(&output),
            vec![(21, w::MediaCommandStatus::StaleRevision as i32)]
        );
        assert!(controls.pending.is_empty());
        assert!(backend.last_media_command().is_none());
    }
}
