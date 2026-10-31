use audio_backend::{BackendError, MuteGuard, MuteSnapshot};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::{
    sync::{atomic::Ordering, mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

pub(super) type EventSink = Arc<dyn Fn(u64, Msg) + Send + Sync>;
struct Active(Arc<crate::privacy::Privacy>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::Release);
    }
}
type Reply<T> = mpsc::SyncSender<T>;
enum Command {
    Snapshot(Reply<Result<MuteSnapshot, BackendError>>),
    Set(
        u64,
        bool,
        (u64, u64),
        Reply<Result<MuteSnapshot, BackendError>>,
    ),
    Grant(u64, u32, Instant, (u64, u64), Reply<w::PttEvent>),
    Revoke(u64, Reply<w::PttEvent>),
    Restore(Reply<Result<(), BackendError>>),
}

// Only this thread touches the native guard: a blocked backend poll cannot delay remuting.
pub(super) struct Guard<M: MuteGuard> {
    commands: mpsc::SyncSender<Command>,
    thread: Option<thread::JoinHandle<M>>,
    privacy: Arc<crate::privacy::Privacy>,
}

pub(super) fn event(grant_id: u64, kind: i32, reason: i32) -> w::PttEvent {
    w::PttEvent {
        grant_id,
        kind,
        reason,
        observed_at_ns: 0,
        expires_at_ns: 0,
    }
}

