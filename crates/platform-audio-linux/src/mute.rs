use crate::{
    backend::{lock, Entry, Key, State},
    raw::{self, Config, Connection, Raw},
};
use audio_backend::{BackendError, MuteGuard, MuteSnapshot};
use pulseaudio::{protocol as p, Client};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

const ECHO_WINDOW: Duration = Duration::from_millis(200);

struct Ownership {
    generation: u64,
    owned: bool,
    serial: u64,
    last_write: Option<(bool, Instant)>,
    checking: bool,
    echo_seen: bool,
}
impl Ownership {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            owned: true,
            serial: 0,
            last_write: None,
            echo_seen: false,
            checking: false,
        }
    }
    fn observe(&mut self, generation: u64, muted: bool, at: Instant) {
        let echo = self.last_write.is_some_and(|(written, when)| {
            written == muted
                && at
                    .checked_duration_since(when)
                    .is_some_and(|delta| delta <= ECHO_WINDOW)
        });
        if generation != self.generation || !echo || self.echo_seen {
            self.owned = false;
        } else {
            self.echo_seen = true;
        }
    }
}

#[derive(Default)]
pub(crate) struct MuteWatches(Mutex<BTreeMap<u32, Weak<Mutex<Ownership>>>>);
impl MuteWatches {
    fn reserve(&self, index: u32, generation: u64) -> Result<Arc<Mutex<Ownership>>, BackendError> {
        let mut watches = lock(&self.0);
        watches.retain(|_, watch| watch.strong_count() > 0);
        if watches.contains_key(&index) {
            return Err(BackendError::Busy);
        }
        let watch = Arc::new(Mutex::new(Ownership::new(generation)));
        watches.insert(index, Arc::downgrade(&watch));
        Ok(watch)
    }
    pub(crate) fn invalidate_all(&self) {
        for watch in lock(&self.0).values().filter_map(Weak::upgrade) {
            lock(&watch).owned = false;
        }
    }
    pub(crate) fn observe(
        &self,
        event: p::SubscriptionEvent,
        client: &Client,
    ) -> Option<(u32, bool)> {
        if event.event_facility != p::SubscriptionEventFacility::Source {
            return None;
        }
        let Some(index) = event.index else {
            self.invalidate_all();
            return None;
        };
        let watch = lock(&self.0).get(&index).and_then(Weak::upgrade);
        let at = Instant::now();
        let pending = watch.as_ref().map(|watch| {
            let mut ownership = lock(watch);
            // A late same-value change is external too, even before the query completes.
            if event.event_type != p::SubscriptionEventType::Changed
                || ownership
                    .last_write
                    .is_none_or(|(_, when)| at.duration_since(when) > ECHO_WINDOW)
            {
                ownership.owned = false;
            }
            ownership.checking = true;
            (ownership.generation, ownership.serial)
        });
        // Never hold a watch/catalog lock while calling Client or waiting for its reactor.
        let info = if event.event_type == p::SubscriptionEventType::Changed {
            raw::wait(client.source_info(index))
        } else {
            Err(BackendError::DeviceGone)
        };
        if let (Some(watch), Some((generation, serial))) = (watch, pending) {
            let mut ownership = lock(&watch);
            ownership.checking = false;
            if ownership.serial != serial {
                // A write raced this query; the old event cannot prove the new write.
                ownership.owned = false;
            } else {
                match &info {
                    Ok(info) if info.index == index => {
                        ownership.observe(generation, info.muted, at)
                    }
                    _ => ownership.owned = false,
                }
            }
        }
        info.ok()
            .filter(|info| info.index == index)
            .map(|info| (index, info.muted))
    }
}

