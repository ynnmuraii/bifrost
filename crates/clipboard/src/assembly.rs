//! Chunk assembly: the receiving half of a transfer, with every limit enforced.
//!
//! `docs/05-protocol.md`: `ClipboardData(offer_id, chunk_index, chunk_count, bytes)` with
//! `chunk_count` in `1..64`, `bytes` ≤16 KiB, the payload ≤1 MiB and the decoded frame
//! ≤32 KiB; the transfer ends with `ClipboardComplete{sha256}`, and the text is applied only
//! after the hash and the UTF-8 check. The mermaid state diagram in the same section is the
//! order this module enforces: `Requested` → `Receiving` (index +1 each time) → `Verifying`
//! → `Applied` or `Cancelled`.

use crate::{sha256, ClipboardError, CHUNK_BYTES, DECODED_FRAME_BYTES, MAX_BYTES, MAX_CHUNKS};

/// Why a transfer was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssemblyError {
    /// A chunk for another offer, or after the transfer was cancelled.
    WrongOffer,
    /// `chunk_count` outside `1..64`.
    BadChunkCount,
    /// A chunk larger than 16 KiB, or one whose framing would exceed 32 KiB.
    ChunkTooLarge,
    /// A chunk arrived out of order (`chunk_index != next`).
    OutOfOrder,
    /// The chunks add up to more than the payload limit.
    TooLarge,
    /// `ClipboardComplete`'s hash does not match the payload.
    HashMismatch,
    /// The payload is not UTF-8, or the MIME is not the documented one.
    Invalid,
}

impl AssemblyError {
    /// The wire code this becomes in `ClipboardCancel.reason`.
    pub const fn error(self) -> ClipboardError {
        match self {
            AssemblyError::TooLarge => ClipboardError::TooLarge,
            AssemblyError::HashMismatch => ClipboardError::HashMismatch,
            AssemblyError::WrongOffer
            | AssemblyError::BadChunkCount
            | AssemblyError::ChunkTooLarge
            | AssemblyError::OutOfOrder
            | AssemblyError::Invalid => ClipboardError::Invalid,
        }
    }
}

/// The state of one transfer after a chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyOutcome {
    /// More chunks are expected.
    NeedMore,
    /// The last chunk arrived; `ClipboardComplete` is what finishes it.
    Ready,
}

/// Assembles one offer's chunks.
#[derive(Debug)]
pub struct Assembler {
    offer_id: [u8; 16],
    /// `chunk_count` from the first chunk, which fixes the rest of the transfer.
    count: Option<u32>,
    next: u32,
    /// The size the offer promised; the assembled payload may not exceed it.
    expected: u64,
    payload: Vec<u8>,
}

impl Assembler {
    /// Starts a transfer for `offer_id`, which promised `expected` bytes.
    pub fn new(offer_id: [u8; 16], expected: u64) -> Result<Self, AssemblyError> {
        if expected > MAX_BYTES {
            return Err(AssemblyError::TooLarge);
        }
        Ok(Assembler {
            offer_id,
            count: None,
            next: 0,
            expected,
            payload: Vec::with_capacity(expected.min(MAX_BYTES) as usize),
        })
    }

    /// Takes one `ClipboardData`.
    pub fn push(
        &mut self,
        offer_id: &[u8],
        index: u32,
        count: u32,
        data: &[u8],
    ) -> Result<AssemblyOutcome, AssemblyError> {
        if offer_id != self.offer_id {
            return Err(AssemblyError::WrongOffer);
        }
        if count == 0 || count as usize > MAX_CHUNKS {
            return Err(AssemblyError::BadChunkCount);
        }
        if data.len() > CHUNK_BYTES || data.len() > DECODED_FRAME_BYTES {
            return Err(AssemblyError::ChunkTooLarge);
        }
        match self.count {
            // The first chunk fixes the count, so a later chunk cannot disagree with it.
            None => self.count = Some(count),
            Some(fixed) if fixed != count => return Err(AssemblyError::BadChunkCount),
            Some(_) => {}
        }
        if index != self.next {
            return Err(AssemblyError::OutOfOrder);
        }
        if index >= count {
            return Err(AssemblyError::BadChunkCount);
        }
        let total = self.payload.len() as u64 + data.len() as u64;
        if total > MAX_BYTES || total > self.expected {
            return Err(AssemblyError::TooLarge);
        }
        self.payload.extend_from_slice(data);
        self.next += 1;
        if self.next == count {
            Ok(AssemblyOutcome::Ready)
        } else {
            Ok(AssemblyOutcome::NeedMore)
        }
    }

    /// Finishes with `ClipboardComplete`'s hash: the payload is applied only when the hash
    /// matches **and** the text is valid UTF-8, per `docs/05-protocol.md`.
    pub fn finish(self, sha256_expected: &[u8]) -> Result<String, AssemblyError> {
        let Some(count) = self.count else {
            return Err(AssemblyError::Invalid);
        };
        if self.next != count {
            return Err(AssemblyError::Invalid);
        }
        if sha256(&self.payload) != sha256_expected {
            return Err(AssemblyError::HashMismatch);
        }
        String::from_utf8(self.payload).map_err(|_| AssemblyError::Invalid)
    }

