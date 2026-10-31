//! Revalidate native grants after the bounded worker queue; expiry does not wait for native I/O.
use super::*;
#[derive(Default)]
pub(crate) struct Forwarder {
    held: Option<(Instant, u64)>,
}
impl Forwarder {
    pub(crate) fn expire(&mut self, now: Instant) -> Option<w::PttEvent> {
        if self.held.is_some_and(|(deadline, _)| now >= deadline) {
            self.held = None;
            Some(released())
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
            return Some(event);
        }
        let Some(host_ns) = host_ns else {
            return self.clear();
        };
        if let Some(released) = self.expire(now) {
            return Some(released);
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
        self.held.take().map(|_| released())
    }
}
fn released() -> w::PttEvent {
    w::PttEvent {
        kind: 3,
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
        assert!(forward.accept(released(), None, now).is_some());
    }
}
