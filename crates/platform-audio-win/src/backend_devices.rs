//! Lazy device control bridge. All native work follows an explicit control operation.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::mpsc::TryRecvError,
    sync::{Arc, Mutex, MutexGuard},
};

use crate::{
    backend::map_error,
    devices::{self, CaptureMute, DeviceFlow, DeviceMonitor, DeviceRole, EndpointInfo},
};
use audio_backend::{
    AudioFormat, BackendError, DefaultRole, Defaults, Device, DeviceId, DeviceSelection, Direction,
    Event, MuteGuard, MuteSnapshot, EVENT_CAPACITY, MAX_DEVICE_LABEL_BYTES,
};

type Result<T, E = BackendError> = std::result::Result<T, E>;

struct Endpoint {
    generation: u64,
    mute: Option<CaptureMute>,
    leased: bool,
}

#[derive(Default)]
struct State {
    monitor: Option<DeviceMonitor>,
    endpoints: BTreeMap<DeviceId, Endpoint>,
    next_generation: u64,
    revocation: u64,
    events: VecDeque<Event>,
    resync: bool,
}

impl State {
    fn start(&mut self) -> Result<()> {
        if self.monitor.is_none() {
            self.monitor = Some(devices::watch_devices(EVENT_CAPACITY).map_err(map_error)?);
        }
        self.pump();
        Ok(())
    }

    fn revoke(&mut self) {
        self.revocation = self.revocation.saturating_add(1);
        self.events.clear();
        self.resync = true;
    }

    fn emit(&mut self, event: Event) {
        if self.events.len() == EVENT_CAPACITY {
            self.revoke();
        } else {
            self.events.push_back(event);
        }
    }

