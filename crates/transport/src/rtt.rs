//! Round-trip latency and jitter of one link, measured on its `Ping`/`Pong` pairs.
//!
//! A link sends `Ping { nonce }` once per second (`docs/05-protocol.md`: «`Ping` каждую 1 s по
//! каждому линку») and the peer answers with the same nonce. Pairing the reply with the instant
//! its `Ping` was written is the whole measurement; the statistics live here, apart from the
//! socket, because the arithmetic is the part worth testing and it needs no I/O.
//!
//! # What is reported
//!
//! * **p95** — the nearest-rank 95th percentile of the RTTs in the window: sort a copy (at most
//!   [`WINDOW_CAPACITY`] values) and take `ceil(0.95 · n)`. Not an interpolation: a percentile
//!   of a sample this small is a statement about *sampled* packets, and the nearest rank is
//!   the value an actual packet had.
//! * **jitter** — the mean absolute difference of consecutive RTTs in arrival order
//!   (`Σ |rtt[i] − rtt[i−1]| / (n − 1)`), in microseconds. This is the plain "how much does the
//!   delay move between two packets" number; RFC 3550's smoothed `J` was not chosen because
//!   with one probe per second over 30 s it would mostly report its own filter constant.
//!
//! Fewer than [`MIN_SAMPLES`] samples in the window report zero for both: the caller's
//! convention is that zero means *unknown* (the GUI hides it, `Status.rtt_p95_us` documents
//! itself as best-effort), and two samples are not a distribution.
//!
//! # Why time is a parameter
//!
//! Every method takes the monotonic instant of the event. The socket uses [`Instant::now`];
//! the tests pass a synthetic clock, so the window expiry and the stale-reply rule are asserted
//! without a single sleep.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How many pings may be in flight at once.
///
/// The cadence is one per second, so eight covers a peer that stops answering for eight seconds
/// — longer than the link's own offline window (`policy::LinkLiveness::offline_ms`, 3 s), which
/// means a live link never loses a pending nonce to this bound.
pub const MAX_OUTSTANDING: usize = 8;

/// A `Pong` for a `Ping` older than this is late, not a measurement: the link's liveness
/// window is three seconds, so anything this old belongs to a connection that was already
/// declared dead (or to a peer replaying a very old nonce).
pub const STALE_REPLY: Duration = Duration::from_secs(10);

/// The sliding window the statistics describe: «за последние 30 с».
pub const WINDOW: Duration = Duration::from_secs(30);

/// Ring capacity for the window. The 1 s cadence fits thirty samples; the extra room costs
/// nothing and keeps a burst of ad-hoc probes from evicting a second of history.
pub const WINDOW_CAPACITY: usize = 64;

/// Below this many samples both statistics are unknown (reported as zero).
pub const MIN_SAMPLES: usize = 3;

/// What one link measured, in microseconds.
///
/// `p95_us == 0` means *unknown*, not «instantaneous»: a zero-microsecond round trip does not
/// happen, and both the IPC `Status` and the GUI already read zero that way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RttSnapshot {
    /// Nearest-rank 95th percentile of the RTTs in the window, microseconds; 0 when unknown.
    pub p95_us: u32,
    /// Mean absolute difference of consecutive RTTs, microseconds; 0 when unknown.
    pub jitter_us: u32,
    /// How many samples the window held. Less than [`MIN_SAMPLES`] means the two fields above
    /// are zero because nothing is known, not because the link is fast.
    pub samples: u32,
}

impl RttSnapshot {
    /// `true` when the window held enough answered pings for the two values to mean something.
    pub const fn is_known(self) -> bool {
        self.samples >= MIN_SAMPLES as u32
    }
}

/// One RTT sample: when its `Pong` arrived and how long the round trip took.
#[derive(Debug, Clone, Copy)]
struct Sample {
    at: Instant,
    rtt: Duration,
}

/// The measurement state of one connection.
///
/// A meter belongs to a connection: the link task owns one for its lifetime, so a reconnect
/// starts from an empty window and old latency never outlives the socket it described.
#[derive(Debug, Default)]
pub struct RttMeter {
    /// The pings whose `Pong` has not arrived yet, oldest first: `(nonce, sent at)`.
    outstanding: VecDeque<(u64, Instant)>,
    /// The samples inside the window, in arrival order.
    samples: VecDeque<Sample>,
}

