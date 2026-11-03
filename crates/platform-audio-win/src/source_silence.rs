//! Source-only endpoint mute ownership. Process loopback spans all active render endpoints.
//! COM and setters stay on this control worker; callbacks and stream snapshots use atomics.
use crate::{backend::map_error, common::Apartment, devices};
use audio_backend::{BackendError, MAX_DEVICE_ID_BYTES};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Mutex, OnceLock, Weak,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    core::{implement, Interface, GUID, PCWSTR},
    Win32::{
        Foundation::{PROPERTYKEY, S_FALSE, S_OK},
        Media::Audio::{
            eRender, EDataFlow, ERole,
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IMMDevice, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
            AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE,
        },
        System::Com::{CoCreateGuid, CLSCTX_ALL},
    },
};

type Result<T, E = BackendError> = std::result::Result<T, E>;
type Admission = Arc<dyn Fn() -> bool + Send + Sync>;

fn advance(revision: &AtomicU64) {
    let _ = revision.try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
        Some(value.saturating_add(1))
    });
}
struct Pin {
    id: Vec<u16>,
    valid: AtomicBool,
}
#[derive(Default)]
struct Scope {
    pins: OnceLock<Vec<Arc<Pin>>>,
    revision: AtomicU64,
    active: AtomicBool,
}
impl Scope {
    fn find(&self, id: &PCWSTR) -> Option<&Arc<Pin>> {
        if id.is_null() {
            return None;
        }
        // SAFETY: notification strings remain valid during the callback.
        let id = unsafe { id.as_wide() };
        self.pins.get()?.iter().find(|pin| pin.id.as_slice() == id)
    }
    fn changed(&self, id: &PCWSTR, invalidated: bool) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        if invalidated {
            if let Some(pin) = self.find(id) {
                pin.valid.store(false, Ordering::Release);
            }
        }
        advance(&self.revision);
    }
    fn intact(&self, revision: u64) -> bool {
        revision != u64::MAX
            && self.revision.load(Ordering::Acquire) == revision
            && self
                .pins
                .get()
                .is_some_and(|pins| pins.iter().all(|pin| pin.valid.load(Ordering::Acquire)))
    }
}
#[implement(IMMNotificationClient)]
struct ScopeCallback(Arc<Scope>);
#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for ScopeCallback_Impl {
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        self.0.changed(id, true);
        Ok(())
    }
    fn OnDeviceAdded(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.0.changed(id, false);
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> windows::core::Result<()> {
        self.0.changed(id, true);
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        id: &PCWSTR,
    ) -> windows::core::Result<()> {
        if flow == eRender && self.0.find(id).is_none() {
            self.0.changed(id, false);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        self.0.changed(id, false);
        Ok(())
    }
}
struct Registration {
    enumerator: IMMDeviceEnumerator,
    callback: IMMNotificationClient,
    scope: Arc<Scope>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.scope.active.store(false, Ordering::Release);
        // All endpoint callbacks have been unregistered before this apartment exits.
        if unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.callback)
        }
        .is_err()
        {
            tracing::warn!("Source render notification unregistration failed");
        }
    }
}
#[derive(Default)]
struct VolumeEvents {
    active: AtomicBool,
    muted: AtomicBool,
    external_revision: AtomicU64,
    generation: AtomicU64,
}
impl VolumeEvents {
    fn observe(&self, muted: bool, local: bool) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let previous = self.muted.swap(muted, Ordering::AcqRel);
        // Same-mute notifications include volume-only changes: they must not release hold.
        if !local && previous != muted {
            advance(&self.external_revision);
        }
        // All notifications invalidate native reconciliation, including our delayed
        // callbacks and volume-only events. Their payload is not a GetMute result.
        advance(&self.generation);
    }
}
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    events: Arc<VolumeEvents>,
    context: GUID,
}
#[allow(non_snake_case)]
impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if !data.is_null() {
            // SAFETY: only the fixed header is read during the native callback.
            let data = unsafe { &*data };
            self.events
                .observe(data.bMuted.as_bool(), data.guidEventContext == self.context);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteOutcome {
    Changed,
    Unchanged,
}
struct Ownership {
    original: bool,
    owned: bool,
    revision: u64,
}
impl Ownership {
    fn observe(&mut self, revision: u64) {
        if revision != self.revision {
            self.owned = false;
            self.revision = revision;
        }
    }
    fn applied(
        &mut self,
        outcome: WriteOutcome,
        before: u64,
        after: u64,
        muted: bool,
    ) -> Result<()> {
        self.owned = false;
        if before == u64::MAX || before != after || !muted {
            return Err(BackendError::OwnershipLost);
        }
        self.revision = after;
        // S_FALSE is not a write by us. It never creates a restoration right.
        self.owned = outcome == WriteOutcome::Changed && !self.original;
        Ok(())
    }
    fn release(&mut self, valid: bool, revision: u64, muted: bool) -> Result<bool> {
        let owned = std::mem::replace(&mut self.owned, false);
        if !owned || self.original {
            return Ok(false);
        }
        if !valid || revision == u64::MAX || revision != self.revision {
            return Err(BackendError::OwnershipLost);
        }
        Ok(muted)
    }
}
struct Endpoint {
    id: String,
    device: IMMDevice,
    volume: IAudioEndpointVolume,
    callback: IAudioEndpointVolumeCallback,
    events: Arc<VolumeEvents>,
    pin: Arc<Pin>,
    context: GUID,
    ownership: Ownership,
    qualified_generation: Option<u64>,
}
impl Endpoint {
    fn open(device: IMMDevice, id: String, pin: Arc<Pin>) -> Result<Self> {
        let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| map_error(error.into()))?;
        let context = unsafe { CoCreateGuid() }.map_err(|error| map_error(error.into()))?;
        let events = Arc::new(VolumeEvents::default());
        let initial = unsafe { volume.GetMute() }.map_err(|error| map_error(error.into()))?;
        events.muted.store(initial.as_bool(), Ordering::Release);
        events.active.store(true, Ordering::Release);
        let callback: IAudioEndpointVolumeCallback = VolumeCallback {
            events: events.clone(),
            context,
        }
        .into();
        unsafe { volume.RegisterControlChangeNotify(&callback) }
            .map_err(|error| map_error(error.into()))?;
        let mut endpoint = Self {
            id,
            device,
            volume,
            callback,
            events,
            pin,
            context,
            ownership: Ownership {
                original: true,
                owned: false,
                revision: 0,
            },
            qualified_generation: None,
        };
        let revision = endpoint.events.external_revision.load(Ordering::Acquire);
        let original = endpoint.read()?;
        if endpoint.events.external_revision.load(Ordering::Acquire) != revision {
            return Err(BackendError::OwnershipLost);
        }
        endpoint.events.muted.store(original, Ordering::Release);
        endpoint.ownership = Ownership {
            original,
            owned: false,
            revision,
        };
        Ok(endpoint)
    }
    fn read(&self) -> Result<bool> {
        unsafe { self.volume.GetMute() }
            .map(|value| value.as_bool())
            .map_err(|error| map_error(error.into()))
    }
    fn valid(&self) -> bool {
        self.pin.valid.load(Ordering::Acquire)
            && unsafe { self.device.GetState() }.is_ok_and(|state| state == DEVICE_STATE_ACTIVE)
    }
    fn set(&self, muted: bool) -> Result<WriteOutcome> {
        // The projection erases S_FALSE. Preserve the HRESULT to distinguish an owned write.
        let result = unsafe {
            (Interface::vtable(&self.volume).SetMute)(
                Interface::as_raw(&self.volume),
                muted.into(),
                &self.context,
            )
        };
        if result == S_OK {
            Ok(WriteOutcome::Changed)
        } else if result == S_FALSE {
            Ok(WriteOutcome::Unchanged)
        } else {
            Err(map_error(windows::core::Error::from_hresult(result).into()))
        }
    }
    fn hold(&mut self, scope: &Scope, revision: u64, admitted: &dyn Fn() -> bool) -> Result<()> {
        let generation = self.events.generation.load(Ordering::Acquire);
        let external = self.events.external_revision.load(Ordering::Acquire);
        if generation == u64::MAX || external == u64::MAX {
            return Err(BackendError::OwnershipLost);
        }
        let unchanged = external == self.ownership.revision;
        self.ownership.observe(external);
        // Only a native GetMute(true) qualifies this generation, never callback
        // payloads. A delayed/reordered notification always requires reconciliation.
        if self.qualified_generation == Some(generation)
            && unchanged
            && self.pin.valid.load(Ordering::Acquire)
        {
            return Ok(());
        }
        let initial = self.qualified_generation.is_none();
        let muted = self.read()?;
        if !self.pin.valid.load(Ordering::Acquire) || ((initial || !muted) && !self.valid()) {
            return Err(BackendError::DeviceGone);
        }
        if muted {
            self.events.muted.store(true, Ordering::Release);
            self.qualified_generation = Some(generation);
            return Ok(());
        }
        if !admitted() {
            return Err(BackendError::PermissionDenied);
        }
        if !scope.intact(revision) {
            return Err(BackendError::StaleRevision);
        }
        // A failed native write is ambiguous: never assume rollback ownership from its value.
        self.ownership.owned = false;
        let outcome = self.set(true)?;
        let current = self.read()?;
        let after = self.events.external_revision.load(Ordering::Acquire);
        self.events.muted.store(current, Ordering::Release);
        self.ownership.applied(outcome, external, after, current)?;
        if !self.valid() {
            self.ownership.owned = false;
            return Err(BackendError::DeviceGone);
        }
        self.qualified_generation = Some(generation);
        Ok(())
    }
    fn restore(&mut self) -> Result<()> {
        if !self.ownership.owned {
            return Ok(());
        }
        let revision = self.events.external_revision.load(Ordering::Acquire);
        // Consume the restoration right before a fallible read; never retry an uncertain release.
        if self.ownership.release(self.valid(), revision, true)? && self.read()? {
            // Callback revision and native identity are checked again at the final setter seam.
            if !self.pin.valid.load(Ordering::Acquire)
                || self.events.external_revision.load(Ordering::Acquire) != revision
            {
                return Err(BackendError::OwnershipLost);
            }
            self.set(false)?;
            if self.events.external_revision.load(Ordering::Acquire) != revision {
                return Err(BackendError::OwnershipLost);
            }
        }
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.events.active.store(false, Ordering::Release);
        if unsafe { self.volume.UnregisterControlChangeNotify(&self.callback) }.is_err() {
            tracing::warn!("Source render volume notification unregistration failed");
        }
    }
}
fn active_render(enumerator: &IMMDeviceEnumerator) -> Result<Vec<(String, IMMDevice)>> {
    let collection = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|error| map_error(error.into()))?;
    let count = unsafe { collection.GetCount() }.map_err(|error| map_error(error.into()))?;
    let mut devices = Vec::with_capacity(count as usize);
    for index in 0..count {
        let device = unsafe { collection.Item(index) }.map_err(|error| map_error(error.into()))?;
        let id = devices::device_id(&device).map_err(map_error)?;
        if id.is_empty() || id.len() > MAX_DEVICE_ID_BYTES || id.contains('\0') {
            return Err(BackendError::InvalidArgument);
        }
        devices.push((id, device));
    }
    devices.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Ok(devices)
}
struct Owner {
    endpoints: Vec<Endpoint>,
    registration: Registration,
    revision: u64,
}
impl Owner {
    fn open(admitted: &dyn Fn() -> bool) -> Result<Self> {
        if !admitted() {
            return Err(BackendError::PermissionDenied);
        }
        let enumerator = devices::enumerator().map_err(map_error)?;
        let scope = Arc::new(Scope::default());
        scope.active.store(true, Ordering::Release);
        let callback: IMMNotificationClient = ScopeCallback(scope.clone()).into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&callback) }
            .map_err(|error| map_error(error.into()))?;
        let registration = Registration {
            enumerator,
            callback,
            scope,
        };
        let revision = registration.scope.revision.load(Ordering::Acquire);
        let devices = active_render(&registration.enumerator)?;
        if devices.is_empty() {
            return Err(BackendError::DeviceGone);
        }
        let pins: Vec<_> = devices
            .iter()
            .map(|(id, _)| {
                Arc::new(Pin {
                    id: id.encode_utf16().collect(),
                    valid: AtomicBool::new(true),
                })
            })
            .collect();
        registration
            .scope
            .pins
            .set(pins.clone())
            .map_err(|_| BackendError::OwnershipLost)?;
        let mut owner = Self {
            endpoints: Vec::with_capacity(devices.len()),
            registration,
            revision,
        };
        for ((id, device), pin) in devices.into_iter().zip(pins) {
            if !admitted() {
                return Err(BackendError::PermissionDenied);
            }
            owner.endpoints.push(Endpoint::open(device, id, pin)?);
        }
        if !owner.registration.scope.intact(revision) {
            return Err(BackendError::StaleRevision);
        }
        Ok(owner)
    }
    fn check_scope(&mut self) -> Result<bool> {
        let scope = &self.registration.scope;
        if scope.intact(self.revision) {
            return Ok(true);
        }
        if scope
            .pins
            .get()
            .is_none_or(|pins| pins.iter().any(|pin| !pin.valid.load(Ordering::Acquire)))
        {
            return Err(BackendError::DeviceGone);
        }
        let revision = scope.revision.load(Ordering::Acquire);
        if revision == u64::MAX {
            return Err(BackendError::OwnershipLost);
        }
        let current = active_render(&self.registration.enumerator)?;
        if !current
            .iter()
            .map(|(id, _)| id)
            .eq(self.endpoints.iter().map(|endpoint| &endpoint.id))
        {
            return Err(BackendError::DeviceGone);
        }
        // Unrelated capture/property hints do not change the pinned render scope.
        self.revision = revision;
        Ok(scope.intact(revision))
    }
    fn hold(&mut self, admitted: &dyn Fn() -> bool) -> Result<()> {
        if !self.check_scope()? {
            return Err(BackendError::StaleRevision);
        }
        for endpoint in &mut self.endpoints {
            endpoint.hold(&self.registration.scope, self.revision, admitted)?;
        }
        if !admitted() {
            return Err(BackendError::PermissionDenied);
        }
        if !self.registration.scope.intact(self.revision) {
            return Err(BackendError::StaleRevision);
        }
        Ok(())
    }
    fn restore(&mut self) {
        for endpoint in self.endpoints.iter_mut().rev() {
            if let Err(error) = endpoint.restore() {
                // Same best-effort policy as Linux: uncertainty must not revoke unrelated P4/mic.
                tracing::warn!(
                    ?error,
                    "Source local playback restoration forfeited or failed (best effort)"
                );
            }
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.restore();
    }
}
#[derive(Default)]
struct Signals {
    stop: AtomicBool,
    error: AtomicU8,
    completed: AtomicBool,
}
impl Signals {
    fn fail(&self, error: BackendError) {
        let code = match error {
            BackendError::DeviceGone => 1,
            BackendError::OwnershipLost => 2,
            BackendError::PermissionDenied => 3,
            _ => 4,
        };
        self.error.store(code, Ordering::Release);
    }
}
struct AdmissionSlot {
    admitted: Admission,
    active: AtomicBool,
}
impl AdmissionSlot {
    fn admitted(&self) -> bool {
        if !self.active.load(Ordering::Acquire) {
            return false;
        }
        if !(self.admitted)() {
            // OFF/ReturnLocal cannot rearm an existing capture's lease.
            self.active.store(false, Ordering::Release);
            return false;
        }
        true
    }
}
struct Member {
    admission: Weak<AdmissionSlot>,
    ready: Option<mpsc::SyncSender<Result<()>>>,
}
impl Member {
    fn admitted(&self) -> bool {
        self.admission.upgrade().is_some_and(|slot| slot.admitted())
    }
    fn reply(&mut self, result: Result<()>) {
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(result);
        }
    }
}
fn any_admitted(members: &[Member]) -> bool {
    let mut admitted = false;
    for member in members {
        // Do not short-circuit: another eligible Source must not hide this
        // member's ReturnLocal edge and leave its old admission rearmable.
        admitted |= member.admitted();
    }
    admitted
}
#[derive(Default)]
struct PoolSlot {
    owner: Weak<SharedLease>,
    completion: Option<Arc<Signals>>,
}
impl PoolSlot {
    fn current(&self) -> Result<Option<Arc<SharedLease>>> {
        if let Some(owner) = self.owner.upgrade() {
            return Ok(Some(owner));
        }
        if self
            .completion
            .as_ref()
            .is_some_and(|state| !state.completed.load(Ordering::Acquire))
        {
            return Err(BackendError::Busy);
        }
        Ok(None)
    }
}
#[derive(Default)]
pub(crate) struct RenderLeasePool(Mutex<PoolSlot>);
struct SharedLease {
    signals: Arc<Signals>,
    attach: mpsc::SyncSender<Member>,
    worker: Option<JoinHandle<()>>,
}
pub(crate) struct SourceRenderLease {
    shared: Option<Arc<SharedLease>>,
    admission: Arc<AdmissionSlot>,
}
impl SourceRenderLease {
    pub(crate) fn acquire(pool: &RenderLeasePool, admitted: Admission) -> Result<Self> {
        if !admitted() {
            return Err(BackendError::PermissionDenied);
        }
        let admission = Arc::new(AdmissionSlot {
            admitted,
            active: AtomicBool::new(true),
        });
        let (ready, result) = mpsc::sync_channel(1);
        let member = Member {
            admission: Arc::downgrade(&admission),
            ready: Some(ready),
        };
        let mut pool = pool.0.lock().map_err(|_| BackendError::OwnershipLost)?;
        let shared = if let Some(shared) = pool.current()? {
            if let Some(error) = shared.error() {
                return Err(error);
            }
            shared
                .attach
                .send(member)
                .map_err(|_| BackendError::ServerUnavailable)?;
            shared.worker.as_ref().unwrap().thread().unpark();
            shared
        } else {
            let signals = Arc::new(Signals::default());
            // Install before creation/publication: strong count zero precedes Drop.
            pool.completion = Some(signals.clone());
            let state = signals.clone();
            // Only native admission is queued; endpoint ownership stays on one MTA.
            let (attach, incoming) = mpsc::sync_channel(1);
            let worker = thread::Builder::new()
                .name("bfa-render-hold".into())
                .spawn(move || {
                    let mut members = vec![member];
                    let run = |members: &mut Vec<Member>| -> Result<()> {
                        let _apartment = Apartment::mta().map_err(map_error)?;
                        let admitted = || any_admitted(members);
                        let mut owner = Owner::open(&admitted)?;
                        let mut released = false;
                        loop {
                            if state.stop.load(Ordering::Acquire) {
                                break;
                            }
                            members.extend(incoming.try_iter());
                            let admitted = || any_admitted(members);
                            if !admitted() && !released {
                                owner.restore();
                                released = true;
                            }
                            if !released {
                                match owner.hold(&admitted) {
                                    Err(_) if !admitted() => {
                                        owner.restore();
                                        released = true;
                                    }
                                    Err(BackendError::StaleRevision) => {
                                        thread::park_timeout(Duration::from_millis(25));
                                        continue;
                                    }
                                    Err(error) => {
                                        // Publish failure before best-effort COM retirement can block.
                                        state.fail(error);
                                        return Err(error);
                                    }
                                    Ok(()) => {}
                                }
                            } else if let Err(error) = owner.check_scope() {
                                state.fail(error);
                                return Err(error);
                            }
                            // A joining stream is Ready only after the actual owner's scope/
                            // held endpoints were checked, never merely after enqueueing it.
                            for member in members.iter_mut() {
                                if member.ready.is_none() {
                                    continue;
                                }
                                let result = if released {
                                    Err(BackendError::Busy)
                                } else if member.admitted() {
                                    Ok(())
                                } else {
                                    Err(BackendError::PermissionDenied)
                                };
                                member.reply(result);
                            }
                            members.retain(|member| member.admission.strong_count() != 0);
                            thread::park_timeout(Duration::from_millis(25));
                        }
                        Ok(())
                    };
                    if let Err(error) = run(&mut members) {
                        state.fail(error);
                        for member in &mut members {
                            member.reply(Err(error));
                        }
                    }
                })
                .map_err(|_| {
                    // A failed spawn created no owner/thread and cannot have native writes.
                    signals.completed.store(true, Ordering::Release);
                    BackendError::ServerUnavailable
                })?;
            let shared = Arc::new(SharedLease {
                signals,
                attach,
                worker: Some(worker),
            });
            pool.owner = Arc::downgrade(&shared);
            shared
        };
        drop(pool);
        let mut lease = Self {
            shared: Some(shared),
            admission,
        };
        match result.recv() {
            Ok(Ok(())) => Ok(lease),
            other => {
                lease.stop();
                match other {
                    Ok(Err(error)) => Err(error),
                    _ => Err(BackendError::ServerUnavailable),
                }
            }
        }
    }
    pub(crate) fn error(&self) -> Option<BackendError> {
        self.shared.as_ref().and_then(|shared| shared.error())
    }
    pub(crate) fn stop(&mut self) {
        self.admission.active.store(false, Ordering::Release);
        if let Some(shared) = self.shared.take() {
            shared.worker.as_ref().unwrap().thread().unpark();
            // The final capture owns retirement; an earlier session cannot unmute
            // an endpoint that another consented Source still needs held.
            drop(shared);
        }
    }
}
impl SharedLease {
    fn error(&self) -> Option<BackendError> {
        match self.signals.error.load(Ordering::Acquire) {
            1 => Some(BackendError::DeviceGone),
            2 => Some(BackendError::OwnershipLost),
            3 => Some(BackendError::PermissionDenied),
            4 => Some(BackendError::ServerUnavailable),
            _ if !self.signals.stop.load(Ordering::Acquire)
                && self.worker.as_ref().is_some_and(JoinHandle::is_finished) =>
            {
                Some(BackendError::ServerUnavailable)
            }
            _ => None,
        }
    }
}
impl Drop for SharedLease {
    fn drop(&mut self) {
        self.signals.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            if worker.join().is_ok() {
                // This is after native Owner/COM apartment destruction, not logical OFF.
                self.signals.completed.store(true, Ordering::Release);
            } else {
                self.signals.fail(BackendError::ServerUnavailable);
                tracing::warn!("Source silence worker panicked; native pool remains fenced");
            }
        }
    }
}
impl Drop for SourceRenderLease {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_owner_cannot_reacquire_until_actual_native_retirement_completes() {
        use audio_backend::{
            dry::{DryBackend, ScriptEvent},
            AudioFormat, Backend, Capabilities, CaptureSpec, Device, DeviceId, Direction, Stream,
        };
        let mut backend = DryBackend::new(Capabilities {
            system_capture: true,
            exclude_own_process_tree: true,
            endpoint_mute: true,
            media: false,
            global_ptt: false,
        });
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        let device = DeviceId::new("owned-render").unwrap();
        backend
            .apply(ScriptEvent::UpsertDevice(Device {
                id: device.clone(),
                label: "Owned fixture".into(),
                direction: Direction::Render,
                format,
            }))
            .unwrap();
        let mut stream = backend
            .start_capture(
                CaptureSpec::SystemExcludingOwnProcessTree { format },
                Box::new(|_| {}),
            )
            .unwrap();
        stream.suppress_local_playback(Arc::new(|| true)).unwrap();
        let signals = Arc::new(Signals::default());
        let state = signals.clone();
        let (entered, at_cleanup) = mpsc::sync_channel(1);
        let (release, withheld) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            while !state.stop.load(Ordering::Acquire) {
                thread::park();
            }
            entered.send(()).unwrap();
            withheld.recv().unwrap();
            stream.stop().unwrap();
        });
        let (attach, _incoming) = mpsc::sync_channel(1);
        let owner = Arc::new(SharedLease {
            signals: signals.clone(),
            attach,
            worker: Some(worker),
        });
        let pool = RenderLeasePool(Mutex::new(PoolSlot {
            owner: Arc::downgrade(&owner),
            completion: Some(signals.clone()),
        }));
        let retiring = thread::spawn(move || drop(owner));
        at_cleanup.recv_timeout(Duration::from_secs(2)).unwrap();
        // Production SharedLease::drop is now joining a real owned Dry stream's
        // withheld cleanup. Capture the actual native-pool decision before release.
        let admission = pool.0.lock().unwrap().current();
        let held = backend.render_mute_state(&device).unwrap();
        release.send(()).unwrap();
        retiring.join().unwrap();
        assert!(
            matches!(admission, Err(BackendError::Busy)),
            "an expired Weak is not completed native ownership"
        );
        assert_eq!(
            (held.0, held.1),
            (true, true),
            "rejected admission made no mute mutation"
        );
        assert!(signals.completed.load(Ordering::Acquire));
        assert!(pool.0.lock().unwrap().current().unwrap().is_none());
        assert!(!backend.render_mute_state(&device).unwrap().0);
        // A fresh source must snapshot CURRENT state, not the retired owner's false.
        backend
            .apply(ScriptEvent::SetMute {
                device: device.clone(),
                muted: true,
            })
            .unwrap();
        let mut fresh = backend
            .start_capture(
                CaptureSpec::SystemExcludingOwnProcessTree { format },
                Box::new(|_| {}),
            )
            .unwrap();
        fresh.suppress_local_playback(Arc::new(|| true)).unwrap();
        fresh.stop().unwrap();
        let restored = backend.render_mute_state(&device).unwrap();
        assert_eq!((restored.0, restored.1), (true, false));
    }

    #[test]
    fn panicked_native_owner_does_not_publish_completed_retirement() {
        let signals = Arc::new(Signals::default());
        let (attach, _incoming) = mpsc::sync_channel(1);
        let owner = Arc::new(SharedLease {
            signals: signals.clone(),
            attach,
            worker: Some(thread::spawn(|| panic!("owned native-retirement fixture"))),
        });
        let pool = RenderLeasePool(Mutex::new(PoolSlot {
            owner: Arc::downgrade(&owner),
            completion: Some(signals.clone()),
        }));
        drop(owner);
        assert!(!signals.completed.load(Ordering::Acquire));
        assert!(matches!(
            pool.0.lock().unwrap().current(),
            Err(BackendError::Busy)
        ));
    }

    fn ownership(original: bool) -> Ownership {
        Ownership {
            original,
            owned: false,
            revision: 0,
        }
    }
    #[test]
    fn return_local_latches_each_member_while_another_source_remains_admitted() {
        let first = Arc::new(AdmissionSlot {
            admitted: Arc::new(|| true),
            active: AtomicBool::new(true),
        });
        let hold = Arc::new(AtomicBool::new(true));
        let observed = hold.clone();
        let second = Arc::new(AdmissionSlot {
            admitted: Arc::new(move || observed.load(Ordering::Acquire)),
            active: AtomicBool::new(true),
        });
        let members = [
            Member {
                admission: Arc::downgrade(&first),
                ready: None,
            },
            Member {
                admission: Arc::downgrade(&second),
                ready: None,
            },
        ];
        assert!(any_admitted(&members));
        hold.store(false, Ordering::Release);
        assert!(
            any_admitted(&members),
            "the first Source still requires quiet"
        );
        hold.store(true, Ordering::Release);
        first.active.store(false, Ordering::Release);
        assert!(
            !any_admitted(&members),
            "the old returned member cannot reacquire"
        );
    }

    #[test]
    fn only_an_acknowledged_changed_write_can_be_restored() {
        let mut ours = ownership(false);
        ours.applied(WriteOutcome::Changed, 0, 0, true).unwrap();
        assert!(ours.release(true, 0, true).unwrap());
        assert!(!ours.release(true, 0, true).unwrap());
        let mut raced = ownership(false);
        raced.applied(WriteOutcome::Unchanged, 0, 0, true).unwrap();
        assert!(!raced.release(true, 0, true).unwrap());
        let mut pre_muted = ownership(true);
        pre_muted
            .applied(WriteOutcome::Changed, 0, 0, true)
            .unwrap();
        assert!(!pre_muted.release(true, 0, true).unwrap());
    }
    #[test]
    fn external_race_or_replaced_incarnation_cannot_authorize_unmute() {
        for (valid, revision) in [(false, 0), (true, 1)] {
            let mut lease = ownership(false);
            lease.applied(WriteOutcome::Changed, 0, 0, true).unwrap();
            assert_eq!(
                lease.release(valid, revision, true),
                Err(BackendError::OwnershipLost)
            );
            assert!(!lease.owned);
        }
        let mut lease = ownership(false);
        assert_eq!(
            lease.applied(WriteOutcome::Changed, 0, 1, true),
            Err(BackendError::OwnershipLost)
        );
        assert!(!lease.owned);
    }
    #[test]
    fn volume_only_callbacks_keep_hold_but_external_mute_transitions_revoke_it() {
        let events = VolumeEvents::default();
        events.active.store(true, Ordering::Release);
        events.observe(true, true);
        events.observe(true, false);
        assert_eq!(events.external_revision.load(Ordering::Acquire), 0);
        let mut lease = ownership(false);
        lease.applied(WriteOutcome::Changed, 0, 0, true).unwrap();
        events.observe(false, false);
        lease.observe(events.external_revision.load(Ordering::Acquire));
        assert!(!lease.release(true, 1, true).unwrap());
        events.active.store(false, Ordering::Release);
        events.observe(true, false);
        assert_eq!(events.external_revision.load(Ordering::Acquire), 1);
    }
    #[test]
    fn default_within_held_scope_does_not_revoke_but_incarnation_change_does() {
        let scope = Arc::new(Scope::default());
        scope.active.store(true, Ordering::Release);
        let id: Vec<u16> = "render-a".encode_utf16().chain(Some(0)).collect();
        let pin = Arc::new(Pin {
            id: id[..id.len() - 1].to_vec(),
            valid: AtomicBool::new(true),
        });
        scope.pins.set(vec![pin.clone()]).ok().unwrap();
        let callback: IMMNotificationClient = ScopeCallback(scope.clone()).into();
        unsafe {
            callback.OnDefaultDeviceChanged(
                eRender,
                windows::Win32::Media::Audio::eMultimedia,
                PCWSTR(id.as_ptr()),
            )
        }
        .unwrap();
        assert!(scope.intact(0));
        unsafe { callback.OnDeviceRemoved(PCWSTR(id.as_ptr())) }.unwrap();
        assert!(!scope.intact(0));
        assert!(!pin.valid.load(Ordering::Acquire));
    }
}
