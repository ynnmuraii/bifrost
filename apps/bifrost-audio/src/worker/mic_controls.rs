#[path = "talk_guard.rs"]
mod talk_guard;
use audio_backend::{
    Backend, BackendError, DeviceSelection, Event, MuteGuard, MuteSnapshot, PttHotkey, PTT_LEASE_NS,
};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Controls<M: MuteGuard> {
    guard: Option<talk_guard::Guard<M>>,
    events: talk_guard::EventSink,
    snapshot: Option<MuteSnapshot>,
    configured: bool,
    press: Option<(u64, Instant)>,
    last_press: Option<u64>,
}

impl<M: MuteGuard + 'static> Controls<M> {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::new_with_events(|_, _| {})
    }

    pub(super) fn new_with_events(events: impl Fn(u64, Msg) + Send + Sync + 'static) -> Self {
        Self {
            guard: None,
            events: Arc::new(events),
            snapshot: None,
            configured: false,
            press: None,
            last_press: None,
        }
    }

    pub(super) fn handles(message: &Msg) -> bool {
        matches!(
            message,
            Msg::AcquireMicMute(_)
                | Msg::SetMicMute(_)
                | Msg::ReleaseMicMute(_)
                | Msg::ConfigurePtt(_)
                | Msg::PttGrant(_)
                | Msg::PttRevoke(_)
        )
    }

    #[cfg(test)]
    pub(super) fn command<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        request_id: u64,
        message: Msg,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        self.command_at(backend, request_id, message, Instant::now(), emit)
    }

    pub(super) fn command_at<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        request_id: u64,
        message: Msg,
        admitted_at: Instant,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        match message {
            Msg::AcquireMicMute(request) => {
                if self.guard.is_some() {
                    return Err(BackendError::Busy);
                }
                let selection = super::selection(
                    request
                        .selection
                        .as_ref()
                        .ok_or(BackendError::InvalidArgument)?,
                )
                .map_err(|_| BackendError::InvalidArgument)?;
                let mut guard = backend.acquire_mute(&selection)?;
                let snapshot = (|| {
                    let snapshot = guard.snapshot()?;
                    let current = backend
                        .capture_mute(&DeviceSelection::Explicit(snapshot.endpoint.clone()))?;
                    if snapshot != current {
                        return Err(BackendError::StaleRevision);
                    }
                    Ok(snapshot)
                })();
                match snapshot {
                    Ok(snapshot) => {
                        self.guard = Some(talk_guard::Guard::new(guard, self.events.clone())?);
                        self.snapshot = None;
                        self.publish(request_id, snapshot, emit);
                        Ok(())
                    }
                    Err(error) => {
                        let _ = guard.restore();
                        Err(error)
                    }
                }
            }
            Msg::SetMicMute(request) => {
                let guard = self.guard.as_ref().ok_or(BackendError::Stopped)?;
                let snapshot = guard.set(request.expected_generation, request.muted)?;
                self.publish(request_id, snapshot, emit);
                Ok(())
            }
            Msg::PttGrant(request) => {
                let result = self.guard.as_ref().map_or_else(
                    || talk_guard::event(request.grant_id, 4, 0),
                    |guard| guard.grant(request.grant_id, request.ttl_ms, admitted_at),
                );
                emit(request_id, Msg::PttEvent(result));
                Ok(())
            }
            Msg::PttRevoke(request) => {
                let result = self.guard.as_ref().map_or_else(
                    || talk_guard::event(request.grant_id, 5, 2),
                    |guard| guard.revoke(request.grant_id),
                );
                emit(request_id, Msg::PttEvent(result));
                Ok(())
            }
            Msg::ReleaseMicMute(_) => self.release(backend, request_id, emit),
            Msg::ConfigurePtt(request) => {
                // Reconfiguration revokes the previous grant before native admission, even on failure.
                self.revoke(backend, emit);
                let binding = request.binding.as_ref().map(hotkey);
                if request.reserved.len() > 16 {
                    return Err(BackendError::InvalidArgument);
                }
                let mut reserved = [PttHotkey {
                    key_code: 4,
                    control: false,
                    alt: false,
                    shift: false,
                    meta: false,
                }; 16];
                for (slot, key) in reserved.iter_mut().zip(&request.reserved) {
                    *slot = hotkey(key);
                }
                backend.configure_ptt(binding, &reserved[..request.reserved.len()])?;
                self.configured = binding.is_some();
                // Admission acknowledgment, not evidence that an OS permission prompt was granted.
                emit(request_id, released());
                Ok(())
            }
            _ => Err(BackendError::InvalidArgument),
        }
    }

    fn publish(
        &mut self,
        request_id: u64,
        snapshot: MuteSnapshot,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        emit(
            request_id,
            Msg::MuteSnapshot(w::MuteSnapshot {
                endpoint_id: snapshot.endpoint.as_str().to_owned(),
                generation: snapshot.generation,
                muted: snapshot.muted,
                // A local handle is held; the generic trait cannot prove native restoration ownership.
                guard_active: self.guard.is_some(),
            }),
        );
        if self.guard.is_none()
            || self
                .snapshot
                .as_ref()
                .is_none_or(|previous| previous.endpoint == snapshot.endpoint)
        {
            self.snapshot = Some(snapshot);
        }
    }

    fn release<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        request_id: u64,
        emit: &mut impl FnMut(u64, Msg),
    ) -> Result<(), BackendError> {
        let mut guard = self.guard.take().ok_or(BackendError::Stopped)?;
        let result = guard.restore();
        if let Some(previous) = self.snapshot.as_ref() {
            let snapshot =
                backend.capture_mute(&DeviceSelection::Explicit(previous.endpoint.clone()));
            match snapshot {
                Ok(snapshot) => self.publish(request_id, snapshot, emit),
                Err(error) => {
                    // Report the last observed state, but never advertise a retained guard on failure.
                    if let Some(snapshot) = self.snapshot.clone() {
                        self.publish(request_id, snapshot, emit);
                    }
                    return result.and(Err(error));
                }
            }
        }
        result
    }

    fn revoke<B: Backend<Mute = M>>(&mut self, backend: &mut B, emit: &mut impl FnMut(u64, Msg)) {
        let held = self.press.take().is_some();
        if self.configured || held {
            let _ = backend.configure_ptt(None, &[]);
        }
        self.configured = false;
        if held {
            emit(0, released());
        }
    }

    pub(super) fn event<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        event: &Event,
        now_ns: Option<u64>,
        now: Instant,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.tick(backend, now_ns, now, emit);
        match event {
            Event::MuteChanged(snapshot) => {
                if self.snapshot.as_ref().is_some_and(|old| {
                    old.endpoint == snapshot.endpoint && old.generation != snapshot.generation
                }) {
                    let _ = self.release(backend, 0, emit);
                }
                self.publish(0, snapshot.clone(), emit);
            }
            Event::Device(_) => {
                if let Some(guard) = self.guard.as_ref() {
                    if guard.snapshot().is_err() {
                        let _ = self.release(backend, 0, emit);
                    }
                }
            }
            Event::PttPressed {
                observed_at_ns,
                expires_at_ns,
            } => {
                let Some(clock) = now_ns else {
                    return;
                };
                if !self.configured
                    || self.press.is_some()
                    || self.last_press.is_some_and(|last| *observed_at_ns <= last)
                    || *observed_at_ns > clock
                    || *expires_at_ns <= clock
                    || expires_at_ns.saturating_sub(*observed_at_ns) > PTT_LEASE_NS
                    || *expires_at_ns <= *observed_at_ns
                {
                    return;
                }
                self.last_press = Some(*observed_at_ns);
                self.press = Some((
                    *expires_at_ns,
                    now + Duration::from_nanos(*expires_at_ns - clock),
                ));
                emit(
                    0,
                    Msg::PttEvent(w::PttEvent {
                        kind: 1,
                        observed_at_ns: *observed_at_ns,
                        expires_at_ns: *expires_at_ns,
                        grant_id: 0,
                        reason: 0,
                    }),
                );
            }
            Event::PttRenewed { expires_at_ns } => {
                let (Some(clock), Some((old, _))) = (now_ns, self.press) else {
                    return;
                };
                if *expires_at_ns <= old
                    || *expires_at_ns <= clock
                    || *expires_at_ns - clock > PTT_LEASE_NS
                {
                    return;
                }
                self.press = Some((
                    *expires_at_ns,
                    now + Duration::from_nanos(*expires_at_ns - clock),
                ));
                emit(
                    0,
                    Msg::PttEvent(w::PttEvent {
                        kind: 2,
                        observed_at_ns: 0,
                        expires_at_ns: *expires_at_ns,
                        grant_id: 0,
                        reason: 0,
                    }),
                );
            }
            Event::PttReleased => {
                if self.press.take().is_some() {
                    emit(0, released());
                }
            }
            Event::ResyncRequired => {
                self.revoke(backend, emit);
                if let Some(guard) = self.guard.as_ref() {
                    match guard.snapshot() {
                        Ok(snapshot) => self.publish(0, snapshot, emit),
                        Err(_) => {
                            let _ = self.release(backend, 0, emit);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn tick<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        now_ns: Option<u64>,
        now: Instant,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        if now_ns.is_none()
            || self.press.is_some_and(|(expiry, deadline)| {
                now_ns.is_some_and(|clock| clock >= expiry) || now >= deadline
            })
        {
            self.revoke(backend, emit);
        }
    }

    pub(super) fn shutdown<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.revoke(backend, emit);
        if self.guard.is_some() {
            let _ = self.release(backend, 0, emit);
        }
    }
}

fn hotkey(key: &w::PttHotkey) -> PttHotkey {
    PttHotkey {
        key_code: key.key_code,
        control: key.control,
        alt: key.alt,
        shift: key.shift,
        meta: key.meta,
    }
}
fn released() -> Msg {
    Msg::PttEvent(w::PttEvent {
        kind: 3,
        observed_at_ns: 0,
        expires_at_ns: 0,
        grant_id: 0,
        reason: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_backend::{
        dry::{DryBackend, DryMuteGuard, ScriptEvent},
        AudioFormat, Capabilities, Device, DeviceId, Direction,
    };

    fn fixture() -> (DryBackend, Controls<DryMuteGuard>, Vec<(u64, Msg)>) {
        let mut backend = DryBackend::new(Capabilities {
            endpoint_mute: true,
            global_ptt: true,
            ..Capabilities::default()
        });
        backend
            .apply(ScriptEvent::UpsertDevice(Device {
                id: DeviceId::new("mic").unwrap(),
                label: "Microphone".into(),
                direction: Direction::Capture,
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 1,
                },
            }))
            .unwrap();
        while backend.poll_event().is_some() {}
        (backend, Controls::new(), Vec::new())
    }
    fn selection() -> DeviceSelection {
        DeviceSelection::Explicit(DeviceId::new("mic").unwrap())
    }
    fn acquire() -> Msg {
        Msg::AcquireMicMute(w::AcquireMicMute {
            selection: Some(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::DeviceId("mic".into())),
            }),
        })
    }
    fn configure(enabled: bool) -> Msg {
        Msg::ConfigurePtt(w::ConfigurePtt {
            binding: enabled.then_some(w::PttHotkey {
                key_code: 4,
                control: true,
                ..Default::default()
            }),
            reserved: Vec::new(),
        })
    }
    fn run(
        controls: &mut Controls<DryMuteGuard>,
        backend: &mut DryBackend,
        output: &mut Vec<(u64, Msg)>,
        message: Msg,
    ) -> Result<(), BackendError> {
        controls.command(backend, 7, message, &mut |id, msg| output.push((id, msg)))
    }
    fn pump(
        controls: &mut Controls<DryMuteGuard>,
        backend: &mut DryBackend,
        output: &mut Vec<(u64, Msg)>,
        clock: u64,
        now: Instant,
    ) {
        while let Some(event) = backend.poll_event() {
            controls.event(backend, &event, Some(clock), now, &mut |id, msg| {
                output.push((id, msg))
            });
        }
    }
    fn kinds(output: &[(u64, Msg)]) -> Vec<i32> {
        output
            .iter()
            .filter_map(|(_, msg)| {
                if let Msg::PttEvent(event) = msg {
                    Some(event.kind)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn guard_lifecycle_stale_generation_busy_and_restore() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        assert_eq!(
            run(&mut controls, &mut backend, &mut output, acquire()),
            Err(BackendError::Busy)
        );
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        assert_eq!(
            run(
                &mut controls,
                &mut backend,
                &mut output,
                Msg::SetMicMute(w::SetMicMute {
                    expected_generation: generation + 1,
                    muted: true
                })
            ),
            Err(BackendError::StaleRevision)
        );
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::ReleaseMicMute(w::ReleaseMicMute {}),
        )
        .unwrap();
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        assert!(
            matches!(&output.last().unwrap(), (7, Msg::MuteSnapshot(snapshot)) if !snapshot.guard_active && !snapshot.muted)
        );
        assert_eq!(
            run(
                &mut controls,
                &mut backend,
                &mut output,
                Msg::ReleaseMicMute(w::ReleaseMicMute {})
            ),
            Err(BackendError::Stopped)
        );
        assert!(backend.acquire_mute(&selection()).is_ok());
    }

    #[test]
    fn external_same_value_write_keeps_mute_and_revokes_native_ownership() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        backend
            .apply(ScriptEvent::SetMute {
                device: DeviceId::new("mic").unwrap(),
                muted: true,
            })
            .unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, Instant::now());
        assert_eq!(
            run(
                &mut controls,
                &mut backend,
                &mut output,
                Msg::SetMicMute(w::SetMicMute {
                    expected_generation: generation,
                    muted: false
                })
            ),
            Err(BackendError::OwnershipLost)
        );
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        let mut replacement = Controls::<DryMuteGuard>::new();
        replacement.shutdown(&mut backend, &mut |_, _| {});
        assert!(backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn drop_restores_owned_change_but_preserves_original_mute() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        drop(controls);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        backend
            .apply(ScriptEvent::SetMute {
                device: DeviceId::new("mic").unwrap(),
                muted: true,
            })
            .unwrap();
        let mut controls = Controls::new();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        assert_eq!(
            run(
                &mut controls,
                &mut backend,
                &mut output,
                Msg::SetMicMute(w::SetMicMute {
                    expected_generation: generation,
                    muted: false
                })
            ),
            Err(BackendError::PermissionDenied)
        );
        controls.shutdown(&mut backend, &mut |_, _| {});
        assert!(backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn native_zero_press_and_renew_are_bounded_and_release_never_reopens() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        let now = Instant::now();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, now);
        assert_eq!(kinds(&output), vec![3, 1]);
        backend
            .apply(ScriptEvent::AdvanceTime {
                now_ns: 100_000_000,
            })
            .unwrap();
        backend.apply(ScriptEvent::PttRenew).unwrap();
        pump(
            &mut controls,
            &mut backend,
            &mut output,
            100_000_000,
            now + Duration::from_millis(100),
        );
        assert_eq!(kinds(&output), vec![3, 1, 2]);
        controls.event(
            &mut backend,
            &Event::PttRenewed {
                expires_at_ns: 500_000_000,
            },
            Some(100_000_000),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(controls.press.unwrap().0, 350_000_000);
        backend.apply(ScriptEvent::PttRelease).unwrap();
        pump(&mut controls, &mut backend, &mut output, 100_000_000, now);
        controls.event(
            &mut backend,
            &Event::PttRenewed {
                expires_at_ns: 350_000_000,
            },
            Some(100_000_000),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(controls.press.is_none());
        assert_eq!(kinds(&output), vec![3, 1, 2, 3]);
    }

    #[test]
    fn expired_queued_native_press_and_stale_renew_do_not_open() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        backend
            .apply(ScriptEvent::AdvanceTime { now_ns: 10 })
            .unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(
            &mut controls,
            &mut backend,
            &mut output,
            PTT_LEASE_NS + 10,
            Instant::now(),
        );
        controls.event(
            &mut backend,
            &Event::PttRenewed {
                expires_at_ns: 2 * PTT_LEASE_NS,
            },
            Some(PTT_LEASE_NS + 10),
            Instant::now(),
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(controls.press.is_none());
        assert_eq!(kinds(&output), vec![3]);
    }

    #[test]
    fn timer_expiry_releases_even_when_native_unregister_fails() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        let now = Instant::now();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, now);
        backend
            .apply(ScriptEvent::Availability(Some(
                BackendError::ServerUnavailable,
            )))
            .unwrap();
        controls.tick(
            &mut backend,
            Some(PTT_LEASE_NS),
            now + Duration::from_millis(250),
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(kinds(&output), vec![3, 1, 3]);
        assert!(!controls.configured);
        assert!(controls.press.is_none());
    }

    #[test]
    fn configure_none_conflict_resync_and_clock_failure_revoke() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, Instant::now());
        run(&mut controls, &mut backend, &mut output, configure(false)).unwrap();
        assert!(controls.press.is_none());
        assert!(!controls.configured);
        assert_eq!(kinds(&output), vec![3, 1, 3, 3]);
        let Msg::ConfigurePtt(mut request) = configure(true) else {
            unreachable!()
        };
        request.reserved.push(request.binding.clone().unwrap());
        assert_eq!(
            run(
                &mut controls,
                &mut backend,
                &mut output,
                Msg::ConfigurePtt(request)
            ),
            Err(BackendError::Conflict)
        );
        assert!(!controls.configured);
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        controls.event(
            &mut backend,
            &Event::ResyncRequired,
            Some(0),
            Instant::now(),
            &mut |_, _| {},
        );
        assert!(!controls.configured);
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        controls.tick(&mut backend, None, Instant::now(), &mut |_, _| {});
        assert!(!controls.configured);
    }

    #[test]
    fn replacement_generation_invalidates_guard_and_shutdown_restores_once() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let old = backend.capture_mute(&selection()).unwrap().generation;
        backend
            .apply(ScriptEvent::UpsertDevice(Device {
                id: DeviceId::new("mic").unwrap(),
                label: "Replacement".into(),
                direction: Direction::Capture,
                format: AudioFormat {
                    sample_rate: 48_000,
                    channels: 1,
                },
            }))
            .unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, Instant::now());
        assert!(controls.guard.is_none());
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        assert_ne!(old, generation);
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        let count = output.len();
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        assert_eq!(output.len(), count);
    }

    #[test]
    fn local_deadline_expires_even_if_native_clock_stalls() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        let now = Instant::now();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, now);
        controls.tick(
            &mut backend,
            Some(0),
            now + Duration::from_millis(250),
            &mut |id, msg| output.push((id, msg)),
        );
        assert_eq!(kinds(&output), vec![3, 1, 3]);
        assert!(controls.press.is_none());
    }

    #[test]
    fn shutdown_releases_ptt_without_coupling_it_to_endpoint_mute() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        let now = Instant::now();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(&mut controls, &mut backend, &mut output, 0, now);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        controls.shutdown(&mut backend, &mut |id, msg| output.push((id, msg)));
        assert_eq!(kinds(&output), vec![3, 1, 3]);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        assert_eq!(
            backend.apply(ScriptEvent::PttPress),
            Err(BackendError::Unsupported)
        );
        assert!(!Controls::<DryMuteGuard>::new().configured);
    }

    fn grant(id: u64, ttl_ms: u32) -> Msg {
        Msg::PttGrant(w::PttGrant {
            grant_id: id,
            ttl_ms,
        })
    }

    fn revoke_grant(id: u64) -> Msg {
        Msg::PttRevoke(w::PttRevoke { grant_id: id })
    }

    fn result(output: &[(u64, Msg)], id: u64, kind: i32, reason: i32) {
        assert!(matches!(output.last(), Some((7, Msg::PttEvent(event)))
            if event.grant_id == id && event.kind == kind && event.reason == reason));
    }

    #[test]
    fn remote_grants_expire_without_control_worker_ticks_and_restore_on_drop() {
        let (mut backend, _, mut output) = fixture();
        let (send, receive) = std::sync::mpsc::channel();
        let mut controls = Controls::new_with_events(move |id, event| {
            if matches!(event, Msg::PttEvent(_)) {
                let _ = send.send((id, event));
            }
        });
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        run(&mut controls, &mut backend, &mut output, grant(1, 30)).unwrap();
        result(&output, 1, 1, 0);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        let (request, Msg::PttEvent(event)) = receive.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("missing independent release")
        };
        assert_eq!(
            (request, event.grant_id, event.kind, event.reason),
            (0, 1, 3, 1)
        );
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        drop(controls);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn remote_renew_replace_stale_revoke_and_explicit_mute() {
        let (mut backend, _, mut output) = fixture();
        let (send, receive) = std::sync::mpsc::channel();
        let mut controls = Controls::new_with_events(move |id, event| {
            if matches!(event, Msg::PttEvent(_)) {
                let _ = send.send((id, event));
            }
        });
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(&mut controls, &mut backend, &mut output, grant(10, 250)).unwrap();
        result(&output, 10, 1, 0);
        run(&mut controls, &mut backend, &mut output, grant(10, 250)).unwrap();
        result(&output, 10, 2, 0);
        run(&mut controls, &mut backend, &mut output, grant(11, 250)).unwrap();
        result(&output, 11, 1, 0);
        let (id, Msg::PttEvent(event)) = receive.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (id, event.grant_id, event.kind, event.reason),
            (0, 10, 3, 3)
        );
        run(&mut controls, &mut backend, &mut output, revoke_grant(10)).unwrap();
        result(&output, 10, 5, 2);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        run(&mut controls, &mut backend, &mut output, revoke_grant(11)).unwrap();
        result(&output, 11, 3, 2);
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        run(&mut controls, &mut backend, &mut output, grant(12, 250)).unwrap();
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        let (_, Msg::PttEvent(event)) = receive.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!()
        };
        assert_eq!((event.grant_id, event.kind, event.reason), (12, 3, 4));
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        assert!(receive.recv_timeout(Duration::from_millis(280)).is_err());
    }

    #[test]
    fn remote_grant_respects_preexisting_mute_and_persistent_unmute() {
        let (mut backend, mut controls, mut output) = fixture();
        backend
            .apply(ScriptEvent::SetMute {
                device: DeviceId::new("mic").unwrap(),
                muted: true,
            })
            .unwrap();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        run(&mut controls, &mut backend, &mut output, grant(1, 10)).unwrap();
        result(&output, 1, 4, 5);
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        drop(controls);
        backend
            .apply(ScriptEvent::SetMute {
                device: DeviceId::new("mic").unwrap(),
                muted: false,
            })
            .unwrap();
        let mut controls = Controls::new();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: false,
            }),
        )
        .unwrap();
        run(&mut controls, &mut backend, &mut output, grant(2, 1)).unwrap();
        result(&output, 2, 5, 6);
        std::thread::sleep(Duration::from_millis(20));
        run(&mut controls, &mut backend, &mut output, revoke_grant(2)).unwrap();
        result(&output, 2, 5, 2);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn remote_same_id_renewal_extends_the_deadline() {
        let (mut backend, _, mut output) = fixture();
        let (send, receive) = std::sync::mpsc::channel();
        let mut controls = Controls::new_with_events(move |id, event| {
            if matches!(event, Msg::PttEvent(_)) {
                let _ = send.send((id, event));
            }
        });
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        run(&mut controls, &mut backend, &mut output, grant(7, 150)).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        run(&mut controls, &mut backend, &mut output, grant(7, 250)).unwrap();
        result(&output, 7, 2, 0);
        assert!(receive.recv_timeout(Duration::from_millis(150)).is_err());
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        let (_, Msg::PttEvent(event)) = receive.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!()
        };
        assert_eq!((event.grant_id, event.kind, event.reason), (7, 3, 1));
        assert!(backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn queued_grant_keeps_original_admission_deadline_and_cannot_reopen() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        let generation = backend.capture_mute(&selection()).unwrap().generation;
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: true,
            }),
        )
        .unwrap();
        let admitted_at = Instant::now() - Duration::from_millis(300);
        controls
            .command_at(
                &mut backend,
                7,
                grant(8, 250),
                admitted_at,
                &mut |id, event| output.push((id, event)),
            )
            .unwrap();
        result(&output, 8, 3, 1);
        assert!(backend.capture_mute(&selection()).unwrap().muted);
        controls
            .command_at(
                &mut backend,
                7,
                grant(9, 250),
                Instant::now() - Duration::from_millis(200),
                &mut |id, event| output.push((id, event)),
            )
            .unwrap();
        result(&output, 9, 1, 0);
        let Msg::PttEvent(event) = &output.last().unwrap().1 else {
            panic!()
        };
        assert!(event.expires_at_ns - event.observed_at_ns <= 50_000_000);
        std::thread::sleep(Duration::from_millis(100));
        assert!(backend.capture_mute(&selection()).unwrap().muted);
    }

    #[test]
    fn remote_native_transitions_publish_observed_snapshots_without_worker_polls() {
        let (mut backend, _, mut output) = fixture();
        let (send, receive) = std::sync::mpsc::channel();
        let mut controls = Controls::new_with_events(move |id, message| {
            if let Msg::MuteSnapshot(snapshot) = message {
                let _ = send.send((id, snapshot));
            }
        });
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        run(&mut controls, &mut backend, &mut output, grant(20, 250)).unwrap();
        let (id, snapshot) = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(id, 0);
        assert!(snapshot.guard_active && !snapshot.muted);
        run(&mut controls, &mut backend, &mut output, revoke_grant(20)).unwrap();
        let (id, snapshot) = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(id, 0);
        assert!(snapshot.guard_active && snapshot.muted);
        run(&mut controls, &mut backend, &mut output, grant(21, 30)).unwrap();
        assert!(
            !receive
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .1
                .muted
        );
        let (id, snapshot) = receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(id, 0);
        assert!(snapshot.guard_active && snapshot.muted);
    }
}
