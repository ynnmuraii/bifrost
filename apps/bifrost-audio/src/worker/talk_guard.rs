use audio_backend::{BackendError, MuteGuard, MuteSnapshot};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::{
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

pub(super) type EventSink = Arc<dyn Fn(u64, Msg) + Send + Sync>;
type Reply<T> = mpsc::SyncSender<T>;
enum Command {
    Snapshot(Reply<Result<MuteSnapshot, BackendError>>),
    Set(u64, bool, Reply<Result<MuteSnapshot, BackendError>>),
    Grant(u64, u32, Instant, Reply<w::PttEvent>),
    Revoke(u64, Reply<w::PttEvent>),
    Restore(Reply<Result<(), BackendError>>),
}

// Only this thread touches the native guard: a blocked backend poll cannot delay remuting.
pub(super) struct Guard<M: MuteGuard> {
    commands: mpsc::SyncSender<Command>,
    thread: Option<thread::JoinHandle<M>>,
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
    pub(super) fn new(mut guard: M, sink: EventSink) -> Result<Self, BackendError> {
        let (commands, receive) = mpsc::sync_channel(4);
        let thread = thread::Builder::new()
            .name("audio-mic-grant".into())
            .spawn(move || {
                let mut active: Option<(u64, Instant)> = None;
                let mut persistent = false;
                loop {
                    // Check before receiving too: queued commands never starve the deadline.
                    if active.is_some_and(|(_, deadline)| Instant::now() >= deadline) {
                        if let Some((id, _)) = active.take() {
                            let _ = remute(&mut guard);
                            sink(0, Msg::PttEvent(event(id, 3, 1)));
                            publish(&guard, &sink);
                        }
                    }
                    let command = match active {
                        Some((_, deadline)) => {
                            receive.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        }
                        None => receive
                            .recv()
                            .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
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
                        Command::Set(generation, muted, reply) => {
                            let result = guard
                                .set_capture_mute(generation, muted)
                                .and_then(|()| guard.snapshot());
                            if result.is_ok() {
                                if let Some((id, _)) = active.take() {
                                    sink(0, Msg::PttEvent(event(id, 3, if muted { 4 } else { 6 })));
                                }
                                persistent = !muted;
                            } else if let Some((id, _)) = active.take() {
                                let _ = remute(&mut guard);
                                sink(0, Msg::PttEvent(event(id, 3, 2)));
                            }
                            publish(&guard, &sink);
                            let _ = reply.send(result);
                        }
                        Command::Grant(id, ttl_ms, deadline, reply) => {
                            let mut result = if id == 0 || !(1..=250).contains(&ttl_ms) {
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
                                        let _ = remute(&mut guard);
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
                            if !publish(&guard, &sink) {
                                if let Some((old, _)) = active.take() {
                                    let _ = remute(&mut guard);
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
                                let _ = remute(&mut guard);
                                event(id, 3, 2)
                            } else {
                                event(id, 5, 2)
                            };
                            publish(&guard, &sink);
                            let _ = reply.send(result);
                        }
                        Command::Restore(reply) => {
                            if let Some((id, _)) = active.take() {
                                let _ = remute(&mut guard);
                                sink(0, Msg::PttEvent(event(id, 3, 2)));
                            }
                            let _ = reply.send(guard.restore());
                            return guard;
                        }
                    }
                }
                if let Some((id, _)) = active.take() {
                    let _ = remute(&mut guard);
                    sink(0, Msg::PttEvent(event(id, 3, 2)));
                }
                let _ = guard.restore();
                guard
            })
            .map_err(|_| BackendError::Busy)?;
        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    pub(super) fn snapshot(&self) -> Result<MuteSnapshot, BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Snapshot(send))
            .map_err(|_| BackendError::Stopped)?;
        receive.recv().map_err(|_| BackendError::Stopped)?
    }
    pub(super) fn set(&self, generation: u64, muted: bool) -> Result<MuteSnapshot, BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Set(generation, muted, send))
            .map_err(|_| BackendError::Stopped)?;
        receive.recv().map_err(|_| BackendError::Stopped)?
    }
    pub(super) fn grant(&self, id: u64, ttl_ms: u32, admitted_at: Instant) -> w::PttEvent {
        let Some(deadline) = admitted_at.checked_add(Duration::from_millis(u64::from(ttl_ms)))
        else {
            return event(id, 3, 1);
        };
        let (send, receive) = mpsc::sync_channel(1);
        if self
            .commands
            .send(Command::Grant(id, ttl_ms, deadline, send))
            .is_err()
        {
            return event(id, 4, 7);
        }
        receive.recv().unwrap_or_else(|_| event(id, 4, 7))
    }
    pub(super) fn revoke(&self, id: u64) -> w::PttEvent {
        let (send, receive) = mpsc::sync_channel(1);
        if self.commands.send(Command::Revoke(id, send)).is_err() {
            return event(id, 5, 2);
        }
        receive.recv().unwrap_or_else(|_| event(id, 5, 2))
    }
    pub(super) fn restore(&mut self) -> Result<(), BackendError> {
        let (send, receive) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Restore(send))
            .map_err(|_| BackendError::Stopped)?;
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
            let _ = receive.recv();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}
fn remute(guard: &mut impl MuteGuard) -> Result<(), BackendError> {
    let snapshot = guard.snapshot()?;
    guard.set_capture_mute(snapshot.generation, true)
}

fn publish(guard: &impl MuteGuard, sink: &EventSink) -> bool {
    match guard.snapshot() {
        Ok(snapshot) => {
            sink(
                0,
                Msg::MuteSnapshot(w::MuteSnapshot {
                    endpoint_id: snapshot.endpoint.as_str().to_owned(),
                    generation: snapshot.generation,
                    muted: snapshot.muted,
                    guard_active: true,
                }),
            );
            true
        }
        Err(error) => {
            // Unknown native state is an error, never a fabricated muted observation.
            sink(
                0,
                Msg::Error(w::Error {
                    code: super::super::control_code(error),
                    session_id: Vec::new(),
                }),
            );
            false
        }
    }
}