    /// How many bytes have arrived so far.
    pub fn received(&self) -> usize {
        self.payload.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> [u8; 16] {
        [7u8; 16]
    }

    /// Chunk `data` into the documented 16 KiB pieces.
    ///
    /// `chunk_count` is `1..64`, so an empty payload is one empty chunk, not zero chunks.
    fn chunked(payload: &[u8]) -> Vec<Vec<u8>> {
        if payload.is_empty() {
            return vec![Vec::new()];
        }
        payload.chunks(CHUNK_BYTES).map(<[u8]>::to_vec).collect()
    }

    fn assemble(payload: &[u8]) -> Result<String, AssemblyError> {
        let chunks = chunked(payload);
        let mut assembler = Assembler::new(offer(), payload.len() as u64)?;
        for (index, chunk) in chunks.iter().enumerate() {
            assembler.push(&offer(), index as u32, chunks.len() as u32, chunk)?;
        }
        assembler.finish(&sha256(payload))
    }

    #[test]
    fn a_payload_round_trips_through_the_documented_chunking() {
        for text in ["", "hello", "привет, мир", &"x".repeat(4096)] {
            assert_eq!(assemble(text.as_bytes()).expect("assembled"), text);
        }
    }

    #[test]
    fn the_one_mebibyte_boundary_is_exact() {
        // `AC-09`: 1 MiB in 64 chunks is the largest transfer, and it is accepted.
        let biggest = vec![b'a'; MAX_BYTES as usize];
        let chunks = chunked(&biggest);
        assert_eq!(chunks.len(), MAX_CHUNKS);
        assert_eq!(assemble(&biggest).expect("1 MiB").len(), MAX_BYTES as usize);

        // One byte more is refused before anything is allocated.
        assert_eq!(
            Assembler::new(offer(), MAX_BYTES + 1).err(),
            Some(AssemblyError::TooLarge)
        );
    }

    #[test]
    fn every_limit_is_enforced() {
        let mut assembler = Assembler::new(offer(), MAX_BYTES).expect("new");
        // A chunk over 16 KiB.
        assert_eq!(
            assembler
                .push(&offer(), 0, 2, &vec![0u8; CHUNK_BYTES + 1])
                .err(),
            Some(AssemblyError::ChunkTooLarge)
        );
        // A chunk count outside 1..64.
        assert_eq!(
            assembler.push(&offer(), 0, 0, b"x").err(),
            Some(AssemblyError::BadChunkCount)
        );
        assert_eq!(
            assembler
                .push(&offer(), 0, MAX_CHUNKS as u32 + 1, b"x")
                .err(),
            Some(AssemblyError::BadChunkCount)
        );
        // A chunk for a different offer.
        assert_eq!(
            assembler.push(&[9u8; 16], 0, 1, b"x").err(),
            Some(AssemblyError::WrongOffer)
        );
        // Out of order: index 1 before index 0.
        assert_eq!(
            assembler.push(&offer(), 1, 2, b"x").err(),
            Some(AssemblyError::OutOfOrder)
        );
        // The first chunk fixes the count.
        assert_eq!(
            assembler.push(&offer(), 0, 2, b"ab").expect("first"),
            AssemblyOutcome::NeedMore
        );
        assert_eq!(
            assembler.push(&offer(), 1, 3, b"cd").err(),
            Some(AssemblyError::BadChunkCount)
        );
        // And the payload may not exceed what the offer promised: this transfer promised
        // four bytes, so a full chunk is already over it.
        let mut small = Assembler::new(offer(), 4).expect("new");
        assert_eq!(
            small.push(&offer(), 0, 2, b"abc").expect("first"),
            AssemblyOutcome::NeedMore
        );
        assert_eq!(
            small.push(&offer(), 1, 2, &vec![0u8; CHUNK_BYTES]).err(),
            Some(AssemblyError::TooLarge)
        );
    }

    #[test]
    fn the_hash_and_the_utf8_check_are_what_applies_the_text() {
        let payload = "секрет".as_bytes();
        // A wrong hash cancels the transfer.
        let mut assembler = Assembler::new(offer(), payload.len() as u64).expect("new");
        assembler.push(&offer(), 0, 1, payload).expect("one chunk");
        assert_eq!(
            assembler.finish(&sha256(b"other")).err(),
            Some(AssemblyError::HashMismatch)
        );
        // Invalid UTF-8 is refused even with a correct hash.
        let invalid = [0xffu8, 0xfe];
        let mut assembler = Assembler::new(offer(), 2).expect("new");
        assembler.push(&offer(), 0, 1, &invalid).expect("chunk");
        assert_eq!(
            assembler.finish(&sha256(&invalid)).err(),
            Some(AssemblyError::Invalid)
        );
    }

    #[test]
    fn a_truncated_transfer_is_not_applied() {
        let mut assembler = Assembler::new(offer(), 10).expect("new");
        assembler.push(&offer(), 0, 2, b"abcde").expect("first");
        // Two chunks were promised; finishing now must not apply anything.
        assert_eq!(
            assembler.finish(&sha256(b"abcde")).err(),
            Some(AssemblyError::Invalid)
        );
    }

    #[test]
    fn the_error_maps_to_the_documented_wire_code() {
        assert_eq!(
            AssemblyError::TooLarge.error(),
            crate::ClipboardError::TooLarge
        );
        assert_eq!(
            AssemblyError::HashMismatch.error(),
            crate::ClipboardError::HashMismatch
        );
        for error in [
            AssemblyError::WrongOffer,
            AssemblyError::BadChunkCount,
            AssemblyError::ChunkTooLarge,
            AssemblyError::OutOfOrder,
            AssemblyError::Invalid,
        ] {
            assert_eq!(error.error(), crate::ClipboardError::Invalid);
        }
    }
}