impl RttMeter {
    /// Records that a `Ping` with `nonce` was written at `at` and waits for its `Pong`.
    ///
    /// The oldest pending ping is forgotten when [`MAX_OUTSTANDING`] is exceeded: a peer that
    /// answers them out of order cannot make the map grow, and the reply to the evicted one is
    /// then simply unknown (and ignored).
    pub fn sent(&mut self, nonce: u64, at: Instant) {
        self.outstanding.push_back((nonce, at));
        while self.outstanding.len() > MAX_OUTSTANDING {
            self.outstanding.pop_front();
        }
    }

    /// Pairs a `Pong` with its `Ping` and records the sample.
    ///
    /// Returns the measured RTT, or `None` when the nonce is unknown — which covers all three
    /// cases the wire can produce: a duplicate `Pong` (its nonce is already consumed), a late
    /// one (older than [`STALE_REPLY`]), and one for a ping this link never sent.
    pub fn received(&mut self, nonce: u64, at: Instant) -> Option<Duration> {
        let index = self
            .outstanding
            .iter()
            .position(|(pending, _)| *pending == nonce)?;
        let (_, sent_at) = self.outstanding.remove(index)?;
        // A clock that went backwards (or a synthetic one in a test) yields zero, not a panic.
        let rtt = at.checked_duration_since(sent_at).unwrap_or_default();
        if rtt > STALE_REPLY {
            return None;
        }
        self.push_sample(Sample { at, rtt });
        Some(rtt)
    }

    /// The statistics of the last [`WINDOW`], as of `now`.
    pub fn snapshot(&self, now: Instant) -> RttSnapshot {
        let live = self
            .samples
            .iter()
            .filter(|sample| within_window(**sample, now))
            .collect::<Vec<_>>();
        let samples = u32::try_from(live.len()).unwrap_or(u32::MAX);
        if live.len() < MIN_SAMPLES {
            return RttSnapshot {
                p95_us: 0,
                jitter_us: 0,
                samples,
            };
        }

        let mut sorted = live.iter().map(|sample| sample.rtt).collect::<Vec<_>>();
        sorted.sort_unstable();
        // Nearest rank: ceil(0.95 · n), then the value at that 1-based position.
        let rank = (sorted.len() * 95).div_ceil(100).max(1);
        let p95 = sorted[rank - 1];

        let total = live
            .windows(2)
            .map(|pair| {
                pair[1]
                    .rtt
                    .abs_diff(pair[0].rtt)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64
            })
            .sum::<u64>();
        let pairs = (live.len() - 1) as u64;
        let jitter_us = (total + pairs / 2) / pairs;

        RttSnapshot {
            p95_us: micros(p95),
            jitter_us: u32::try_from(jitter_us).unwrap_or(u32::MAX),
            samples,
        }
    }

    /// How many pings are still waiting for a `Pong` (diagnostics and tests).
    pub fn outstanding(&self) -> usize {
        self.outstanding.len()
    }

    fn push_sample(&mut self, sample: Sample) {
        self.samples.push_back(sample);
        while self.samples.len() > WINDOW_CAPACITY {
            self.samples.pop_front();
        }
        let now = sample.at;
        while self
            .samples
            .front()
            .is_some_and(|front| !within_window(*front, now))
        {
            self.samples.pop_front();
        }
    }
}

/// `true` while `sample` is no older than [`WINDOW`] at `now`.
fn within_window(sample: Sample, now: Instant) -> bool {
    now.checked_duration_since(sample.at).unwrap_or_default() <= WINDOW
}

