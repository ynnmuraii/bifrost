//! The wall clock next to the monotonic one: the seam that makes a suspend visible.
//!
//! Everything the daemons time is measured on [`std::time::Instant`] — `CLOCK_MONOTONIC`
//! on Linux — and that clock **does not advance while the machine is suspended**, while
//! [`std::time::SystemTime`] does. A resume therefore shows up as wall time having jumped
//! far ahead of monotonic time, which is what [`SuspendDetector`] recognises. A heartbeat
//! that "went quiet" across such a jump was frozen with the machine, not lost: the laptop
//! that slept is the case `docs/03-architecture.md` scopes to «Сон, блокировка, отказы»
//! and `docs/05-protocol.md` to «Просыпание после suspend всегда отменяет lease, даже
//! если ОС monotonic clock не учитывал время сна».
//!
//! The [`Watchdog`](crate::watchdog::Watchdog) of this crate stays sans-IO and still takes
//! `now_ms` from its caller; [`Clock`] is where the callers — `lws-core`'s supervision and
//! `lws-agent`'s session — get that reading from, together with the wall reading that tells
//! the two apart. [`ManualClock`] is the other implementation: a test advances it by hand
//! instead of sleeping.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Monotonic and wall-clock milliseconds: the two readings a suspend is visible in.
///
/// The trait is the seam that keeps the decision types pure — the restart circuit and the
/// agent heartbeat are driven by `u64` milliseconds, so a test hands them a
/// [`ManualClock`] and never sleeps.
pub trait Clock {
    /// Monotonic milliseconds since this clock's origin.
    ///
    /// Frozen while the machine is suspended (`CLOCK_MONOTONIC`, `Instant`).
    fn monotonic_ms(&self) -> u64;

    /// Wall-clock milliseconds since the Unix epoch.
    ///
    /// Keeps running across a suspend (`CLOCK_REALTIME`, `SystemTime`).
    fn wall_ms(&self) -> u64;
}

/// The process clock: one `Instant` origin, plus `SystemTime`.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    /// The monotonic origin, shared with the caller's own deadlines.
    origin: Instant,
}

impl SystemClock {
    /// A clock whose monotonic origin is now.
    pub fn new() -> Self {
        Self::from_origin(Instant::now())
    }

    /// The same, with the monotonic origin given: a loop that already times its
    /// deadlines from one `Instant` keeps them on that scale.
    pub fn from_origin(origin: Instant) -> Self {
        Self { origin }
    }

    /// The monotonic origin, so deadlines can be built on the same scale.
    pub fn origin(&self) -> Instant {
        self.origin
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    fn wall_ms(&self) -> u64 {
        // A wall clock set before 1970 is the only way this fails, and it is not worth
        // a panic in a daemon: the reading falls back to the epoch.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_millis() as u64)
    }
}

/// A clock a test moves by hand.
///
/// [`ManualClock::advance`] is real time passing (both readings move);
/// [`ManualClock::suspend`] is the machine asleep (only wall time moves).
#[derive(Debug, Default, Clone, Copy)]
pub struct ManualClock {
    monotonic_ms: u64,
    wall_ms: u64,
}

impl ManualClock {
    /// A clock at zero on both scales.
    pub fn new() -> Self {
        Self::default()
    }

    /// Real time passes: both readings move by `ms`.
    pub fn advance(&mut self, ms: u64) {
        self.monotonic_ms = self.monotonic_ms.saturating_add(ms);
        self.wall_ms = self.wall_ms.saturating_add(ms);
    }

    /// The machine is suspended for `ms`: wall time moves, the monotonic clock does not.
    pub fn suspend(&mut self, ms: u64) {
        self.wall_ms = self.wall_ms.saturating_add(ms);
    }
}

impl Clock for ManualClock {
    fn monotonic_ms(&self) -> u64 {
        self.monotonic_ms
    }

    fn wall_ms(&self) -> u64 {
        self.wall_ms
    }
}

/// Recognises a suspend from the divergence between the two clocks.
///
/// One sample per call; the divergence is what wall time did that the monotonic clock did
/// not. Ordinary operation keeps the two within scheduling jitter of each other, so
/// anything above the threshold is time this process spent not running.
#[derive(Debug, Clone, Copy)]
pub struct SuspendDetector {
    last_monotonic_ms: u64,
    last_wall_ms: u64,
    threshold_ms: u64,
}

impl SuspendDetector {
    /// A detector whose first sample is `(monotonic_ms, wall_ms)`; a divergence of at
    /// least `threshold_ms` is a suspend.
    pub fn new(monotonic_ms: u64, wall_ms: u64, threshold_ms: u64) -> Self {
        Self {
            last_monotonic_ms: monotonic_ms,
            last_wall_ms: wall_ms,
            threshold_ms,
        }
    }

    /// Samples both clocks and returns the wall time the monotonic clock did not account
    /// for, when that is at least the threshold.
    ///
    /// `None` is ordinary operation. `Some(ms)` means the machine was asleep — or this
    /// process was not running — for `ms` of wall time, and every deadline the caller
    /// measured on the monotonic clock was frozen with it.
    pub fn observe(&mut self, monotonic_ms: u64, wall_ms: u64) -> Option<u64> {
        let monotonic_delta = monotonic_ms.saturating_sub(self.last_monotonic_ms);
        let wall_delta = wall_ms.saturating_sub(self.last_wall_ms);
        self.last_monotonic_ms = monotonic_ms;
        self.last_wall_ms = wall_ms;
        // A wall clock stepped *backwards* (a correction, not a suspend) saturates to
        // zero here and is never reported as a suspend.
        let unaccounted_ms = wall_delta.saturating_sub(monotonic_delta);
        (unaccounted_ms >= self.threshold_ms).then_some(unaccounted_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_time_keeps_both_clocks_together() {
        let mut clock = ManualClock::new();
        let mut detector = SuspendDetector::new(clock.monotonic_ms(), clock.wall_ms(), 2_000);
        clock.advance(50);
        assert_eq!(
            detector.observe(clock.monotonic_ms(), clock.wall_ms()),
            None
        );
        clock.advance(10_000);
        assert_eq!(
            detector.observe(clock.monotonic_ms(), clock.wall_ms()),
            None,
            "ten seconds of real time is not a suspend"
        );
    }

    #[test]
    fn a_suspend_is_the_wall_clock_moving_alone() {
        let mut clock = ManualClock::new();
        let mut detector = SuspendDetector::new(clock.monotonic_ms(), clock.wall_ms(), 2_000);
        clock.advance(50);
        assert_eq!(
            detector.observe(clock.monotonic_ms(), clock.wall_ms()),
            None
        );
        // The laptop slept for two minutes and ten seconds: wall time runs, monotonic
        // time is frozen.
        clock.suspend(130_000);
        assert_eq!(
            detector.observe(clock.monotonic_ms(), clock.wall_ms()),
            Some(130_000)
        );
        // ... and the sample after that is ordinary again.
        clock.advance(50);
        assert_eq!(
            detector.observe(clock.monotonic_ms(), clock.wall_ms()),
            None
        );
    }

    #[test]
    fn a_backwards_step_of_the_wall_clock_is_not_a_suspend() {
        let mut detector = SuspendDetector::new(1_000, 1_000, 2_000);
        assert_eq!(detector.observe(1_050, 500), None);
    }
}