impl<M: MuteGuard + 'static> Guard<M> {
    pub(super) fn new(
        mut guard: M,
        sink: EventSink,
        privacy: Arc<crate::privacy::Privacy>,
    ) -> Result<Self, BackendError> {
        let (commands, receive) = mpsc::sync_channel(4);
        let timer_privacy = privacy.clone();
        privacy.active.store(true, Ordering::Release);
        let thread = thread::Builder::new()
            .name("audio-mic-grant".into())
            .spawn(move || {
                let _active = Active(timer_privacy.clone());
                let mut active: Option<(u64, Instant)> = None;
                let mut persistent = false;
                let mut saturated = false;
                loop {
                    if reductions(
                        &mut guard,
                        &sink,
                        &timer_privacy,
                        &mut active,
                        &mut persistent,
                        &mut saturated,
                    ) {
                        return guard;
                    }
                    // Check before receiving too: queued commands never starve the deadline.
                    if active.is_some_and(|(_, deadline)| Instant::now() >= deadline) {
                        if let Some((id, _)) = active.take() {
                            remute(&mut guard, &sink);
                            sink(0, Msg::PttEvent(event(id, 3, 1)));
                            publish(&guard, &sink);
                        }
                    }
                    // Both queues coalesce a wake token; idle guards need no polling.
                    let timeout = active
                        .map(|(_, deadline)| deadline.saturating_duration_since(Instant::now()));
                    let command = match receive.try_recv() {
                        Ok(command) => Ok(command),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            Err(mpsc::RecvTimeoutError::Disconnected)
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            timer_privacy.wait(timeout);
                            continue;
                        }
                    };
                    let command = match command {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match command {
                        Command::Snapshot(reply) => {
                            let _ = reply.send(guard.snapshot());
                        }
                        Command::Set(generation, muted, fence, reply) => {
                            let stale = || {
                                fence.0 != timer_privacy.epoch.load(Ordering::Acquire)
                                    || fence.1 != timer_privacy.talk_epoch.load(Ordering::Acquire)
                                    || timer_privacy.failed.load(Ordering::Acquire)
                            };
                            if !muted && stale() {
                                let _ = reply.send(Err(BackendError::Stopped));
                                continue;
                            }
                            let mut result = guard
                                .set_capture_mute(generation, muted)
                                .and_then(|()| guard.snapshot());
                            if !muted && result.is_ok() && stale() {
                                remute(&mut guard, &sink);
                                result = Err(BackendError::Stopped);
                            }
                            if result.is_ok() {
                                if let Some((id, _)) = active.take() {
                                    sink(0, Msg::PttEvent(event(id, 3, if muted { 4 } else { 6 })));
                                }
                                persistent = !muted;
                            } else if let Some((id, _)) = active.take() {
                                remute(&mut guard, &sink);
                                sink(0, Msg::PttEvent(event(id, 3, 2)));
                            }
                            publish(&guard, &sink);
                            let _ = reply.send(result);
                        }
                        Command::Grant(id, ttl_ms, deadline, fence, reply) => {
                            let mut result = if fence.0
                                != timer_privacy.epoch.load(Ordering::Acquire)
                                || fence.1 != timer_privacy.talk_epoch.load(Ordering::Acquire)
                                || timer_privacy.failed.load(Ordering::Acquire)
                                || id == 0
                                || !(1..=250).contains(&ttl_ms)
                            {
                                event(id, 4, 7)
                            } else if Instant::now() >= deadline {
                                event(id, 3, 1)
                            } else if persistent {
                                event(id, 5, 6)
                            } else if guard.original_muted() {
                                event(id, 4, 5)
                            } else {
                                let timing = crate::clock::now_ns().and_then(|observed| {
                                    let remaining =
                                        deadline.saturating_duration_since(Instant::now());
                                    let nanos = u64::try_from(remaining.as_nanos()).ok()?;
                                    observed
                                        .checked_add(nanos)
                                        .filter(|expires| *expires > observed)
                                        .map(|expires| (observed, expires))
                                });
                                match timing.ok_or(BackendError::Stopped).and_then(|timing| {
                                    let snapshot = guard.snapshot()?;
                                    if Instant::now() >= deadline {
                                        return Err(BackendError::Stopped);
                                    }
                                    guard.set_capture_mute(snapshot.generation, false)?;
                                    Ok(timing)
                                }) {
                                    Ok(_) if Instant::now() >= deadline => {
                                        remute(&mut guard, &sink);
                                        if let Some((old, _)) = active.take() {
                                            if old != id {
                                                sink(0, Msg::PttEvent(event(old, 3, 3)));
                                            }
                                        }
                                        event(id, 3, 1)
                                    }
                                    Ok((observed, expires)) => {
                                        let renewed = active.is_some_and(|(old, _)| old == id);
                                        if let Some((old, _)) =
                                            active.take().filter(|(old, _)| *old != id)
                                        {
                                            sink(0, Msg::PttEvent(event(old, 3, 3)));
                                        }
                                        active = Some((id, deadline));
                                        let mut result = event(id, if renewed { 2 } else { 1 }, 0);
                                        result.observed_at_ns = if renewed { 0 } else { observed };
                                        result.expires_at_ns = expires;
                                        result
                                    }
                                    Err(_) if Instant::now() >= deadline => event(id, 3, 1),
                                    Err(_) => event(id, 4, 7),
                                }
                            };
                            if reductions(
                                &mut guard,
                                &sink,
                                &timer_privacy,
                                &mut active,
                                &mut persistent,
                                &mut saturated,
                            ) {
                                let _ = reply.send(event(id, 3, 2));
                                return guard;
                            }
                            if matches!(result.kind, 1 | 2)
                                && !active.is_some_and(|(current, _)| current == id)
                            {
                                result = event(id, 3, 2);
                            }
                            if !publish(&guard, &sink) {
                                if let Some((old, _)) = active.take() {
                                    remute(&mut guard, &sink);
                                    sink(0, Msg::PttEvent(event(old, 3, 2)));
                                }
                                if matches!(result.kind, 1 | 2) {
                                    result = event(id, 4, 7);
                                }
                            }
                            let _ = reply.send(result);
                        }
                        Command::Revoke(id, reply) => {
                            let result = if active.is_some_and(|(old, _)| old == id) {
                                active = None;
                                remute(&mut guard, &sink);
                                event(id, 3, 2)
                            } else {
                                event(id, 5, 2)
                            };
                            publish(&guard, &sink);
                            let _ = reply.send(result);
                        }
                        Command::Restore(reply) => {
                            if let Some((id, _)) = active.take() {
                                remute(&mut guard, &sink);
                                sink(0, Msg::PttEvent(event(id, 3, 2)));
                            }
                            let _ = reply.send(guard.restore());
                            return guard;
                        }
                    }
                }
                if let Some((id, _)) = active.take() {
                    remute(&mut guard, &sink);
                    sink(0, Msg::PttEvent(event(id, 3, 2)));
                }
                let _ = guard.restore();
                guard
            })
            .map_err(|_| {
                privacy.active.store(false, Ordering::Release);
                BackendError::Busy
            })?;
        Ok(Self {
            commands,
            thread: Some(thread),
            privacy,
        })
    }

    pub(super) fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
    pub(super) fn snapshot(&self) -> Result<MuteSnapshot, BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Snapshot(send))
            .map_err(|_| BackendError::Stopped)?;
        self.privacy.wake();
        receive.recv().map_err(|_| BackendError::Stopped)?
    }
    pub(super) fn set(
        &self,
        generation: u64,
        muted: bool,
        fence: (u64, u64),
    ) -> Result<MuteSnapshot, BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Set(generation, muted, fence, send))
            .map_err(|_| BackendError::Stopped)?;
        self.privacy.wake();
        receive.recv().map_err(|_| BackendError::Stopped)?
    }
    pub(super) fn grant(
        &self,
        id: u64,
        ttl_ms: u32,
        admitted_at: Instant,
        fence: (u64, u64),
    ) -> w::PttEvent {
        let Some(deadline) = admitted_at.checked_add(Duration::from_millis(u64::from(ttl_ms)))
        else {
            return event(id, 3, 1);
        };
        let (send, receive) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Grant(id, ttl_ms, deadline, fence, send))
            .is_err()
        {
            return event(id, 4, 7);
        }
        self.privacy.wake();
        receive.recv().unwrap_or_else(|_| event(id, 4, 7))
    }
    pub(super) fn revoke(&self, id: u64) -> w::PttEvent {
        let (send, receive) = mpsc::sync_channel(1);
        if self.commands.send(Command::Revoke(id, send)).is_err() {
            return event(id, 5, 2);
        }
        self.privacy.wake();
        receive.recv().unwrap_or_else(|_| event(id, 5, 2))
    }
    pub(super) fn restore(&mut self) -> Result<(), BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Restore(send))
            .map_err(|_| BackendError::Stopped)?;
        self.privacy.wake();
        let result = receive.recv().map_err(|_| BackendError::Stopped)?;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        result
    }
}
impl<M: MuteGuard> Drop for Guard<M> {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let (send, receive) = mpsc::sync_channel(1);
            let _ = self.commands.send(Command::Restore(send));
            self.privacy.wake();
            let _ = receive.recv();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
fn reductions(
    guard: &mut impl MuteGuard,
    sink: &EventSink,
    privacy: &crate::privacy::Privacy,
    active: &mut Option<(u64, Instant)>,
    persistent: &mut bool,
    saturated: &mut bool,
) -> bool {
    if privacy.failed.load(Ordering::Acquire) && !*saturated {
        *saturated = true;
        *persistent = false;
        if let Some((id, _)) = active.take() {
            sink(0, Msg::PttEvent(event(id, 3, 2)));
        }
        remute(guard, sink);
        publish(guard, sink);
    }
    while let Some(reduction) = privacy.next() {
        match reduction {
            crate::privacy::Reduction::Mute(request, generation) => {
                match guard.set_capture_mute(generation, true) {
                    Ok(()) => {
                        *persistent = false;
                        if let Some((id, _)) = active.take() {
                            sink(0, Msg::PttEvent(event(id, 3, 4)));
                        }
                        publish_at(guard, sink, request, true);
                    }
                    Err(error) => sink(
                        request,
                        Msg::Error(w::Error {
                            code: super::super::control_code(error),
                            session_id: Vec::new(),
                        }),
                    ),
                }
            }
            crate::privacy::Reduction::Revoke(request, id) => {
                let result = if active.is_some_and(|(current, _)| current == id) {
                    *active = None;
                    remute(guard, sink);
                    event(id, 3, 2)
                } else {
                    event(id, 5, 2)
                };
                sink(request, Msg::PttEvent(result));
                publish(guard, sink);
            }
            crate::privacy::Reduction::Release(request) => {
                if let Some((id, _)) = active.take() {
                    remute(guard, sink);
                    sink(0, Msg::PttEvent(event(id, 3, 2)));
                }
                match guard.restore() {
                    Ok(()) => {
                        publish_at(guard, sink, request, false);
                    }
                    Err(error) => {
                        sink(
                            request,
                            Msg::Error(w::Error {
                                code: super::super::control_code(error),
                                session_id: Vec::new(),
                            }),
                        );
                        publish_at(guard, sink, 0, false);
                    }
                }
                return true;
            }
        }
    }
    false
}

fn remute(guard: &mut impl MuteGuard, sink: &EventSink) {
    if let Err(error) = guard
        .snapshot()
        .and_then(|snapshot| guard.set_capture_mute(snapshot.generation, true))
    {
        // Expired authorization is not proof that a native mute write succeeded.
        sink(
            0,
            Msg::Error(w::Error {
                code: super::super::control_code(error),
                session_id: Vec::new(),
            }),
        );
    }
}

fn publish(guard: &impl MuteGuard, sink: &EventSink) -> bool {
    publish_at(guard, sink, 0, true)
}

fn publish_at(guard: &impl MuteGuard, sink: &EventSink, request: u64, active: bool) -> bool {
    match guard.snapshot() {
        Ok(snapshot) => {
            sink(
                request,
                Msg::MuteSnapshot(w::MuteSnapshot {
                    endpoint_id: snapshot.endpoint.as_str().to_owned(),
                    generation: snapshot.generation,
                    muted: snapshot.muted,
                    guard_active: active,
                }),
            );
            true
        }
        Err(error) => {
            // Unknown native state is an error, never a fabricated muted observation.
            sink(
                request,
                Msg::Error(w::Error {
                    code: super::super::control_code(error),
                    session_id: Vec::new(),
                }),
            );
            false
        }
    }
}

#[cfg(test)]
mod correlation_tests {
    use super::*;

