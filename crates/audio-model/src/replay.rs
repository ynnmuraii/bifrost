//! Fixed replay state for one incoming direction of one session/key.

use crate::packet::PACKET_NUMBER_LIMIT;
use core::fmt;

pub const REPLAY_WINDOW_SIZE: usize = 1024;
const WORD_BITS: usize = u64::BITS as usize;
const WORDS: usize = REPLAY_WINDOW_SIZE / WORD_BITS;

/// RFC 4303-style sliding bitmap, allowing packet number zero.
///
/// Call [`Self::check`] before authentication to reject known replays cheaply,
/// then [`Self::commit`] only after a valid tag. An unauthenticated high number
/// must never advance this window. A receiver may first observe a nonzero
/// number because earlier packets were lost. Numbers never wrap: rotate to a new
/// session/key/window before 2^24. Never reply to packets failing verification.
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    newest: Option<u64>,
    // Bit n records newest - n; bit zero is the newest authenticated packet.
    bitmap: [u64; WORDS],
}

impl ReplayWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Non-mutating eligibility check; success does not reserve the number.
    pub fn check(&self, packet_number: u64) -> Result<(), ReplayError> {
        if packet_number >= PACKET_NUMBER_LIMIT {
            return Err(ReplayError::RotationRequired);
        }
        let Some(newest) = self.newest else {
            return Ok(());
        };
        if packet_number > newest {
            return Ok(());
        }
        let distance = newest - packet_number;
        if distance >= REPLAY_WINDOW_SIZE as u64 {
            return Err(ReplayError::TooOld);
        }
        let distance = distance as usize;
        if self.bitmap[distance / WORD_BITS] & (1_u64 << (distance % WORD_BITS)) != 0 {
            return Err(ReplayError::Duplicate);
        }
        Ok(())
    }

    /// Rechecks eligibility and records an already-authenticated packet.
    ///
    /// Rechecking prevents a stale successful `check` from admitting a duplicate
    /// or a packet aged out by intervening authenticated traffic. Errors leave
    /// state unchanged. This method performs no cryptography itself.
    pub fn commit(&mut self, packet_number: u64) -> Result<(), ReplayError> {
        self.check(packet_number)?;
        let distance = match self.newest {
            None => {
                self.newest = Some(packet_number);
                0
            }
            Some(newest) if packet_number > newest => {
                let advance = packet_number - newest;
                if advance >= REPLAY_WINDOW_SIZE as u64 {
                    self.bitmap.fill(0);
                } else {
                    self.advance(advance as usize);
                }
                self.newest = Some(packet_number);
                0
            }
            Some(newest) => (newest - packet_number) as usize,
        };
        self.bitmap[distance / WORD_BITS] |= 1_u64 << (distance % WORD_BITS);
        Ok(())
    }

    fn advance(&mut self, distance: usize) {
        let words = distance / WORD_BITS;
        let bits = distance % WORD_BITS;
        // Descending updates preserve source words without a temporary bitmap.
        for destination in (0..WORDS).rev() {
            self.bitmap[destination] = if destination < words {
                0
            } else {
                let source = destination - words;
                let mut value = self.bitmap[source] << bits;
                if bits != 0 && source > 0 {
                    value |= self.bitmap[source - 1] >> (WORD_BITS - bits);
                }
                value
            };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
    Duplicate,
    TooOld,
    RotationRequired,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Duplicate => "audio packet number was already authenticated",
            Self::TooOld => "audio packet number is outside the replay window",
            Self::RotationRequired => "audio packet number requires a new session",
        })
    }
}

impl std::error::Error for ReplayError {}
