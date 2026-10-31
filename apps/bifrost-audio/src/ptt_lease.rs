//! Revalidate native grants after the bounded worker queue; expiry does not wait for native I/O.
use super::*;
#[derive(Default)]
pub(crate) struct Forwarder {
    held: Option<(Instant, u64)>,
    native_fence_ns: Option<u64>,
    native_blocked: bool,
    revoked_through: u64,
}
impl Forwarder {
    pub(crate) fn fence(&mut self, native_fence_ns: u64, revoked_through: u64) {
        self.held = None;
        self.native_fence_ns = Some(native_fence_ns);
        self.native_blocked = true;
        self.revoked_through = self.revoked_through.max(revoked_through);
    }
    pub(crate) fn expire(&mut self, now: Instant) -> Option<w::PttEvent> {
        if self.held.is_some_and(|(deadline, _)| now >= deadline) {
            self.held = None;
            Some(released(1))
        } else {
            None
        }
    }
    pub(crate) fn accept(
        &mut self,
        event: w::PttEvent,
        host_ns: Option<u64>,
        now: Instant,
    ) -> Option<w::PttEvent> {
        // Remote talk grants have a separate guard-owning native timer. Do not
        // conflate them with the physical-shortcut observation lease.
        if event.grant_id != 0 {
            if event.grant_id <= self.revoked_through && matches!(event.kind, 1 | 2 | 5) {
                return Some(w::PttEvent {
                    grant_id: event.grant_id,
                    ..released(2)
                });
            }
            if matches!(event.kind, 1 | 2)
                && host_ns.is_none_or(|clock| event.expires_at_ns <= clock)
            {
                return Some(w::PttEvent {
                    kind: 3,
                    grant_id: event.grant_id,
                    reason: 1,
                    ..Default::default()
                });
            }
            return Some(event);
        }
        if event.kind == 3 {
            self.held = None;
            if event.reason == 8 {
                self.native_blocked = false;
            } else if !matches!(event.reason, 1..=4 | 6 | 7) {
                return Some(released(7));
            }
            return Some(event);
        }
        let Some(host_ns) = host_ns else {
            return self.clear();
        };
        if let Some(released) = self.expire(now) {
            return Some(released);
        }
        if event.kind == 1
            && (self.native_blocked
                || self
                    .native_fence_ns
                    .is_some_and(|fence| event.observed_at_ns <= fence))
        {
            // A pre-fence key-up queued before an old press cannot rearm that held press.
            self.native_blocked = true;
            return None;
        }
        let remaining = event
            .expires_at_ns
            .checked_sub(host_ns)
            .filter(|remaining| (1..=PTT_LEASE_NS).contains(remaining));
        match event.kind {
            1 if event.observed_at_ns <= host_ns && remaining.is_some() => {
                self.held = Some((
                    now + Duration::from_nanos(remaining.unwrap()),
                    event.expires_at_ns,
                ));
                Some(event)
            }
            2 if self
                .held
                .is_some_and(|(_, expiry)| event.expires_at_ns >= expiry)
                && remaining.is_some() =>
            {
                self.held = Some((
                    now + Duration::from_nanos(remaining.unwrap()),
                    event.expires_at_ns,
                ));
                Some(event)
            }
            _ => self.clear(),
        }
    }
    fn clear(&mut self) -> Option<w::PttEvent> {
        self.held.take().map(|_| released(7))
    }
}
fn released(reason: i32) -> w::PttEvent {
    w::PttEvent {
        kind: 3,
        reason,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_grants_expire_and_late_renewals_never_reopen() {
        let mut forward = Forwarder::default();
        let now = Instant::now();
        assert!(forward
            .accept(
                w::PttEvent {
                    kind: 1,
                    observed_at_ns: 0,
                    expires_at_ns: PTT_LEASE_NS,
                    ..Default::default()
                },
                Some(0),
                now
            )
            .is_some());
        assert!(forward.expire(now + Duration::from_millis(249)).is_none());
        assert_eq!(
            forward
                .expire(now + Duration::from_millis(250))
                .unwrap()
                .kind,
            3
        );
        assert!(forward
            .accept(
                w::PttEvent {
                    kind: 2,
                    observed_at_ns: 0,
                    expires_at_ns: PTT_LEASE_NS * 2,
                    ..Default::default()
                },
                Some(PTT_LEASE_NS),
                now + Duration::from_millis(250)
            )
            .is_none());
        assert!(forward
            .accept(
                w::PttEvent {
                    kind: 1,
                    observed_at_ns: 0,
                    expires_at_ns: PTT_LEASE_NS,
                    ..Default::default()
                },
                Some(PTT_LEASE_NS),
                now
            )
            .is_none());
    }
    #[test]
    fn clock_failure_releases_but_configuration_acknowledgments_survive() {
        let now = Instant::now();
        let mut forward = Forwarder::default();
        forward.accept(
            w::PttEvent {
                kind: 1,
                observed_at_ns: 0,
                expires_at_ns: PTT_LEASE_NS,
                ..Default::default()
            },
            Some(0),
            now,
        );
        assert_eq!(
            forward
                .accept(
                    w::PttEvent {
                        kind: 2,
                        observed_at_ns: 0,
                        expires_at_ns: PTT_LEASE_NS,
                        ..Default::default()
                    },
                    None,
                    now
                )
                .unwrap()
                .kind,
            3
        );
        assert!(forward.accept(released(2), None, now).is_some());
    }

    #[test]
    fn only_physical_release_rearms_fenced_native_events_and_old_remote_ids_stay_revoked() {
        let mut forward = Forwarder::default();
        let now = Instant::now();
        forward.fence(100, 7);
        let press = |observed| w::PttEvent {
            kind: 1,
            observed_at_ns: observed,
            expires_at_ns: 1_000_000,
            ..Default::default()
        };
        assert!(forward.accept(press(200), Some(200), now).is_none());
        for reason in [1, 2, 7] {
            assert!(forward.accept(released(reason), Some(200), now).is_some());
            assert!(forward.accept(press(200), Some(200), now).is_none());
        }
        assert!(forward.accept(released(8), Some(200), now).is_some());
        assert!(forward.accept(press(50), Some(200), now).is_none());
        assert!(forward.accept(press(201), Some(201), now).is_none());
        assert!(forward.accept(released(8), Some(201), now).is_some());
        assert_eq!(forward.accept(press(202), Some(202), now).unwrap().kind, 1);
        let old = w::PttEvent {
            grant_id: 7,
            ..press(202)
        };
        let revoked = forward.accept(old, Some(202), now).unwrap();
        assert_eq!((revoked.grant_id, revoked.kind, revoked.reason), (7, 3, 2));
        let fresh = w::PttEvent {
            grant_id: 8,
            ..press(202)
        };
        assert_eq!(forward.accept(fresh, Some(202), now).unwrap().kind, 1);
    }
}
