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
    native_epoch: u64,
    native_blocked: bool,
    native_status: Option<(u64, i32, bool, i32)>,
    discard_native_proof: bool,
    portal_observation: bool,
    native_unavailable_reason: i32,
    privacy: Arc<crate::privacy::Privacy>,
    pub(super) fence: Option<(u64, u64)>,
}

impl<M: MuteGuard + 'static> Controls<M> {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::new_with_events(|_, _| {})
    }

    #[cfg(test)]
    pub(super) fn new_with_events(events: impl Fn(u64, Msg) + Send + Sync + 'static) -> Self {
        Self::with_privacy(crate::privacy::Privacy::new(), events)
    }

    pub(super) fn with_privacy(
        privacy: Arc<crate::privacy::Privacy>,
        events: impl Fn(u64, Msg) + Send + Sync + 'static,
    ) -> Self {
        Self {
            guard: None,
            events: Arc::new(events),
            snapshot: None,
            configured: false,
            press: None,
            last_press: None,
            native_epoch: 0,
            native_blocked: false,
            native_status: None,
            discard_native_proof: false,
            portal_observation: false,
            native_unavailable_reason: 3,
            privacy,
            fence: None,
        }
    }

    fn sync_native_fence(&mut self) {
        let epoch = self
            .privacy
            .native_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        if epoch != self.native_epoch {
            self.native_epoch = epoch;
            self.native_blocked = true;
            self.discard_native_proof = true;
            self.press = None;
        }
    }

    pub(super) fn begin_native_events(&mut self) {
        self.sync_native_fence();
    }
    pub(super) fn native_queue_drained(&mut self) {
        self.sync_native_fence();
        // Pending proofs predate an unobservable queue boundary; only later
        // native key-up observations may rearm the current safety epoch.
        self.discard_native_proof = false;
    }

    fn publish_native_status(&mut self, now_ns: Option<u64>, emit: &mut impl FnMut(u64, Msg)) {
        let state = if !self.configured || now_ns.is_none() {
            3
        } else if self.native_blocked {
            2
        } else {
            1
        };
        let reason = if now_ns.is_none() {
            1
        } else if !self.configured {
            self.native_unavailable_reason
        } else {
            0
        };
        self.privacy
            .native_failure_reason
            .store(reason as u64, std::sync::atomic::Ordering::Release);
        self.privacy
            .native_registered
            .store(self.configured, std::sync::atomic::Ordering::Release);
        let status = (self.native_epoch, state, self.configured, reason);
        if self.native_status != Some(status) {
            self.native_status = Some(status);
            emit(
                0,
                Msg::PttStatus(w::PttStatus {
                    state,
                    safety_epoch: self.native_epoch,
                    registered: self.configured,
                    reason,
                }),
            );
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
        self.sync_native_fence();
        let fence = self.fence.take().unwrap_or_else(|| {
            (
                self.privacy
                    .epoch
                    .load(std::sync::atomic::Ordering::Acquire),
                self.privacy
                    .talk_epoch
                    .load(std::sync::atomic::Ordering::Acquire),
            )
        });
        match message {
            Msg::AcquireMicMute(request) => {
                if self.guard.as_ref().is_some_and(|guard| guard.finished()) {
                    self.guard.take();
                }
                if fence.0
                    != self
                        .privacy
                        .epoch
                        .load(std::sync::atomic::Ordering::Acquire)
                    || self
                        .privacy
                        .failed
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(BackendError::Stopped);
                }
                if self.guard.is_some() {
                    return Err(BackendError::Busy);
                }
                // A completed release may leave reductions for that old guard in the mailbox.
                while let Some(reduction) = self.privacy.next() {
                    match reduction {
                        crate::privacy::Reduction::ReleaseAll(id) => {
                            emit(id, Msg::PttEvent(talk_guard::event(0, 3, 2)));
                        }
                        crate::privacy::Reduction::Revoke(id, grant) => {
                            emit(id, Msg::PttEvent(talk_guard::event(grant, 5, 2)))
                        }
                        crate::privacy::Reduction::Mute(id, _)
                        | crate::privacy::Reduction::Release(id) => {
                            emit(
                                id,
                                Msg::Error(w::Error {
                                    code: 6,
                                    session_id: Vec::new(),
                                }),
                            );
                        }
                    }
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
                        if fence.0
                            != self
                                .privacy
                                .epoch
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            let _ = guard.restore();
                            return Err(BackendError::Stopped);
                        }
                        self.guard = Some(talk_guard::Guard::new(
                            guard,
                            self.events.clone(),
                            self.privacy.clone(),
                        )?);
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
                let snapshot = guard.set(request.expected_generation, request.muted, fence)?;
                self.publish(request_id, snapshot, emit);
                Ok(())
            }
            Msg::PttGrant(request) => {
                let result = self.guard.as_ref().map_or_else(
                    || talk_guard::event(request.grant_id, 4, 7),
                    |guard| guard.grant(request.grant_id, request.ttl_ms, admitted_at, fence),
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
                if fence.1
                    != self
                        .privacy
                        .talk_epoch
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(BackendError::Stopped);
                }
                // Reconfiguration revokes the previous grant before native admission, even on failure.
                self.revoke(backend, 2, emit);
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
                self.native_unavailable_reason = 3;
                self.portal_observation = false;
                if let Err(error) =
                    backend.configure_ptt(binding, &reserved[..request.reserved.len()])
                {
                    self.publish_native_status(crate::clock::now_ns(), emit);
                    return Err(error);
                }
                self.configured = binding.is_some();
                let cutoff = self
                    .privacy
                    .native_fence_ns
                    .load(std::sync::atomic::Ordering::Acquire);
                self.privacy.fence_native((cutoff != 0).then_some(cutoff));
                self.sync_native_fence();
                self.native_blocked = true;
                self.publish_native_status(crate::clock::now_ns(), emit);
                // Admission acknowledgment, not evidence that an OS permission prompt was granted.
                emit(request_id, released(2, self.native_epoch));
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
                guard_active: self.guard.as_ref().is_some_and(|guard| !guard.finished()),
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

    fn revoke<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        reason: i32,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        let held = self.press.take().is_some();
        let observed = self.configured || self.native_status.is_some();
        if self.configured || held {
            let _ = backend.configure_ptt(None, &[]);
        }
        self.configured = false;
        self.native_blocked = true;
        self.privacy
            .native_registered
            .store(false, std::sync::atomic::Ordering::Release);
        if held {
            emit(0, released(reason, self.native_epoch));
        }
        if observed {
            self.publish_native_status(crate::clock::now_ns(), emit);
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
                // Native notifications can outlive a released guard. Never attach an
                // old observation to a newly acquired guard on the same endpoint.
                let snapshot = match self.guard.as_ref() {
                    Some(guard) => match guard.snapshot() {
                        Ok(current) if current.endpoint == snapshot.endpoint => current,
                        Ok(_) => return,
                        Err(_) => {
                            let _ = self.release(backend, 0, emit);
                            return;
                        }
                    },
                    None => snapshot.clone(),
                };
                if self.snapshot.as_ref().is_some_and(|old| {
                    old.endpoint == snapshot.endpoint && old.generation != snapshot.generation
                }) {
                    let _ = self.release(backend, 0, emit);
                }
                self.publish(0, snapshot, emit);
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
                let cutoff = self
                    .privacy
                    .native_fence_ns
                    .load(std::sync::atomic::Ordering::Acquire);
                if cutoff != 0 && *observed_at_ns <= cutoff {
                    self.native_blocked = true;
                    self.publish_native_status(now_ns, emit);
                    return;
                }
                if self.portal_observation && clock.saturating_sub(*observed_at_ns) > PTT_LEASE_NS {
                    self.native_unavailable_reason = 2;
                    self.configured = false;
                    self.privacy.fence_native(now_ns);
                    self.sync_native_fence();
                    self.publish_native_status(now_ns, emit);
                    return;
                }
                if !self.configured
                    || self.native_blocked
                    || self.press.is_some()
                    || self.last_press.is_some_and(|last| *observed_at_ns <= last)
                    || *observed_at_ns > clock
                    || *expires_at_ns <= clock
                    || (!self.portal_observation
                        && expires_at_ns.saturating_sub(*observed_at_ns) > PTT_LEASE_NS)
                    || expires_at_ns.saturating_sub(clock) > PTT_LEASE_NS
                    || *expires_at_ns <= *observed_at_ns
                {
                    return;
                }
                self.last_press = Some(*observed_at_ns);
                self.press = Some((
                    *expires_at_ns,
                    now + Duration::from_nanos(*expires_at_ns - clock),
                ));
                if self.native_epoch
                    != self
                        .privacy
                        .native_epoch
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    self.sync_native_fence();
                    return;
                }
                emit(
                    0,
                    Msg::PttEvent(w::PttEvent {
                        safety_epoch: self.native_epoch,
                        kind: 1,
                        observed_at_ns: if self.portal_observation {
                            clock
                        } else {
                            *observed_at_ns
                        },
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
                        safety_epoch: self.native_epoch,
                        kind: 2,
                        observed_at_ns: 0,
                        expires_at_ns: *expires_at_ns,
                        grant_id: 0,
                        reason: 0,
                    }),
                );
            }
            Event::PttReleased { reason } => {
                let held = self.press.take().is_some();
                match reason {
                    audio_backend::PttReleaseReason::PortalFreshnessUnavailable => {
                        self.native_unavailable_reason = 2;
                        self.configured = false;
                        self.privacy.fence_native(now_ns);
                        self.sync_native_fence();
                    }
                    audio_backend::PttReleaseReason::PortalDeactivated { observed_at_ns } => {
                        let cutoff = self
                            .privacy
                            .native_fence_ns
                            .load(std::sync::atomic::Ordering::Acquire);
                        if let Some(clock) = now_ns {
                            if *observed_at_ns > clock
                                || clock.saturating_sub(*observed_at_ns) > PTT_LEASE_NS
                            {
                                self.native_unavailable_reason = 2;
                                self.configured = false;
                                self.privacy.fence_native(now_ns);
                                self.sync_native_fence();
                            } else if self.configured
                                && !self.discard_native_proof
                                && *observed_at_ns > cutoff
                            {
                                self.portal_observation = true;
                                self.native_blocked = false;
                                emit(0, released(9, self.native_epoch));
                            } else {
                                self.native_blocked = true;
                            }
                        } else {
                            self.native_blocked = true;
                        }
                    }
                    audio_backend::PttReleaseReason::PhysicalKeyUp
                        if now_ns.is_some() && self.configured && !self.discard_native_proof =>
                    {
                        self.native_blocked = false;
                        emit(0, released(8, self.native_epoch));
                        self.portal_observation = false;
                    }
                    audio_backend::PttReleaseReason::LeaseExpired => {
                        if !self.native_blocked {
                            self.privacy.fence_native(now_ns);
                            self.sync_native_fence();
                        }
                        self.native_blocked = true;
                        if held {
                            emit(0, released(1, self.native_epoch));
                        }
                    }
                    audio_backend::PttReleaseReason::UnavailableOrReset => {
                        if !self.native_blocked {
                            self.privacy.fence_native(now_ns);
                            self.sync_native_fence();
                        }
                        self.native_blocked = true;
                        if held {
                            emit(0, released(7, self.native_epoch));
                        }
                    }
                    audio_backend::PttReleaseReason::PhysicalKeyUp => {
                        self.native_blocked = true;
                        if now_ns.is_none() {
                            emit(0, released(7, self.native_epoch));
                        }
                    }
                }
                self.publish_native_status(now_ns, emit);
            }
            Event::ResyncRequired => {
                self.revoke(backend, 7, emit);
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
        _backend: &mut B,
        now_ns: Option<u64>,
        now: Instant,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.sync_native_fence();
        let expired = self.press.is_some_and(|(expiry, deadline)| {
            now_ns.is_some_and(|clock| clock >= expiry) || now >= deadline
        });
        if expired
            || now_ns.is_none()
                && self.configured
                && self.native_status.is_none_or(|(_, state, _, _)| state != 3)
        {
            let held = self.press.take().is_some();
            self.privacy.fence_native(now_ns);
            self.sync_native_fence();
            self.native_blocked = true;
            if held {
                emit(
                    0,
                    released(if now_ns.is_none() { 7 } else { 1 }, self.native_epoch),
                );
            }
        }
        if self.configured || self.native_epoch != 0 || self.native_status.is_some() {
            self.publish_native_status(now_ns, emit);
        }
    }

    pub(super) fn shutdown<B: Backend<Mute = M>>(
        &mut self,
        backend: &mut B,
        emit: &mut impl FnMut(u64, Msg),
    ) {
        self.revoke(backend, 2, emit);
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
fn released(reason: i32, safety_epoch: u64) -> Msg {
    Msg::PttEvent(w::PttEvent {
        safety_epoch,
        kind: 3,
        observed_at_ns: 0,
        expires_at_ns: 0,
        grant_id: 0,
        reason,
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
        let initial_registration = controls.native_status.is_none()
            && controls
                .privacy
                .talk_epoch
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            && matches!(&message,Msg::ConfigurePtt(request) if request.binding.is_some());
        let result = controls.command(backend, 7, message, &mut |id, msg| output.push((id, msg)));
        controls.native_queue_drained();
        if initial_registration && result.is_ok() {
            // Unit fixtures perform a real scripted key cycle, never a free up.
            backend.apply(ScriptEvent::PttPress).unwrap();
            backend.apply(ScriptEvent::PttRelease).unwrap();
            pump(controls, backend, &mut Vec::new(), 1, Instant::now());
        }
        result
    }
    fn pump(
        controls: &mut Controls<DryMuteGuard>,
        backend: &mut DryBackend,
        output: &mut Vec<(u64, Msg)>,
        clock: u64,
        now: Instant,
    ) {
        controls.begin_native_events();
        while let Some(event) = backend.poll_event() {
            controls.event(backend, &event, Some(clock), now, &mut |id, msg| {
                output.push((id, msg))
            });
        }
        controls.native_queue_drained();
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
    fn queued_mute_observation_cannot_replay_state_into_a_reacquired_guard() {
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
        let delayed = backend.poll_event().expect("the native mute notification");
        assert!(matches!(&delayed, Event::MuteChanged(snapshot) if snapshot.muted));
        run(
            &mut controls,
            &mut backend,
            &mut output,
            Msg::ReleaseMicMute(w::ReleaseMicMute {}),
        )
        .unwrap();
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
        output.clear();
        controls.event(
            &mut backend,
            &delayed,
            Some(0),
            Instant::now(),
            &mut |id, message| output.push((id, message)),
        );
        assert!(
            matches!(output.last(), Some((0, Msg::MuteSnapshot(snapshot)))
                if snapshot.guard_active && !snapshot.muted && snapshot.generation == generation),
            "the acquired guard must report its current state, not a queued old observation: {output:?}"
        );
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
    fn timer_expiry_closes_talk_without_relying_on_native_unregister() {
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
        assert!(
            controls.configured,
            "registration stays live to observe real key-up"
        );
        assert!(controls.native_blocked);
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
        request.reserved.push(request.binding.unwrap());
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
        assert!(controls.configured);
        assert!(controls.native_blocked);
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

    #[test]
    fn expiry_reports_native_mute_failure_without_fabricating_muted_state() {
        let (mut backend, _, mut output) = fixture();
        let (send, receive) = std::sync::mpsc::channel();
        let mut controls = Controls::new_with_events(move |_, message| {
            let _ = send.send(message);
        });
        run(&mut controls, &mut backend, &mut output, acquire()).unwrap();
        run(&mut controls, &mut backend, &mut output, grant(31, 100)).unwrap();
        while receive.try_recv().is_ok() {}
        backend
            .apply(ScriptEvent::SetMute {
                device: DeviceId::new("mic").unwrap(),
                muted: false,
            })
            .unwrap();
        let (mut failed, mut released, mut observed) = (false, false, false);
        while !released || !observed {
            match receive.recv_timeout(Duration::from_secs(1)).unwrap() {
                Msg::Error(_) => failed = true,
                Msg::PttEvent(event) => {
                    assert_eq!((event.grant_id, event.kind, event.reason), (31, 3, 1));
                    released = true;
                }
                Msg::MuteSnapshot(snapshot) => {
                    assert!(!snapshot.muted, "an external owner must not be overwritten");
                    observed = true;
                }
                _ => panic!("unexpected mute timer response"),
            }
        }
        assert!(failed);
    }

    #[test]
    fn release_all_fences_a_held_native_key_across_synthetic_release_and_reconfigure() {
        let (mut backend, mut controls, mut output) = fixture();
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        let now = Instant::now();
        let press = |observed| Event::PttPressed {
            observed_at_ns: observed,
            expires_at_ns: observed + 1_000_000,
        };
        controls.event(&mut backend, &press(100), Some(100), now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert_eq!(kinds(&output).last(), Some(&1));
        for time in [200, 201] {
            assert_eq!(
                controls
                    .privacy
                    .admit_at(9, &Msg::PttReleaseAll(w::PttReleaseAll {}), Some(time)),
                Some(Ok(false))
            );
        }
        output.clear();
        controls.event(
            &mut backend,
            &Event::PttRenewed {
                expires_at_ns: 1_000_000,
            },
            Some(300),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        for reason in [
            audio_backend::PttReleaseReason::LeaseExpired,
            audio_backend::PttReleaseReason::UnavailableOrReset,
        ] {
            controls.event(
                &mut backend,
                &Event::PttReleased { reason },
                Some(300),
                now,
                &mut |id, msg| output.push((id, msg)),
            );
        }
        run(&mut controls, &mut backend, &mut output, configure(true)).unwrap();
        controls.event(&mut backend, &press(400), Some(400), now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert!(!kinds(&output).iter().any(|kind| matches!(kind, 1 | 2)));
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PhysicalKeyUp,
            },
            Some(500),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(output
            .iter()
            .rev()
            .any(|(_, msg)| matches!(msg,Msg::PttEvent(event) if event.reason==8)));
        controls.event(&mut backend, &press(100), Some(500), now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert!(controls.native_blocked);
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PhysicalKeyUp,
            },
            Some(600),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        controls.event(&mut backend, &press(700), Some(700), now, &mut |id, msg| {
            output.push((id, msg))
        });
        assert_eq!(kinds(&output).last(), Some(&1));
    }

    #[test]
    fn release_all_revokes_old_ids_but_allows_fresh_grants_and_persistent_unmute() {
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
        run(&mut controls, &mut backend, &mut output, grant(10, 250)).unwrap();
        let privacy = controls.privacy.clone();
        for request in [90, 91] {
            assert_eq!(
                privacy.admit(request, &Msg::PttReleaseAll(w::PttReleaseAll {})),
                Some(Ok(true))
            );
            assert!(controls.guard.as_ref().unwrap().snapshot().unwrap().muted);
        }
        run(&mut controls, &mut backend, &mut output, grant(10, 250)).unwrap();
        result(&output, 10, 4, 2);
        run(&mut controls, &mut backend, &mut output, grant(11, 250)).unwrap();
        result(&output, 11, 1, 0);
        assert!(!backend.capture_mute(&selection()).unwrap().muted);
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
        assert_eq!(
            privacy.admit(92, &Msg::PttReleaseAll(w::PttReleaseAll {})),
            Some(Ok(true))
        );
        assert!(!controls.guard.as_ref().unwrap().snapshot().unwrap().muted);
        run(&mut controls, &mut backend, &mut output, grant(12, 1)).unwrap();
        result(&output, 12, 5, 6);
        std::thread::sleep(Duration::from_millis(10));
        assert!(!controls.guard.as_ref().unwrap().snapshot().unwrap().muted);
    }

    #[test]
    fn native_registration_is_observably_unarmed_and_clock_recovery_requires_real_up() {
        let (mut backend, mut controls, mut output) = fixture();
        controls
            .command(&mut backend, 7, configure(true), &mut |id, msg| {
                output.push((id, msg))
            })
            .unwrap();
        assert!(output.iter().any(
            |(_, msg)| matches!(msg,Msg::PttStatus(status) if status.registered && status.state==2)
        ));
        assert!(controls.native_blocked);
        controls.native_queue_drained();
        assert_eq!(
            controls
                .privacy
                .admit_at(8, &Msg::PttReleaseAll(w::PttReleaseAll {}), None),
            Some(Ok(false))
        );
        controls.tick(&mut backend, None, Instant::now(), &mut |id, msg| {
            output.push((id, msg))
        });
        assert!(output.iter().any(
            |(_, msg)| matches!(msg,Msg::PttStatus(status) if status.state==3 && status.reason==1)
        ));
        assert_eq!(
            controls
                .privacy
                .native_fence_ns
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        controls.native_queue_drained();
        backend.apply(ScriptEvent::PttPress).unwrap();
        backend.apply(ScriptEvent::PttRelease).unwrap();
        pump(
            &mut controls,
            &mut backend,
            &mut output,
            1000,
            Instant::now(),
        );
        assert!(!controls.native_blocked);
        backend
            .apply(ScriptEvent::AdvanceTime { now_ns: 2000 })
            .unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        pump(
            &mut controls,
            &mut backend,
            &mut output,
            2000,
            Instant::now(),
        );
        assert_eq!(kinds(&output).last(), Some(&1));
    }

    #[test]
    fn portal_logical_proof_keeps_original_freshness_and_separate_bounded_lease() {
        let (mut backend, mut controls, mut output) = fixture();
        controls
            .command(&mut backend, 7, configure(true), &mut |id, msg| {
                output.push((id, msg))
            })
            .unwrap();
        controls.native_queue_drained();
        let now = Instant::now();
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PortalDeactivated {
                    observed_at_ns: 100,
                },
            },
            Some(200),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(output.iter().any(|(_,msg)| matches!(msg,Msg::PttEvent(event) if event.reason==9 && event.safety_epoch==controls.native_epoch)));
        controls.event(
            &mut backend,
            &Event::PttPressed {
                observed_at_ns: 101,
                expires_at_ns: 200 + PTT_LEASE_NS,
            },
            Some(200),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(
            matches!(output.last(),Some((0,Msg::PttEvent(event))) if event.kind==1 && event.observed_at_ns==200 && event.expires_at_ns-event.observed_at_ns==PTT_LEASE_NS)
        );
        assert_eq!(controls.last_press, Some(101));
        controls
            .privacy
            .admit_at(8, &Msg::PttReleaseAll(w::PttReleaseAll {}), Some(300));
        controls.begin_native_events();
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PortalDeactivated {
                    observed_at_ns: 299,
                },
            },
            Some(350),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(
            controls.native_blocked,
            "queued pre-fence logical up is not proof"
        );
        controls.native_queue_drained();
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PortalDeactivated {
                    observed_at_ns: 301,
                },
            },
            Some(350),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(!controls.native_blocked);
        controls.event(
            &mut backend,
            &Event::PttReleased {
                reason: audio_backend::PttReleaseReason::PortalFreshnessUnavailable,
            },
            Some(400),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        controls.event(
            &mut backend,
            &Event::ResyncRequired,
            Some(400),
            now,
            &mut |id, msg| output.push((id, msg)),
        );
        assert!(
            matches!(output.last(),Some((0,Msg::PttStatus(status))) if status.state==3 && status.reason==2 && !status.registered)
        );
    }
}