/// Endpoint-wide mute with **best-effort ownership**, not an atomic Pulse compare-and-set.
/// Pulse does not identify writers. One matching Changed event within 200ms of our write is
/// accepted as its echo; indistinguishable external writes in that window cannot be detected.
/// Any other change, missing acknowledgement, removal or uncertainty forfeits restoration.
/// Keep the backend alive until the guard is released. Drop attempts restoration exactly once.
pub struct LinuxMuteGuard {
    state: Arc<State>,
    key: Key,
    entry: Entry,
    connection: Connection,
    raw: Raw,
    ownership: Option<Arc<Mutex<Ownership>>>,
    original: bool,
}
impl LinuxMuteGuard {
    /// Current local ownership belief, not a stronger native atomic ownership guarantee.
    pub fn owns_mute(&self) -> bool {
        self.state.valid(&self.key, self.entry.generation) && self.owned()
    }
    pub(crate) fn acquire(
        config: Arc<Config>,
        state: Arc<State>,
        key: Key,
        entry: Entry,
    ) -> Result<Self, BackendError> {
        let connection = config.client()?;
        let raw = config.raw()?;
        let ownership = state.mutes.reserve(entry.index, entry.generation)?;
        let mut guard = Self {
            state,
            key,
            entry,
            connection,
            raw,
            ownership: Some(ownership),
            original: false,
        };
        match guard.read() {
            Ok(snapshot) if guard.owned() => {
                guard.original = snapshot.muted;
                Ok(guard)
            }
            Ok(_) => {
                guard.ownership = None;
                Err(BackendError::OwnershipLost)
            }
            Err(error) => {
                guard.ownership = None;
                Err(error)
            }
        }
    }
    fn owned(&self) -> bool {
        self.ownership
            .as_ref()
            .is_some_and(|watch| lock(watch).owned)
    }
    fn read(&self) -> Result<MuteSnapshot, BackendError> {
        if !self.state.valid(&self.key, self.entry.generation) {
            return Err(BackendError::DeviceGone);
        }
        let info = raw::wait(
            self.connection
                .client
                .source_info_by_name(raw::name(self.entry.device.id.as_str())?),
        )?;
        if info.index != self.entry.index || !self.state.valid(&self.key, self.entry.generation) {
            return Err(BackendError::DeviceGone);
        }
        Ok(MuteSnapshot {
            endpoint: self.entry.device.id.clone(),
            generation: self.entry.generation,
            muted: info.muted,
        })
    }
    fn write(&mut self, muted: bool) -> Result<(), BackendError> {
        self.raw
            .ack(p::Command::SetSourceMute(p::SetDeviceMuteParams {
                device_index: Some(self.entry.index),
                device_name: None,
                mute: muted,
            }))
    }
}
impl MuteGuard for LinuxMuteGuard {
    fn original_muted(&self) -> bool {
        self.original
    }
    fn snapshot(&self) -> Result<MuteSnapshot, BackendError> {
        let snapshot = self.read();
        if let Some(watch) = &self.ownership {
            let mut ownership = lock(watch);
            if !snapshot.as_ref().is_ok_and(|snapshot| {
                ownership
                    .last_write
                    .map_or(self.original, |(written, _)| written)
                    == snapshot.muted
            }) {
                ownership.owned = false;
            }
        }
        snapshot
    }
    fn set_capture_mute(
        &mut self,
        expected_generation: u64,
        muted: bool,
    ) -> Result<(), BackendError> {
        let watch = self
            .ownership
            .as_ref()
            .ok_or(BackendError::Stopped)?
            .clone();
        if expected_generation != self.entry.generation {
            lock(&watch).owned = false;
            return Err(BackendError::OwnershipLost);
        }
        let snapshot = match self.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                lock(&watch).owned = false;
                return Err(error);
            }
        };
        {
            let mut ownership = lock(&watch);
            if !ownership.owned || ownership.checking {
                ownership.owned = false;
                return Err(BackendError::OwnershipLost);
            }
            // No native write means no echo to attribute, and no new restoration responsibility.
            if snapshot.muted == muted {
                return Ok(());
            }
            ownership.serial += 1;
            ownership.last_write = Some((muted, Instant::now()));
            ownership.echo_seen = false;
        }
        // The watcher can process the echo before this acknowledgement without lock inversion.
        if let Err(error) = self.write(muted) {
            lock(&watch).owned = false;
            return Err(error);
        }
        if !self.owned() {
            return Err(BackendError::OwnershipLost);
        }
        Ok(())
    }
    fn restore(&mut self) -> Result<(), BackendError> {
        let Some(watch) = self.ownership.take() else {
            return Ok(());
        };
        // Keep the reservation alive throughout restoration, but release it on every return path.
        let snapshot = self.read();
        let restore = {
            let mut ownership = lock(&watch);
            let valid = ownership.owned
                && !ownership.checking
                && (ownership.last_write.is_none() || ownership.echo_seen);
            ownership.owned = false;
            if !valid {
                return Err(BackendError::OwnershipLost);
            }
            let snapshot = snapshot?;
            match ownership.last_write {
                Some((written, _)) if written == snapshot.muted => snapshot.muted != self.original,
                Some(_) => return Err(BackendError::OwnershipLost),
                None => false,
            }
        };
        if restore {
            self.write(self.original)?;
        }
        Ok(())
    }
}
impl Drop for LinuxMuteGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn written(at: Instant) -> Ownership {
        let mut ownership = Ownership::new(7);
        ownership.last_write = Some((true, at));
        ownership
    }
    #[test]
    fn matching_echo_within_window_preserves_ownership() {
        let now = Instant::now();
        let mut ownership = written(now);
        ownership.observe(7, true, now + Duration::from_millis(199));
        assert!(ownership.owned && ownership.echo_seen);
    }
    #[test]
    fn different_value_same_value_after_window_and_generation_change_revoke() {
        let now = Instant::now();
        for (generation, muted, delta) in [(7, false, 1), (7, true, 201), (8, true, 1)] {
            let mut ownership = written(now);
            ownership.observe(generation, muted, now + Duration::from_millis(delta));
            assert!(!ownership.owned);
        }
    }
    #[test]
    fn duplicate_echo_and_event_before_write_are_uncertain() {
        let now = Instant::now();
        let mut ownership = written(now);
        ownership.observe(7, true, now);
        ownership.observe(7, true, now);
        assert!(!ownership.owned);
        let mut ownership = Ownership::new(7);
        ownership.observe(7, false, now);
        assert!(!ownership.owned);
    }
    #[test]
    fn one_guard_per_endpoint_and_reservation_releases_on_drop() {
        let watches = MuteWatches::default();
        let first = watches.reserve(1, 7).unwrap();
        assert!(matches!(watches.reserve(1, 7), Err(BackendError::Busy)));
        drop(first);
        assert!(watches.reserve(1, 8).is_ok());
    }
}
