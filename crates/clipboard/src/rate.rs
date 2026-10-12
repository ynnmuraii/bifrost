//! Pacing: `docs/05-protocol.md` allows «≤256 KiB/s **на канал** по умолчанию», and
//! `KVM-042` («объём clipboard не блокирует key-up») is why the clipboard channel exists at
//! all — this limiter is what keeps a 1 MiB payload from flooding the link it shares with
//! nothing else. It is a token bucket over the caller's monotonic clock, so it is testable
//! without sleeping.

use crate::DEFAULT_RATE_BYTES_PER_S;

/// A token bucket in bytes per second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rate {
    bytes_per_s: u64,
    /// Bytes already allowed, as a fractional allowance in bytes.
    allowance: u64,
    last_ms: u64,
}

impl Rate {
    /// A bucket that starts full: the first chunk of a transfer does not wait.
    pub fn new(bytes_per_s: u64, now_ms: u64) -> Self {
        Rate {
            bytes_per_s: bytes_per_s.max(1),
            allowance: bytes_per_s.max(1),
            last_ms: now_ms,
        }
    }

    /// The documented default (256 KiB/s).
    pub fn default_at(now_ms: u64) -> Self {
        Rate::new(DEFAULT_RATE_BYTES_PER_S, now_ms)
    }

    /// How long to wait before `bytes` may be sent, consuming the allowance.
    ///
    /// A return of `0` means "send now". The bucket refills with elapsed time, capped at one
    /// second's worth, so an idle channel does not accumulate an unbounded burst.
    pub fn delay_ms(&mut self, bytes: u64, now_ms: u64) -> u64 {
        let elapsed = now_ms.saturating_sub(self.last_ms);
        let refill = self.bytes_per_s.saturating_mul(elapsed) / 1000;
        self.allowance = (self.allowance + refill).min(self.bytes_per_s);

        let delay = if bytes <= self.allowance {
            self.allowance -= bytes;
            0
        } else {
            let missing = bytes - self.allowance;
            self.allowance = 0;
            // Round up: waiting less than the deficit would exceed the rate.
            missing.saturating_mul(1000).div_ceil(self.bytes_per_s)
        };
        // The clock advances to the moment the bytes leave, so the wait this call asked for
        // is never counted as a refill twice.
        self.last_ms = now_ms + delay;
        delay
    }

    /// The configured rate.
    pub fn bytes_per_s(&self) -> u64 {
        self.bytes_per_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_bucket_sends_at_once_and_an_empty_one_waits() {
        let mut rate = Rate::new(1000, 0);
        // The bucket starts full: one second's worth goes immediately.
        assert_eq!(rate.delay_ms(1000, 0), 0);
        // The next kilobyte has to wait a second for the bucket to refill, and the clock
        // then stands at the moment those bytes leave.
        assert_eq!(rate.delay_ms(1000, 0), 1000);
        // The bucket stays empty while the caller keeps sending: one kilobyte per second,
        // with the first second's worth as the only burst.
        assert_eq!(rate.delay_ms(1000, 1000), 1000);
        assert_eq!(rate.delay_ms(1000, 2000), 1000);
        // Two seconds of silence after that send (at 3000) refill two kilobytes, so a
        // smaller chunk goes now and the rest is paced by what is left.
        assert_eq!(rate.delay_ms(500, 4000), 0);
        assert_eq!(rate.delay_ms(1500, 4000), 1000);
    }

    #[test]
    fn the_average_never_exceeds_the_rate() {
        // Property: sending `n` chunks of `c` bytes each can never finish faster than
        // `n * c / rate` seconds, whatever the chunk sizes are.
        // Every case is realistic: the chunk never exceeds one second's allowance, which
        // is what the protocol guarantees (16 KiB chunks against a 256 KiB/s channel).
        for (rate_bytes, chunk) in [(1000u64, 100u64), (262_144, 16_384), (16_384, 16_384)] {
            let mut rate = Rate::new(rate_bytes, 0);
            let mut now = 0u64;
            let chunks = 20;
            for _ in 0..chunks {
                now += rate.delay_ms(chunk, now);
            }
            let sent = chunks * chunk;
            let fastest = sent.saturating_mul(1000) / rate_bytes;
            // The bucket starts full, so the first second's worth is a legitimate burst:
            // the invariant is the *rate* over time, with at most that one bucket of slack.
            assert!(
                now + 1000 >= fastest,
                "rate {rate_bytes}, chunk {chunk}: {sent} bytes in {now} ms < {fastest} ms"
            );
        }
    }

    #[test]
    fn an_idle_channel_does_not_accumulate_an_unbounded_burst() {
        let mut rate = Rate::new(1000, 0);
        // A minute of silence still only allows one second's worth.
        assert_eq!(rate.delay_ms(1000, 60_000), 0);
        assert_eq!(rate.delay_ms(1, 60_000), 1);
    }

    #[test]
    fn zero_bytes_never_waits() {
        let mut rate = Rate::default_at(0);
        assert_eq!(rate.delay_ms(0, 0), 0);
        assert_eq!(rate.bytes_per_s(), DEFAULT_RATE_BYTES_PER_S);
    }
}