    struct ReleaseGuard {
        restored: bool,
        restore_error: bool,
        snapshot_unavailable_after_restore: bool,
        muted: bool,
    }
    impl MuteGuard for ReleaseGuard {
        fn original_muted(&self) -> bool {
            false
        }
        fn snapshot(&self) -> Result<MuteSnapshot, BackendError> {
            if self.restored && self.snapshot_unavailable_after_restore {
                return Err(BackendError::ServerUnavailable);
            }
            Ok(MuteSnapshot {
                endpoint: audio_backend::DeviceId::new("fixture-mic").unwrap(),
                generation: 1,
                muted: self.muted,
            })
        }
        fn set_capture_mute(&mut self, generation: u64, muted: bool) -> Result<(), BackendError> {
            if self.restored {
                return Err(BackendError::Stopped);
            }
            if generation != 1 {
                return Err(BackendError::StaleRevision);
            }
            self.muted = muted;
            Ok(())
        }
        fn restore(&mut self) -> Result<(), BackendError> {
            self.restored = true;
            if self.restore_error {
                Err(BackendError::OwnershipLost)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn release_completes_exactly_once_even_when_post_restore_snapshot_is_unavailable() {
        for restore_error in [false, true] {
            for unavailable in [false, true] {
                let privacy = crate::privacy::Privacy::new();
                privacy.active.store(true, Ordering::Release);
                assert_eq!(
                    privacy.admit(42, &Msg::ReleaseMicMute(w::ReleaseMicMute {})),
                    Some(Ok(()))
                );
                let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
                let observed = messages.clone();
                let sink: EventSink = Arc::new(move |id, message| {
                    crate::lock(&observed).push((id, message));
                });
                let mut guard = ReleaseGuard {
                    restored: false,
                    restore_error,
                    snapshot_unavailable_after_restore: unavailable,
                    muted: true,
                };
                assert!(reductions(
                    &mut guard, &sink, &privacy, &mut None, &mut false, &mut false
                ));
                let messages = crate::lock(&messages);
                let correlated: Vec<_> = messages.iter().filter(|(id, _)| *id == 42).collect();
                assert_eq!(correlated.len(), 1);
                if restore_error || unavailable {
                    assert!(matches!(correlated[0].1, Msg::Error(_)));
                } else {
                    assert!(matches!(
                        correlated[0].1,
                        Msg::MuteSnapshot(w::MuteSnapshot {
                            guard_active: false,
                            ..
                        })
                    ));
                }
                assert!(messages.iter().all(|(id, _)| *id == 0 || *id == 42));
            }
        }
    }

    #[test]
    fn saturated_privacy_mailbox_publishes_the_actual_remuted_state() {
        let privacy = crate::privacy::Privacy::new();
        privacy.failed.store(true, Ordering::Release);
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = messages.clone();
        let sink: EventSink = Arc::new(move |id, message| {
            crate::lock(&observed).push((id, message));
        });
        let mut guard = ReleaseGuard {
            restored: false,
            restore_error: false,
            snapshot_unavailable_after_restore: false,
            muted: false,
        };
        assert!(!reductions(
            &mut guard, &sink, &privacy, &mut None, &mut true, &mut false
        ));
        assert!(crate::lock(&messages).iter().any(|(id, message)| {
            *id == 0
                && matches!(
                    message,
                    Msg::MuteSnapshot(w::MuteSnapshot {
                        muted: true,
                        guard_active: true,
                        ..
                    })
                )
        }));
    }
}
