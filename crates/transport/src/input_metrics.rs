//! Bounded duration diagnostics. No event contents or peer/lease identifiers are logged.
use tokio::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(10);
const ACK_CAPACITY: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationSnapshot {
    pub count: u64,
    pub unknown_count: u64,
    pub sum_us: u64,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub max_us: Option<u64>,
    pub over_50ms: u64,
    pub stall_total_us: u64,
}

#[derive(Debug, Clone)]
pub struct DurationHistogram {
    bins: [u64; 193],
    count: u64,
    unknown_count: u64,
    sum_us: u64,
    max_us: u64,
    over_50ms: u64,
    stall_total_us: u64,
}

impl Default for DurationHistogram {
    fn default() -> Self {
        Self {
            bins: [0; 193],
            count: 0,
            unknown_count: 0,
            sum_us: 0,
            max_us: 0,
            over_50ms: 0,
            stall_total_us: 0,
        }
    }
}

impl DurationHistogram {
    pub fn record(&mut self, duration_us: Option<u64>) {
        let Some(value) = duration_us else {
            self.unknown_count = self.unknown_count.saturating_add(1);
            return;
        };
        let index = if value == 0 {
            0
        } else {
            let power = 63 - value.leading_zeros() as usize;
            let base = 1u64 << power;
            let sub = ((u128::from(value - base) * 8) / u128::from(base)) as usize;
            (1 + power * 8 + sub).min(192)
        };
        self.bins[index] = self.bins[index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(value);
        self.max_us = self.max_us.max(value);
        if value > 50_000 {
            self.over_50ms = self.over_50ms.saturating_add(1);
            self.stall_total_us = self.stall_total_us.saturating_add(value);
        }
    }

    fn quantile(&self, percentile: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = (u128::from(self.count) * u128::from(percentile)).div_ceil(100);
        let mut seen = 0u128;
        for (index, count) in self.bins.iter().enumerate() {
            seen += u128::from(*count);
            if seen >= rank {
                return Some(if index == 0 {
                    0
                } else if index == 192 {
                    self.max_us
                } else {
                    let power = (index - 1) / 8;
                    let sub = (index - 1) % 8;
                    let base = 1u64 << power;
                    // Integer upper edge; low powers have empty sub-buckets.
                    (base + (base * (sub as u64 + 1)).div_ceil(8) - 1).min(self.max_us)
                });
            }
        }
        Some(self.max_us)
    }

    pub fn snapshot(&self) -> DurationSnapshot {
        DurationSnapshot {
            count: self.count,
            unknown_count: self.unknown_count,
            sum_us: self.sum_us,
            p50_us: self.quantile(50),
            p95_us: self.quantile(95),
            p99_us: self.quantile(99),
            max_us: (self.count != 0).then_some(self.max_us),
            over_50ms: self.over_50ms,
            stall_total_us: self.stall_total_us,
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Stage {
    CaptureToEnqueue,
    EnqueueToWrite,
    CaptureToWrite,
    TlsWrite,
    InboundDispatch,
    InjectAckRtt,
    CaptureToAck,
    TlsWriteFailed,
    TlsWriteTimeout,
}
const STAGES: [Stage; 9] = [
    Stage::CaptureToEnqueue,
    Stage::EnqueueToWrite,
    Stage::CaptureToWrite,
    Stage::TlsWrite,
    Stage::InboundDispatch,
    Stage::InjectAckRtt,
    Stage::CaptureToAck,
    Stage::TlsWriteFailed,
    Stage::TlsWriteTimeout,
];

#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub enum Counter {
    Sent,
    Acked,
    Failed,
    Coalesced,
    MotionOverwritten,
    MotionDiscardedOnLeaseEnd,
    QueueHighWater,
    CriticalOverflow,
    ClockUnknown,
    WriterIdleTimeout,
    WriterFrameTimeout,
    AckTrackingEvicted,
    AckUnmatched,
    AckPendingExpired,
}
const COUNTERS: [Counter; 14] = [
    Counter::Sent,
    Counter::Acked,
    Counter::Failed,
    Counter::Coalesced,
    Counter::MotionOverwritten,
    Counter::MotionDiscardedOnLeaseEnd,
    Counter::QueueHighWater,
    Counter::CriticalOverflow,
    Counter::ClockUnknown,
    Counter::WriterIdleTimeout,
    Counter::WriterFrameTimeout,
    Counter::AckTrackingEvicted,
    Counter::AckUnmatched,
    Counter::AckPendingExpired,
];

#[derive(Debug, Clone, Copy)]
pub struct InputMetricsSnapshot {
    pub window: [DurationSnapshot; 9],
    pub lifetime: [DurationSnapshot; 9],
    pub window_counts: [u64; 14],
    pub lifetime_counts: [u64; 14],
}

impl InputMetricsSnapshot {
    pub fn stage(&self, stage: Stage) -> (DurationSnapshot, DurationSnapshot) {
        (self.window[stage as usize], self.lifetime[stage as usize])
    }
    pub fn counter(&self, counter: Counter) -> (u64, u64) {
        (
            self.window_counts[counter as usize],
            self.lifetime_counts[counter as usize],
        )
    }

    /// Emit detached aggregates only: callers must release the admission/writer lock first.
    pub fn publish(&self, final_snapshot: bool) {
        for stage in STAGES {
            let (window, lifetime) = self.stage(stage);
            if window.count == 0
                && window.unknown_count == 0
                && (!final_snapshot || (lifetime.count == 0 && lifetime.unknown_count == 0))
            {
                continue;
            }
            tracing::info!(
                ?stage,
                final_snapshot,
                ?window,
                ?lifetime,
                "input duration metrics"
            );
        }
        for counter in COUNTERS {
            let (window_count, lifetime_count) = self.counter(counter);
            if window_count == 0 && (!final_snapshot || lifetime_count == 0) {
                continue;
            }
            tracing::info!(
                ?counter,
                window_count,
                lifetime_count,
                final_snapshot,
                "input counter metrics"
            );
        }
    }
}

#[derive(Debug)]
pub struct InputMetrics {
    window: [DurationHistogram; 9],
    lifetime: [DurationHistogram; 9],
    window_counts: [u64; 14],
    lifetime_counts: [u64; 14],
    window_started: Instant,
}
impl Default for InputMetrics {
    fn default() -> Self {
        Self {
            window: std::array::from_fn(|_| DurationHistogram::default()),
            lifetime: std::array::from_fn(|_| DurationHistogram::default()),
            window_counts: [0; 14],
            lifetime_counts: [0; 14],
            window_started: Instant::now(),
        }
    }
}
impl InputMetrics {
    pub fn record(&mut self, stage: Stage, duration_us: u64) {
        self.window[stage as usize].record(Some(duration_us));
        self.lifetime[stage as usize].record(Some(duration_us));
    }
    pub fn unknown(&mut self, stage: Stage) {
        self.window[stage as usize].record(None);
        self.lifetime[stage as usize].record(None);
        self.increment(Counter::ClockUnknown, 1);
    }
    pub fn increment(&mut self, counter: Counter, value: u64) {
        let i = counter as usize;
        if matches!(counter, Counter::QueueHighWater) {
            self.window_counts[i] = self.window_counts[i].max(value);
            self.lifetime_counts[i] = self.lifetime_counts[i].max(value);
        } else {
            self.window_counts[i] = self.window_counts[i].saturating_add(value);
            self.lifetime_counts[i] = self.lifetime_counts[i].saturating_add(value);
        }
    }
    pub fn snapshot(&self) -> InputMetricsSnapshot {
        InputMetricsSnapshot {
            window: std::array::from_fn(|i| self.window[i].snapshot()),
            lifetime: std::array::from_fn(|i| self.lifetime[i].snapshot()),
            window_counts: self.window_counts,
            lifetime_counts: self.lifetime_counts,
        }
    }
    /// Rotate under the short metrics lock without invoking any synchronous log sink.
    pub fn take_publication(
        &mut self,
        now: Instant,
        final_snapshot: bool,
    ) -> Option<InputMetricsSnapshot> {
        if !final_snapshot && now.saturating_duration_since(self.window_started) < WINDOW {
            return None;
        }
        let snapshot = self.snapshot();
        let durations = if final_snapshot {
            &snapshot.lifetime
        } else {
            &snapshot.window
        };
        let counts = if final_snapshot {
            &snapshot.lifetime_counts
        } else {
            &snapshot.window_counts
        };
        let nonempty = durations
            .iter()
            .any(|stage| stage.count != 0 || stage.unknown_count != 0)
            || counts.iter().any(|count| *count != 0);
        self.window = std::array::from_fn(|_| DurationHistogram::default());
        self.window_counts = [0; 14];
        self.window_started = now;
        nonempty.then_some(snapshot)
    }
}

#[derive(Clone, Copy)]
struct AckEntry {
    generation: u64,
    lease: [u8; 16],
    seq: u64,
    started: Instant,
    capture_age_us: Option<u64>,
}

// Do not expose lease/sequence identifiers through a containing handle's Debug.
pub struct AckTracker {
    entries: [Option<AckEntry>; ACK_CAPACITY],
    pub evicted: u64,
    pub unmatched: u64,
    pub pending_expired: u64,
}

impl std::fmt::Debug for AckTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AckTracker")
            .field("pending_count", &self.len())
            .field("evicted", &self.evicted)
            .field("unmatched", &self.unmatched)
            .field("pending_expired", &self.pending_expired)
            .finish()
    }
}
impl Default for AckTracker {
    fn default() -> Self {
        Self {
            entries: [None; ACK_CAPACITY],
            evicted: 0,
            unmatched: 0,
            pending_expired: 0,
        }
    }
}
impl AckTracker {
    pub fn len(&self) -> usize {
        self.entries.iter().filter(|entry| entry.is_some()).count()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
    pub fn expire(&mut self, now: Instant) {
        for slot in &mut self.entries {
            if slot.is_some_and(|entry| now.saturating_duration_since(entry.started) >= WINDOW) {
                *slot = None;
                self.pending_expired = self.pending_expired.saturating_add(1);
            }
        }
    }
    pub fn insert(
        &mut self,
        generation: u64,
        lease: [u8; 16],
        seq: u64,
        started: Instant,
        capture_age_us: Option<u64>,
    ) {
        self.expire(started);
        // A duplicate registration cannot move write-start or revive an old sample.
        if self
            .entries
            .iter()
            .flatten()
            .any(|e| e.generation == generation && e.lease == lease && e.seq == seq)
        {
            return;
        }
        let index = self
            .entries
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                self.evicted = self.evicted.saturating_add(1);
                self.entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, e)| e.as_ref().map(|e| e.started))
                    .map_or(0, |(i, _)| i)
            });
        self.entries[index] = Some(AckEntry {
            generation,
            lease,
            seq,
            started,
            capture_age_us,
        });
    }
    pub fn acknowledge(
        &mut self,
        generation: u64,
        lease: [u8; 16],
        seq: u64,
        now: Instant,
    ) -> Option<(u64, Option<u64>)> {
        self.expire(now);
        let index = self.entries.iter().position(|slot| {
            slot.is_some_and(|e| e.generation == generation && e.lease == lease && e.seq == seq)
        });
        let Some(index) = index else {
            self.unmatched = self.unmatched.saturating_add(1);
            return None;
        };
        let entry = self.entries[index].take()?;
        let elapsed = u64::try_from(now.checked_duration_since(entry.started)?.as_micros()).ok()?;
        Some((
            elapsed,
            entry
                .capture_age_us
                .and_then(|age| age.checked_add(elapsed)),
        ))
    }
    pub fn clear_lease(&mut self, generation: u64, lease: [u8; 16]) {
        for slot in &mut self.entries {
            if slot.is_some_and(|e| e.generation == generation && e.lease == lease) {
                *slot = None;
            }
        }
    }
    pub fn clear(&mut self) {
        self.entries.fill(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantiles_are_bounded_conservative_and_unknown_is_not_zero() {
        let mut h = DurationHistogram::default();
        h.record(None);
        assert_eq!(h.snapshot().p50_us, None);
        for value in 0..100_000 {
            h.record(Some(value));
        }
        let s = h.snapshot();
        assert_eq!(s.count, 100_000);
        assert_eq!(s.unknown_count, 1);
        assert!((49_999..=56_249).contains(&s.p50_us.unwrap()));
        assert!(s.p95_us.unwrap() >= 94_999);
        assert!(s.p99_us.unwrap() >= 98_999);
        assert_eq!(s.over_50ms, 49_999);
        h.record(Some(u64::MAX));
        h.record(Some(u64::MAX));
        assert_eq!(h.sum_us, u64::MAX);
        assert_eq!(h.bins.len(), 193);
        assert_eq!(h.snapshot().max_us, Some(u64::MAX));
    }
    #[test]
    fn window_rotates_but_lifetime_remains() {
        let mut m = InputMetrics::default();
        let start = m.window_started;
        m.record(Stage::TlsWrite, 100);
        assert!(m
            .take_publication(start + Duration::from_secs(9), false)
            .is_none());
        assert_eq!(m.snapshot().stage(Stage::TlsWrite).0.count, 1);
        assert!(m.take_publication(start + WINDOW, false).is_some());
        assert_eq!(m.snapshot().stage(Stage::TlsWrite).0.p50_us, None);
        assert_eq!(m.snapshot().stage(Stage::TlsWrite).1.count, 1);
    }

    #[test]
    fn empty_idle_windows_are_not_published_but_final_totals_remain() {
        let mut metrics = InputMetrics::default();
        let start = metrics.window_started;
        assert!(metrics.take_publication(start + WINDOW, false).is_none());
        assert!(metrics.take_publication(start + WINDOW, true).is_none());
        metrics.record(Stage::TlsWrite, 80);
        metrics.increment(Counter::Sent, 1);
        let publication = metrics.take_publication(start + WINDOW * 2, false).unwrap();
        assert_eq!(publication.stage(Stage::TlsWrite).0.count, 1);
        assert_eq!(publication.counter(Counter::Sent), (1, 1));
        assert!(metrics
            .take_publication(start + WINDOW * 3, false)
            .is_none());
        let final_totals = metrics.take_publication(start + WINDOW * 3, true).unwrap();
        assert_eq!(final_totals.stage(Stage::TlsWrite).0.count, 0);
        assert_eq!(final_totals.stage(Stage::TlsWrite).1.count, 1);
        assert_eq!(final_totals.counter(Counter::Sent), (0, 1));
    }

    #[test]
    fn histogram_upper_edges_cover_integer_samples_and_overflow_tail() {
        for value in [
            0,
            1,
            2,
            3,
            7,
            8,
            9,
            15,
            16,
            17,
            50_000,
            50_001,
            16_777_216,
            u64::MAX,
        ] {
            let mut histogram = DurationHistogram::default();
            histogram.record(Some(value));
            assert_eq!(histogram.snapshot().p99_us, Some(value));
        }
        let mut metrics = InputMetrics::default();
        metrics.increment(Counter::Sent, u64::MAX);
        metrics.increment(Counter::Sent, 1);
        metrics.increment(Counter::QueueHighWater, 50);
        metrics.increment(Counter::QueueHighWater, 2);
        assert_eq!(
            metrics.snapshot().counter(Counter::Sent),
            (u64::MAX, u64::MAX)
        );
        assert_eq!(
            metrics.snapshot().counter(Counter::QueueHighWater),
            (50, 50)
        );
    }

    #[test]
    fn ack_unknown_capture_duplicate_registration_and_checked_capture_age() {
        let start = Instant::now();
        let mut tracker = AckTracker::default();
        tracker.insert(1, [2; 16], 1, start, None);
        tracker.insert(1, [2; 16], 1, start + Duration::from_micros(10), Some(999));
        assert_eq!(tracker.len(), 1);
        assert_eq!(
            tracker.acknowledge(1, [2; 16], 1, start + Duration::from_micros(20)),
            Some((20, None))
        );
        tracker.insert(1, [2; 16], 2, start, Some(u64::MAX));
        assert_eq!(
            tracker.acknowledge(1, [2; 16], 2, start + Duration::from_micros(1)),
            Some((1, None))
        );
        tracker.insert(1, [2; 16], 3, start, Some(0));
        assert_eq!(tracker.acknowledge(1, [2; 16], 3, start + WINDOW), None);
        assert_eq!(tracker.pending_expired, 1);
        tracker.clear();
        assert!(tracker.is_empty());
    }
    #[test]
    fn ack_before_completion_duplicate_and_wrong_generation() {
        let start = Instant::now();
        let mut t = AckTracker::default();
        t.insert(1, [2; 16], 3, start, Some(40));
        assert_eq!(t.acknowledge(2, [2; 16], 3, start), None);
        assert_eq!(t.acknowledge(1, [4; 16], 3, start), None);
        assert_eq!(
            t.acknowledge(1, [2; 16], 3, start + Duration::from_micros(60)),
            Some((60, Some(100)))
        );
        assert_eq!(t.acknowledge(1, [2; 16], 3, start), None);
        assert_eq!(t.unmatched, 3);
    }
    #[test]
    fn tracker_is_bounded_expires_and_clears_only_requested_lease() {
        let start = Instant::now();
        let mut t = AckTracker::default();
        for seq in 0..2049 {
            t.insert(1, [2; 16], seq, start + Duration::from_micros(seq), None);
        }
        assert_eq!(t.len(), 2048);
        assert_eq!(t.evicted, 1);
        t.clear_lease(2, [2; 16]);
        assert_eq!(t.len(), 2048);
        t.expire(start + WINDOW + Duration::from_secs(1));
        assert!(t.is_empty());
        assert_eq!(t.pending_expired, 2048);
        t.insert(1, [2; 16], 1, start, None);
        t.clear_lease(1, [2; 16]);
        assert!(t.is_empty());
    }
}
