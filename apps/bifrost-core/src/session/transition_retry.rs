//! Per-peer failed transition cooldowns; pointer hysteresis is enforced by the session.
use geometry::NodeId;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct TransitionRetry {
    failures: BTreeMap<NodeId, Failure>,
}
struct Failure {
    step: usize,
    until_ms: u64,
    reason: String,
}
impl TransitionRetry {
    pub(super) fn blocked(&self, peer: &NodeId, now_ms: u64) -> bool {
        self.failures
            .get(peer)
            .is_some_and(|failure| now_ms < failure.until_ms)
    }
    pub(super) fn fail(&mut self, peer: NodeId, now_ms: u64, reason: String) -> Option<u64> {
        const DELAYS: [u64; 3] = [250, 2_000, 10_000];
        let previous = self.failures.get(&peer).map(|failure| failure.step);
        let step = previous.map_or(0, |step| (step + 1).min(DELAYS.len() - 1));
        let delay = DELAYS[step];
        self.failures.insert(
            peer,
            Failure {
                step,
                until_ms: now_ms.saturating_add(delay),
                reason,
            },
        );
        (previous != Some(step)).then_some(delay)
    }
    pub(super) fn success(&mut self, peer: &NodeId) {
        self.failures.remove(peer);
    }
    pub(super) fn reasons(&self) -> impl Iterator<Item = (&NodeId, &str)> {
        self.failures
            .iter()
            .map(|(peer, failure)| (peer, failure.reason.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_refusals_are_bounded_and_success_resets_backoff() {
        let peer = NodeId::new([1; 32]);
        let mut retry = TransitionRetry::default();
        let mut attempts = 0;
        for now in 0..10_000 {
            if !retry.blocked(&peer, now) {
                attempts += 1;
                retry.fail(peer, now, "CaptureFailed".into());
            }
        }
        assert_eq!(attempts, 3);
        retry.success(&peer);
        assert!(!retry.blocked(&peer, 10_000));
        assert_eq!(retry.fail(peer, 10_000, "LeaseMismatch".into()), Some(250));
    }
    #[test]
    fn warnings_only_advance_with_the_backoff_step() {
        let peer = NodeId::new([1; 32]);
        let mut retry = TransitionRetry::default();
        assert_eq!(retry.fail(peer, 0, "refusal".into()), Some(250));
        assert_eq!(retry.fail(peer, 250, "refusal".into()), Some(2_000));
        assert_eq!(retry.fail(peer, 2_250, "refusal".into()), Some(10_000));
        assert_eq!(retry.fail(peer, 12_250, "refusal".into()), None);
    }
}