    fn pump(&mut self) {
        let overflow = self
            .monitor
            .as_ref()
            .is_some_and(DeviceMonitor::take_overflowed);
        if overflow {
            // Lost removal/state hints mean no endpoint incarnation can be trusted.
            self.endpoints.clear();
            self.revoke();
        }
        for _ in 0..EVENT_CAPACITY {
            let event = self
                .monitor
                .as_ref()
                .map(|monitor| monitor.events.try_recv());
            match event {
                Some(Ok(event)) => self.device_event(event),
                Some(Err(TryRecvError::Disconnected)) => {
                    self.endpoints.clear();
                    self.revoke();
                    break;
                }
                _ => break,
            }
        }
        let mut translated: [Option<Event>; EVENT_CAPACITY] = std::array::from_fn(|_| None);
        let mut translated_count = 0;
        let mut overflow = false;
        for (id, endpoint) in &mut self.endpoints {
            let Some(mute) = &endpoint.mute else { continue };
            overflow |= mute.take_overflowed();
            for _ in 0..EVENT_CAPACITY {
                match mute.changes.try_recv() {
                    Ok(change) => {
                        if translated_count < EVENT_CAPACITY {
                            translated[translated_count] = Some(Event::MuteChanged(MuteSnapshot {
                                endpoint: id.clone(),
                                generation: endpoint.generation,
                                muted: change.muted,
                            }));
                            translated_count += 1;
                        } else {
                            overflow = true;
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        overflow = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
        }
        if overflow {
            self.revoke();
        }
        for event in translated.into_iter().flatten() {
            self.emit(event);
        }
    }

    fn device_event(&mut self, event: devices::DeviceEvent) {
        use audio_backend::DeviceEvent as Out;
        use devices::DeviceEvent as In;
        let translated = match event {
            In::DefaultChanged { flow, role, .. } => {
                let role = match role {
                    DeviceRole::Multimedia => DefaultRole::Multimedia,
                    DeviceRole::Communications => DefaultRole::Communications,
                    DeviceRole::Console => return,
                };
                Out::DefaultChanged {
                    direction: direction(flow),
                    role,
                }
            }
            In::Added { id } => match DeviceId::new(id) {
                Ok(id) => Out::Added(id),
                Err(_) => {
                    self.revoke();
                    return;
                }
            },
            In::Removed { id } => match DeviceId::new(id) {
                Ok(id) => {
                    self.endpoints.remove(&id);
                    Out::Removed(id)
                }
                Err(_) => {
                    self.revoke();
                    return;
                }
            },
            In::StateChanged { id, .. } => match DeviceId::new(id) {
                Ok(id) => {
                    self.endpoints.remove(&id);
                    Out::Invalidated(id)
                }
                Err(_) => {
                    self.revoke();
                    return;
                }
            },
            In::PropertyChanged { id } => match DeviceId::new(id) {
                Ok(id) => Out::FormatChanged(id),
                Err(_) => {
                    self.revoke();
                    return;
                }
            },
        };
        self.emit(Event::Device(translated));
    }

    fn enumerate(&mut self) -> Result<Vec<EndpointInfo>> {
        self.start()?;
        let native = devices::enumerate_devices().map_err(map_error)?;
        let ids: std::collections::BTreeSet<_> = native
            .iter()
            .map(|endpoint| DeviceId::new(endpoint.id.clone()))
            .collect::<Result<_>>()?;
        self.endpoints.retain(|id, _| ids.contains(id));
        for id in ids {
            if !self.endpoints.contains_key(&id) {
                self.next_generation = self
                    .next_generation
                    .checked_add(1)
                    .ok_or(BackendError::Busy)?;
                self.endpoints.insert(
                    id,
                    Endpoint {
                        generation: self.next_generation,
                        mute: None,
                        leased: false,
                    },
                );
            }
        }
        // Notifications received during enumeration must invalidate this snapshot's incarnations.
        self.pump();
        Ok(native)
    }

    fn resolve_capture(&mut self, selection: &DeviceSelection) -> Result<DeviceId> {
        let endpoints = self.enumerate()?;
        let endpoint = endpoints
            .iter()
            .find(|endpoint| {
                endpoint.flow == DeviceFlow::Capture
                    && match selection {
                        DeviceSelection::Explicit(id) => endpoint.id == id.as_str(),
                        DeviceSelection::FollowDefault(DefaultRole::Multimedia) => {
                            endpoint.default_roles.multimedia
                        }
                        DeviceSelection::FollowDefault(DefaultRole::Communications) => {
                            endpoint.default_roles.communications
                        }
                    }
            })
            .ok_or(BackendError::DeviceGone)?;
        let id = DeviceId::new(endpoint.id.clone())?;
        let entry = self
            .endpoints
            .get_mut(&id)
            .ok_or(BackendError::DeviceGone)?;
        if entry.mute.is_none() {
            entry.mute = Some(CaptureMute::open(id.as_str(), EVENT_CAPACITY).map_err(map_error)?);
        }
        Ok(id)
    }

    fn endpoint(&self, id: &DeviceId, generation: u64) -> Result<&Endpoint> {
        self.endpoints
            .get(id)
            .filter(|endpoint| endpoint.generation == generation)
            .ok_or(BackendError::DeviceGone)
    }
}

pub(crate) struct BackendDevices {
    state: Arc<Mutex<State>>,
}

impl BackendDevices {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub(crate) fn devices(&self) -> Result<Vec<Device>> {
        lock(&self.state)?
            .enumerate()?
            .into_iter()
            .map(|endpoint| {
                let mut label = endpoint.friendly_name;
                let mut end = label.len().min(MAX_DEVICE_LABEL_BYTES);
                while !label.is_char_boundary(end) {
                    end -= 1;
                }
                label.truncate(end);
                let device = Device {
                    id: DeviceId::new(endpoint.id)?,
                    label,
                    direction: direction(endpoint.flow),
                    format: AudioFormat {
                        sample_rate: endpoint.mix_format.sample_rate,
                        channels: endpoint.mix_format.channels,
                    },
                };
                device.validate()?;
                Ok(device)
            })
            .collect()
    }

    pub(crate) fn defaults(&self) -> Result<Defaults> {
        let mut defaults = Defaults::default();
        for endpoint in lock(&self.state)?.enumerate()? {
            let id = DeviceId::new(endpoint.id)?;
            for (selected, target) in [
                (endpoint.default_roles.multimedia, &mut defaults.multimedia),
                (
                    endpoint.default_roles.communications,
                    &mut defaults.communications,
                ),
            ] {
                if selected {
                    match endpoint.flow {
                        DeviceFlow::Capture => target.capture = Some(id.clone()),
                        DeviceFlow::Render => target.render = Some(id.clone()),
                    }
                }
            }
        }
        Ok(defaults)
    }

    pub(crate) fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot> {
        let mut state = lock(&self.state)?;
        let id = state.resolve_capture(selection)?;
        let endpoint = state.endpoints.get(&id).ok_or(BackendError::DeviceGone)?;
        Ok(MuteSnapshot {
            endpoint: id,
            generation: endpoint.generation,
            muted: endpoint
                .mute
                .as_ref()
                .ok_or(BackendError::DeviceGone)?
                .mute()
                .map_err(map_error)?,
        })
    }

    pub(crate) fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<WindowsMuteGuard> {
        let mut state = lock(&self.state)?;
        let id = state.resolve_capture(selection)?;
        let revocation = state.revocation;
        let endpoint = state
            .endpoints
            .get_mut(&id)
            .ok_or(BackendError::DeviceGone)?;
        if endpoint.leased {
            return Err(BackendError::Busy);
        }
        let mute = endpoint.mute.as_ref().ok_or(BackendError::DeviceGone)?;
        let revision = mute.external_revision();
        let original = mute.mute().map_err(map_error)?;
        if revision == u64::MAX || mute.external_revision() != revision {
            return Err(BackendError::OwnershipLost);
        }
        endpoint.leased = true;
        Ok(WindowsMuteGuard {
            state: Arc::clone(&self.state),
            id,
            generation: endpoint.generation,
            revocation,
            revision,
            ownership: Ownership {
                original,
                owned: false,
                active: true,
                revoked: false,
            },
        })
    }

    pub(crate) fn poll_event(&mut self) -> Option<Event> {
        let mut state = self.state.lock().ok()?;
        state.pump();
        if std::mem::take(&mut state.resync) {
            Some(Event::ResyncRequired)
        } else {
            state.events.pop_front()
        }
    }
}

fn lock(state: &Mutex<State>) -> Result<MutexGuard<'_, State>> {
    state.lock().map_err(|_| BackendError::OwnershipLost)
}

fn direction(flow: DeviceFlow) -> Direction {
    match flow {
        DeviceFlow::Capture => Direction::Capture,
        DeviceFlow::Render => Direction::Render,
    }
}

struct Ownership {
    original: bool,
    owned: bool,
    active: bool,
    revoked: bool,
}

impl Ownership {
    fn authorize(&mut self, intact: bool, current: bool, requested: bool) -> Result<bool> {
        if !self.active {
            return Err(BackendError::Stopped);
        }
        if !intact || self.revoked {
            self.owned = false;
            self.revoked = true;
            return Err(BackendError::OwnershipLost);
        }
        if current == requested {
            return Ok(false);
        }
        if !requested && (self.original || !self.owned) {
            return Err(BackendError::PermissionDenied);
        }
        Ok(true)
    }

    fn restoration(&mut self, intact: bool, current: bool) -> Result<bool> {
        if !std::mem::replace(&mut self.active, false) {
            return Ok(false);
        }
        if !intact || self.revoked {
            self.owned = false;
            self.revoked = true;
            return Err(BackendError::OwnershipLost);
        }
        Ok(self.owned && current != self.original)
    }
}

pub struct WindowsMuteGuard {
    state: Arc<Mutex<State>>,
    id: DeviceId,
    generation: u64,
    revocation: u64,
    revision: u64,
    ownership: Ownership,
}

impl MuteGuard for WindowsMuteGuard {
    fn original_muted(&self) -> bool {
        self.ownership.original
    }

    fn snapshot(&self) -> Result<MuteSnapshot> {
        let mut state = lock(&self.state)?;
        state.pump();
        let endpoint = state.endpoint(&self.id, self.generation)?;
        Ok(MuteSnapshot {
            endpoint: self.id.clone(),
            generation: self.generation,
            muted: endpoint
                .mute
                .as_ref()
                .ok_or(BackendError::DeviceGone)?
                .mute()
                .map_err(map_error)?,
        })
    }

    fn set_capture_mute(&mut self, expected_generation: u64, muted: bool) -> Result<()> {
        if !self.ownership.active {
            return Err(BackendError::Stopped);
        }
        if expected_generation != self.generation {
            return Err(BackendError::StaleRevision);
        }
        let mut state = lock(&self.state)?;
        state.pump();
        let endpoint = state.endpoint(&self.id, self.generation)?;
        let mute = endpoint.mute.as_ref().ok_or(BackendError::DeviceGone)?;
        let current = mute.mute().map_err(map_error)?;
        let intact = state.revocation == self.revocation
            && self.revocation != u64::MAX
            && mute.external_revision() == self.revision
            && self.revision != u64::MAX;
        if self.ownership.authorize(intact, current, muted)? {
            // An error is ambiguous: never retry restoration after an unproven native write.
            self.ownership.owned = false;
            self.ownership.revoked = true;
            match mute
                .set_mute_if_revision(muted, self.revision)
                .map_err(map_error)?
            {
                true => {
                    self.ownership.owned = true;
                    self.ownership.revoked = false;
                }
                false => return Err(BackendError::OwnershipLost),
            }
        }
        Ok(())
    }

    fn restore(&mut self) -> Result<()> {
        if !self.ownership.active {
            return Ok(());
        }
        // Release the lease on every exit, including native read and ownership failures.
        self.ownership.active = false;
        let mut state = lock(&self.state)?;
        state.pump();
        let revocation = state.revocation;
        let endpoint = state
            .endpoints
            .get_mut(&self.id)
            .filter(|endpoint| endpoint.generation == self.generation)
            .ok_or(BackendError::DeviceGone)?;
        endpoint.leased = false;
        let mute = endpoint.mute.as_ref().ok_or(BackendError::DeviceGone)?;
        let current = mute.mute().map_err(map_error)?;
        let intact = revocation == self.revocation
            && self.revocation != u64::MAX
            && mute.external_revision() == self.revision
            && self.revision != u64::MAX;
        self.ownership.active = true;
        if self.ownership.restoration(intact, current)? {
            if !mute
                .set_mute_if_revision(self.ownership.original, self.revision)
                .map_err(map_error)?
            {
                return Err(BackendError::OwnershipLost);
            }
        }
        Ok(())
    }
}

impl Drop for WindowsMuteGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(original: bool) -> Ownership {
        Ownership {
            original,
            owned: false,
            active: true,
            revoked: false,
        }
    }

    #[test]
    fn preserves_preexisting_mute() {
        let mut owner = lease(true);
        assert_eq!(
            owner.authorize(true, true, false),
            Err(BackendError::PermissionDenied)
        );
        assert_eq!(owner.authorize(true, true, true), Ok(false));
        assert_eq!(owner.restoration(true, true), Ok(false));
    }

    #[test]
    fn only_restores_owned_changes() {
        let mut owner = lease(false);
        assert_eq!(owner.restoration(true, true), Ok(false));
        let mut owner = lease(false);
        assert_eq!(owner.authorize(true, false, true), Ok(true));
        owner.owned = true;
        assert_eq!(owner.restoration(true, true), Ok(true));
        assert_eq!(owner.restoration(true, true), Ok(false));
        assert_eq!(
            owner.authorize(true, false, true),
            Err(BackendError::Stopped)
        );
    }

    #[test]
    fn same_value_external_write_or_overflow_revokes_ownership() {
        let mut owner = lease(false);
        owner.owned = true;
        assert_eq!(
            owner.authorize(false, true, true),
            Err(BackendError::OwnershipLost)
        );
        assert!(!owner.owned);
        assert_eq!(
            owner.authorize(true, true, true),
            Err(BackendError::OwnershipLost)
        );
        assert_eq!(
            owner.restoration(false, true),
            Err(BackendError::OwnershipLost)
        );
        assert!(!owner.active);
    }

    #[test]
    fn event_overflow_is_bounded_and_revokes_all_leases() {
        let mut state = State::default();
        let id = DeviceId::new("capture").unwrap();
        for _ in 0..=EVENT_CAPACITY {
            state.emit(Event::Device(audio_backend::DeviceEvent::Added(id.clone())));
        }
        assert!(state.resync);
        assert_eq!(state.revocation, 1);
        assert!(state.events.is_empty());
    }

    #[test]
    fn new_is_inert_and_poll_does_not_subscribe() {
        let mut bridge = BackendDevices::new();
        assert!(bridge.poll_event().is_none());
        assert!(bridge.state.lock().unwrap().monitor.is_none());
    }
}
