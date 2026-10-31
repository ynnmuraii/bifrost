//! Timestamp-grid scheduling. `now` is a caller-owned 48 kHz render-clock count.
//! Packet numbers are metadata only: probes share that number space with media.
use crate::{packet::MAX_PAYLOAD_LEN, status::AudioHealth};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Profile {
    Ethernet = 1,
    Wifi = 2,
}
impl Profile {
    pub const fn frame_samples(self) -> u16 {
        match self {
            Self::Ethernet => 480,
            Self::Wifi => 960,
        }
    }
    pub const fn initial_delay(self) -> u32 {
        match self {
            Self::Ethernet => 1440,
            Self::Wifi => 3840,
        }
    }
    pub const fn delay_bounds(self) -> (u32, u32) {
        match self {
            Self::Ethernet => (960, 2880),
            Self::Wifi => (2880, 7680),
        }
    }
    pub const fn capacity(self) -> usize {
        match self {
            Self::Ethernet => 16,
            Self::Wifi => 32,
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    timestamp: u32,
    number: u64,
    len: usize,
    bytes: [u8; MAX_PAYLOAD_LEN],
}
const EMPTY: Slot = Slot {
    timestamp: 0,
    number: 0,
    len: 0,
    bytes: [0; MAX_PAYLOAD_LEN],
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertError {
    Duplicate,
    Late,
    OffGrid,
    TooFar,
    InvalidPayload,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playout {
    Pending,
    Frame {
        timestamp: u32,
        packet_number: u64,
        len: usize,
    },
    /// `fec_len` bytes from the following packet are in the output buffer, without consuming it.
    Missing {
        timestamp: u32,
        fec_len: usize,
    },
    /// Startup, inactivity or 120 ms consecutive concealment: silence and rebuffer.
    Underrun,
}

pub struct JitterBuffer {
    profile: Profile,
    slots: [Slot; 32],
    occupied: u32,
    expected: Option<u32>,
    deadline: u64,
    started: bool,
    target: u32,
    stable_since: u64,
    late_run: u8,
    last_arrival: u64,
    transit: Option<(u64, u32)>,
    variation: f64,
    concealed_samples: u32,
    health: AudioHealth,
}
impl JitterBuffer {
    pub fn new(profile: Profile) -> Self {
        Self {
            profile,
            slots: [EMPTY; 32],
            occupied: 0,
            expected: None,
            deadline: 0,
            started: false,
            target: profile.initial_delay(),
            stable_since: 0,
            late_run: 0,
            last_arrival: 0,
            transit: None,
            variation: 0.0,
            concealed_samples: 0,
            health: AudioHealth::default(),
        }
    }
    pub fn target_samples(&self) -> u32 {
        self.target
    }
    pub fn buffered_frames(&self) -> usize {
        self.occupied.count_ones() as usize
    }
    /// Timestamp span still held in encoded storage, including holes awaiting FEC/PLC.
    pub fn buffered_samples(&self) -> u32 {
        let Some(expected) = self.expected else {
            return 0;
        };
        (0..self.profile.capacity())
            .filter(|&i| self.occupied & (1 << i) != 0)
            .map(|i| self.slots[i].timestamp.wrapping_sub(expected))
            .max()
            .map_or(0, |span| {
                span.saturating_add(u32::from(self.profile.frame_samples()))
            })
    }
    pub fn health(&self) -> AudioHealth {
        self.health
    }
    pub fn arrival_variation_samples(&self) -> f64 {
        self.variation
    }
    /// Authenticated but malformed codec frames also count as loss/concealment.
    pub fn record_decode_loss(&mut self) {
        self.health.lost_frames = self.health.lost_frames.saturating_add(1);
        self.health.concealed_frames = self.health.concealed_frames.saturating_add(1);
    }
    pub fn reset(&mut self) {
        self.occupied = 0;
        self.expected = None;
        self.started = false;
        self.concealed_samples = 0;
        self.late_run = 0;
        self.transit = None;
        self.variation = 0.0;
        self.health.discontinuities = self.health.discontinuities.saturating_add(1);
    }
    pub fn insert(
        &mut self,
        now: u64,
        packet_number: u64,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), InsertError> {
        if payload.is_empty() || payload.len() > MAX_PAYLOAD_LEN {
            return Err(InsertError::InvalidPayload);
        }
        let frame = i64::from(self.profile.frame_samples());
        if let Some(expected) = self.expected {
            let delta = i64::from(timestamp.wrapping_sub(expected) as i32);
            if delta % frame != 0 {
                return Err(InsertError::OffGrid);
            }
            if delta < 0 {
                if !self.started && self.deadline.saturating_sub(delta.unsigned_abs()) > now {
                    self.expected = Some(timestamp);
                    self.deadline -= delta.unsigned_abs();
                } else {
                    self.note_late(now);
                    return Err(InsertError::Late);
                }
            } else if delta >= frame * self.profile.capacity() as i64 {
                return Err(InsertError::TooFar);
            }
            if now > self.deadline.saturating_add(delta.max(0) as u64) {
                self.note_late(now);
                return Err(InsertError::Late);
            }
        } else {
            self.expected = Some(timestamp);
            self.deadline = now.saturating_add(u64::from(self.target));
            self.stable_since = now;
        }
        for i in 0..self.profile.capacity() {
            if self.occupied & (1 << i) != 0
                && (self.slots[i].timestamp == timestamp || self.slots[i].number == packet_number)
            {
                return Err(InsertError::Duplicate);
            }
        }
        let index = if let Some(index) =
            (0..self.profile.capacity()).find(|&i| self.occupied & (1 << i) == 0)
        {
            index
        } else {
            // Bounded overrun recovery: discard queued history and fade/rebuffer at the caller.
            self.reset();
            self.expected = Some(timestamp);
            self.deadline = now.saturating_add(u64::from(self.target));
            self.stable_since = now;
            0
        };
        let slot = &mut self.slots[index];
        slot.timestamp = timestamp;
        slot.number = packet_number;
        slot.len = payload.len();
        slot.bytes[..payload.len()].copy_from_slice(payload);
        self.occupied |= 1 << index;
        if let Some((arrival, media)) = self.transit {
            let delta = timestamp.wrapping_sub(media) as i32;
            if delta > 0 && now >= arrival {
                let deviation = ((now - arrival) as f64 - f64::from(delta)).abs();
                self.variation += (deviation - self.variation) / 16.0;
                self.variation = self.variation.min(f64::from(self.profile.delay_bounds().1));
                self.transit = Some((now, timestamp));
            }
        } else {
            self.transit = Some((now, timestamp));
        }
        self.last_arrival = now;
        self.health.received_packets = self.health.received_packets.saturating_add(1);
        // Shrinking advances one deadline, never makes a large resampling-rate change.
        if now.saturating_sub(self.stable_since) >= 480_000
            && self.target > self.profile.delay_bounds().0
            && self.variation < f64::from(self.profile.frame_samples()) / 2.0
        {
            self.target -= u32::from(self.profile.frame_samples());
            self.deadline = self
                .deadline
                .saturating_sub(u64::from(self.profile.frame_samples()));
            self.stable_since = now;
            self.late_run = 0;
        }
        Ok(())
    }
    fn note_late(&mut self, now: u64) {
        self.health.late_frames = self.health.late_frames.saturating_add(1);
        self.stable_since = now;
        self.late_run = self.late_run.saturating_add(1);
        if self.late_run >= 3 && self.target < self.profile.delay_bounds().1 {
            self.target += u32::from(self.profile.frame_samples());
            self.deadline = self
                .deadline
                .saturating_add(u64::from(self.profile.frame_samples()));
            self.late_run = 0;
        }
    }
    fn find(&self, timestamp: u32) -> Option<usize> {
        (0..self.profile.capacity())
            .find(|&i| self.occupied & (1 << i) != 0 && self.slots[i].timestamp == timestamp)
    }
    /// Output must hold a maximum encoded packet; no heap activity occurs here or in insert.
    pub fn pop(&mut self, now: u64, fec: bool, output: &mut [u8; MAX_PAYLOAD_LEN]) -> Playout {
        let Some(timestamp) = self.expected else {
            return Playout::Underrun;
        };
        if now < self.deadline {
            return Playout::Pending;
        }
        if now.saturating_sub(self.last_arrival) > 48_000 || self.concealed_samples >= 5760 {
            self.health.underruns = self.health.underruns.saturating_add(1);
            self.reset();
            return Playout::Underrun;
        }
        self.started = true;
        let next = timestamp.wrapping_add(u32::from(self.profile.frame_samples()));
        self.expected = Some(next);
        self.deadline = self
            .deadline
            .saturating_add(u64::from(self.profile.frame_samples()));
        if let Some(index) = self.find(timestamp) {
            let slot = &self.slots[index];
            output[..slot.len].copy_from_slice(&slot.bytes[..slot.len]);
            self.occupied &= !(1 << index);
            self.concealed_samples = 0;
            Playout::Frame {
                timestamp,
                packet_number: slot.number,
                len: slot.len,
            }
        } else {
            self.stable_since = now;
            self.health.lost_frames = self.health.lost_frames.saturating_add(1);
            self.health.concealed_frames = self.health.concealed_frames.saturating_add(1);
            self.concealed_samples += u32::from(self.profile.frame_samples());
            let fec_len = if fec {
                self.find(next)
                    .map(|i| {
                        let slot = &self.slots[i];
                        output[..slot.len].copy_from_slice(&slot.bytes[..slot.len]);
                        slot.len
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            Playout::Missing { timestamp, fec_len }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_media_anchors_after_idle_at_arbitrary_timestamp() {
        for profile in [Profile::Ethernet, Profile::Wifi] {
            for first in [1, 98_765_431, u32::MAX - 123] {
                let mut jitter = JitterBuffer::new(profile);
                let mut out = [0; MAX_PAYLOAD_LEN];
                for now in (0..24_000).step_by(48) {
                    assert_eq!(jitter.pop(now, false, &mut out), Playout::Underrun);
                }
                let frame = u64::from(profile.frame_samples());
                for n in 0..100u64 {
                    let now = 24_000 + n * frame;
                    let timestamp = first.wrapping_add((n * frame) as u32);
                    jitter.insert(now, 500 + n * 3, timestamp, &[1]).unwrap();
                    assert!(jitter.buffered_samples() <= profile.initial_delay() + frame as u32);
                    while matches!(
                        jitter.pop(now, profile == Profile::Wifi, &mut out),
                        Playout::Frame { .. }
                    ) {}
                }
                assert_eq!(jitter.target_samples(), profile.initial_delay());
                assert_eq!(jitter.health().lost_frames, 0);
                assert_eq!(jitter.health().late_frames, 0);
                assert_eq!(jitter.health().concealed_frames, 0);
                assert_eq!(jitter.health().discontinuities, 0);
                assert_eq!(jitter.health().underruns, 0);
            }
        }
    }
    #[test]
    fn reorder_wrap_duplicate_and_fec() {
        let mut jitter = JitterBuffer::new(Profile::Ethernet);
        let mut out = [0; MAX_PAYLOAD_LEN];
        let t = u32::MAX - 479;
        jitter.insert(0, 10, t, &[1]).unwrap();
        jitter.insert(480, 12, t.wrapping_add(960), &[3]).unwrap();
        assert_eq!(
            jitter.insert(481, 12, t.wrapping_add(960), &[3]),
            Err(InsertError::Duplicate)
        );
        assert_eq!(jitter.pop(1439, true, &mut out), Playout::Pending);
        assert!(matches!(
            jitter.pop(1440, true, &mut out),
            Playout::Frame {
                packet_number: 10,
                ..
            }
        ));
        assert_eq!(
            jitter.pop(1920, true, &mut out),
            Playout::Missing {
                timestamp: 0,
                fec_len: 1
            }
        );
        assert_eq!(out[0], 3);
        assert_eq!(jitter.insert(1921, 11, 0, &[2]), Err(InsertError::Late));
        assert!(matches!(
            jitter.pop(2400, true, &mut out),
            Playout::Frame {
                packet_number: 12,
                ..
            }
        ));
        assert_eq!(jitter.health().lost_frames, 1);
    }
    #[test]
    fn profiles_are_bounded_and_inactivity_rebuffers() {
        for profile in [Profile::Ethernet, Profile::Wifi] {
            let mut jitter = JitterBuffer::new(profile);
            let mut out = [0; MAX_PAYLOAD_LEN];
            jitter.insert(0, 0, 0, &[1]).unwrap();
            jitter.pop(u64::from(profile.initial_delay()), false, &mut out);
            for n in 0..100 {
                let _ = jitter.insert(10_000 + n, 1, 0, &[1]);
            }
            assert_eq!(jitter.target_samples(), profile.delay_bounds().1);
            assert_eq!(jitter.pop(100_000, false, &mut out), Playout::Underrun);
            assert_eq!(jitter.buffered_frames(), 0);
        }
    }
    #[test]
    fn packet_number_gaps_are_not_media_loss() {
        let mut jitter = JitterBuffer::new(Profile::Ethernet);
        let mut out = [0; MAX_PAYLOAD_LEN];
        jitter.insert(0, 0, 0, &[1]).unwrap();
        jitter.insert(480, 100, 480, &[2]).unwrap();
        jitter.pop(1440, false, &mut out);
        jitter.pop(1920, false, &mut out);
        assert_eq!(jitter.health().lost_frames, 0);
    }
    #[test]
    fn stable_shrink_grid_bounds_and_overrun_rebuffer() {
        let mut jitter = JitterBuffer::new(Profile::Ethernet);
        let mut out = [0; MAX_PAYLOAD_LEN];
        for n in 0..1100u64 {
            jitter.insert(n * 480, n, (n * 480) as u32, &[1]).unwrap();
            while matches!(jitter.pop(n * 480, false, &mut out), Playout::Frame { .. }) {}
        }
        assert_eq!(jitter.target_samples(), 960);
        assert_eq!(jitter.health().lost_frames, 0);
        assert_eq!(jitter.arrival_variation_samples(), 0.0);
        let mut jitter = JitterBuffer::new(Profile::Ethernet);
        for n in 0..16 {
            jitter.insert(0, n, n as u32 * 480, &[1]).unwrap();
        }
        assert_eq!(jitter.insert(0, 17, 1, &[1]), Err(InsertError::OffGrid));
        assert_eq!(
            jitter.insert(0, 18, 16 * 480, &[1]),
            Err(InsertError::TooFar)
        );
        jitter.insert(0, 19, 0u32.wrapping_sub(480), &[1]).unwrap();
        assert_eq!(jitter.buffered_frames(), 1);
        assert_eq!(jitter.health().discontinuities, 1);
    }
    #[test]
    fn consecutive_concealment_stops_at_120_ms() {
        let mut jitter = JitterBuffer::new(Profile::Ethernet);
        let mut out = [0; MAX_PAYLOAD_LEN];
        jitter.insert(0, 0, 0, &[1]).unwrap();
        jitter.pop(1440, false, &mut out);
        for n in 1..=12 {
            assert!(matches!(
                jitter.pop(1440 + n * 480, true, &mut out),
                Playout::Missing { fec_len: 0, .. }
            ));
        }
        assert_eq!(
            jitter.pop(1440 + 13 * 480, false, &mut out),
            Playout::Underrun
        );
        assert_eq!(jitter.health().concealed_frames, 12);
        assert_eq!(jitter.health().underruns, 1);
    }
}