/// A duration in microseconds, saturating into the wire's `uint32`.
fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic clock: the tests move it by hand and never sleep.
    struct Clock {
        now: Instant,
    }

    impl Clock {
        fn new() -> Self {
            Self {
                now: Instant::now(),
            }
        }

        fn after(&mut self, duration: Duration) -> Instant {
            self.now += duration;
            self.now
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn a_pong_is_paired_with_its_ping_by_nonce() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        let first = clock.now;
        meter.sent(7, first);
        let second = clock.after(ms(2));
        meter.sent(8, second);

        // Out of order on the wire, still each its own nonce's round trip.
        assert_eq!(meter.received(8, second + ms(5)), Some(ms(5)));
        assert_eq!(meter.received(7, first + ms(12)), Some(ms(12)));
        assert_eq!(meter.outstanding(), 0);
    }

    #[test]
    fn unknown_duplicate_and_late_pongs_are_ignored() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        let sent = clock.now;
        meter.sent(1, sent);

        assert_eq!(meter.received(99, clock.after(ms(1))), None, "never sent");
        assert_eq!(meter.received(1, clock.after(ms(1))), Some(ms(2)));
        assert_eq!(
            meter.received(1, clock.after(ms(1))),
            None,
            "a duplicate changes nothing"
        );
        assert_eq!(meter.snapshot(clock.now).samples, 1);

        let sent = clock.now;
        meter.sent(2, sent);
        assert_eq!(
            meter.received(2, sent + STALE_REPLY + ms(1)),
            None,
            "a reply past the stale bound is not a measurement"
        );
        assert_eq!(meter.snapshot(sent + STALE_REPLY + ms(1)).samples, 1);
    }

    #[test]
    fn the_outstanding_map_is_bounded_and_drops_the_oldest_ping() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        for nonce in 0..(MAX_OUTSTANDING as u64 + 2) {
            meter.sent(nonce, clock.after(ms(1)));
        }
        assert_eq!(meter.outstanding(), MAX_OUTSTANDING);
        let now = clock.now;
        assert_eq!(
            meter.received(0, now),
            None,
            "the evicted oldest ping can no longer be measured"
        );
        assert_eq!(
            meter.received(MAX_OUTSTANDING as u64 + 1, clock.after(ms(1))),
            Some(ms(1))
        );
    }

    #[test]
    fn fewer_than_three_samples_report_unknown() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        for nonce in 0..2 {
            let sent = clock.now;
            meter.sent(nonce, sent);
            let _ = meter.received(nonce, clock.after(ms(5)));
        }
        assert_eq!(
            meter.snapshot(clock.now),
            RttSnapshot {
                p95_us: 0,
                jitter_us: 0,
                samples: 2,
            }
        );
    }

    #[test]
    fn p95_is_the_nearest_rank_and_jitter_the_mean_consecutive_difference() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        // Twenty RTTs, 1 ms apart: 1..=20 ms. Nearest rank for 0.95 · 20 = 19 is the 19th
        // value, 19 ms — the maximum (20) is only the top 5 %, not the 95th percentile.
        for value in 1..=20u64 {
            let sent = clock.now;
            meter.sent(value, sent);
            let _ = meter.received(value, clock.after(ms(value)));
        }
        let snapshot = meter.snapshot(clock.now);
        assert_eq!(snapshot.p95_us, 19_000);
        assert_eq!(snapshot.samples, 20);
        assert_eq!(snapshot.jitter_us, 1_000);

        // A steady link has no jitter even though it has an RTT.
        let mut steady = RttMeter::default();
        let mut clock = Clock::new();
        for nonce in 0..5 {
            let sent = clock.now;
            steady.sent(nonce, sent);
            let _ = steady.received(nonce, clock.after(ms(4)));
        }
        assert_eq!(
            steady.snapshot(clock.now),
            RttSnapshot {
                p95_us: 4_000,
                jitter_us: 0,
                samples: 5,
            }
        );
    }

    #[test]
    fn samples_older_than_the_window_leave_the_statistics() {
        let mut clock = Clock::new();
        let mut meter = RttMeter::default();
        // Three samples early on.
        for nonce in 0..3 {
            let sent = clock.now;
            meter.sent(nonce, sent);
            let _ = meter.received(nonce, clock.after(ms(1)));
        }
        assert_eq!(meter.snapshot(clock.now).samples, 3);

        // One more, thirty seconds later: the first three have expired and one is not enough.
        clock.after(WINDOW);
        let sent = clock.now;
        meter.sent(3, sent);
        let _ = meter.received(3, clock.after(ms(2)));
        assert_eq!(
            meter.snapshot(clock.now),
            RttSnapshot {
                p95_us: 0,
                jitter_us: 0,
                samples: 1,
            }
        );
        assert_eq!(
            meter.snapshot(clock.now + WINDOW).samples,
            1,
            "a sample exactly one window old still counts at the boundary"
        );
        assert_eq!(
            meter.snapshot(clock.now + WINDOW + ms(1)).samples,
            0,
            "past the boundary it is out"
        );
    }

    #[test]
    fn a_new_connection_starts_from_an_empty_window() {
        let mut clock = Clock::new();
        let mut previous = RttMeter::default();
        for nonce in 0..10 {
            let sent = clock.now;
            previous.sent(nonce, sent);
            let _ = previous.received(nonce, clock.after(ms(3)));
        }
        assert!(previous.snapshot(clock.now).p95_us > 0);

        // The reconnect spawns a link with a new meter: nothing is carried over.
        let next = RttMeter::default();
        assert_eq!(next.snapshot(clock.now), RttSnapshot::default());
        assert_eq!(next.outstanding(), 0);
    }
}
