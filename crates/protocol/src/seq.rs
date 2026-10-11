//! Per-direction, per-channel sequence validation.
//!
//! `05-protocol.md`: «Sequence — отдельный строго возрастающий u64 в каждом
//! направлении каждого канала; любое повторение/регресс или пропуск на надёжном
//! stream означает protocol error. Значение не переходит через u64 max: создаётся
//! новый сеанс. `connection_id` и `lease_id` проверяются независимо от `seq`.»
//!
//! One [`SeqValidator`] guards one direction of one channel. Both Hello frames of
//! a link carry `seq = 1` (`05-protocol.md`, «Начало сеанса»), so the validator
//! starts expecting exactly `1`.

use crate::wire;

/// The next sequence number a validator expects, or why the last one failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqError {
    /// The first frame of a direction must carry `seq = 1`, not `got`.
    NotStarted { got: u64 },
    /// `got` was already accepted (a duplicate or a replay).
    Replay { expected: u64, got: u64 },
    /// `got` skipped ahead of `expected`; nothing is ever lost on a reliable
    /// stream, so this is a protocol error.
    Gap { expected: u64, got: u64 },
    /// `u64::MAX` was already accepted. The counter never wraps: a new session
    /// (new `connection_id`) is required.
    Exhausted,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reaching this boundary through accept alone would need 2^64 calls.
    #[test]
    fn maximum_sequence_is_accepted_once_and_never_wraps() {
        let mut seq = SeqValidator {
            next: u64::MAX - 1,
            exhausted: false,
        };
        assert_eq!(seq.accept(u64::MAX - 1), Ok(()));
        assert!(!seq.is_exhausted());
        assert_eq!(seq.accept(u64::MAX), Ok(()));
        assert!(seq.is_exhausted());
        for value in [0, 1, u64::MAX - 1, u64::MAX] {
            assert_eq!(seq.accept(value), Err(SeqError::Exhausted));
            assert_eq!(seq.expected(), u64::MAX);
        }
    }
}

impl SeqError {
    /// Every sequence violation is `ERROR_CODE_INVALID_SEQUENCE`; the session is
    /// closed and no payload is logged.
    pub const fn error_code(self) -> wire::ErrorCode {
        wire::ErrorCode::InvalidSequence
    }
}

/// Strictly-increasing sequence counter for one direction of one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqValidator {
    next: u64,
    exhausted: bool,
}

impl Default for SeqValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqValidator {
    /// A validator for a direction that has not seen a frame yet: it expects
    /// `seq = 1`.
    pub const fn new() -> Self {
        Self {
            next: 1,
            exhausted: false,
        }
    }

    /// The sequence number the next accepted frame must carry. Once the counter
    /// is exhausted this stays at `u64::MAX`.
    pub const fn expected(&self) -> u64 {
        self.next
    }

    /// Whether `u64::MAX` was accepted; only a new session can continue.
    pub const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Accept the `seq` of one received frame.
    ///
    /// On `Err` the validator keeps its state: the session is closed anyway, and
    /// a rejected frame must not advance the counter.
    pub fn accept(&mut self, seq: u64) -> Result<(), SeqError> {
        if self.exhausted {
            return Err(SeqError::Exhausted);
        }
        let expected = self.next;
        if expected == 1 && seq != 1 {
            return Err(SeqError::NotStarted { got: seq });
        }
        if seq == expected {
            if expected == u64::MAX {
                self.exhausted = true;
            } else {
                self.next = expected + 1;
            }
            return Ok(());
        }
        if seq < expected {
            return Err(SeqError::Replay { expected, got: seq });
        }
        Err(SeqError::Gap { expected, got: seq })
    }
}
