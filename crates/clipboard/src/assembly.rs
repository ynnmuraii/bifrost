//! Chunk assembly: the receiving half of a transfer, with every limit enforced.
//!
//! Network chunks are at most 16 KiB. An accepted offer fixes the representation,
//! exact payload length and chunk-count cap (text 64, PNG 2048).
//! Applying content requires complete ordered chunks, matching SHA-256 and valid
//! UTF-8 or PNG signature.

use crate::{
    sha256, ClipboardContent, ClipboardError, ContentKind, CHUNK_BYTES, DECODED_FRAME_BYTES,
};

/// Why a transfer was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssemblyError {
    /// A chunk for another offer, or after the transfer was cancelled.
    WrongOffer,
    /// `chunk_count` exceeds this offer's per-kind bound, or changes mid-transfer.
    BadChunkCount,
    /// A chunk larger than 16 KiB, or one whose framing would exceed 32 KiB.
    ChunkTooLarge,
    /// A chunk arrived out of order (`chunk_index != next`).
    OutOfOrder,
    /// The chunks add up to more than the payload limit.
    TooLarge,
    /// `ClipboardComplete`'s hash does not match the payload.
    HashMismatch,
    /// The payload fails its kind's representation check.
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
    kind: ContentKind,
    payload: Vec<u8>,
}

impl Assembler {
    /// Starts a transfer for `offer_id`, which promised `expected` bytes.
    pub fn new(
        offer_id: [u8; 16],
        expected: u64,
        kind: ContentKind,
    ) -> Result<Self, AssemblyError> {
        if expected > kind.hard_max_bytes() {
            return Err(AssemblyError::TooLarge);
        }
        Ok(Assembler {
            offer_id,
            count: None,
            next: 0,
            expected,
            kind,
            payload: Vec::with_capacity(expected as usize),
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
        if count == 0 || count as usize > self.kind.max_network_chunks() {
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
        if total > self.kind.hard_max_bytes() || total > self.expected {
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
    /// matches, the byte count fulfils the offer and the representation is valid.
    pub fn finish(self, sha256_expected: &[u8]) -> Result<ClipboardContent, AssemblyError> {
        let Some(count) = self.count else {
            return Err(AssemblyError::Invalid);
        };
        if self.next != count || self.payload.len() as u64 != self.expected {
            return Err(AssemblyError::Invalid);
        }
        if sha256(&self.payload) != sha256_expected {
            return Err(AssemblyError::HashMismatch);
        }
        ClipboardContent::from_bytes(self.kind, self.payload).map_err(|_| AssemblyError::Invalid)
    }

    /// How many bytes have arrived so far.
    pub fn received(&self) -> usize {
        self.payload.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_BYTES, MAX_CHUNKS};

    #[test]
    fn png_boundaries_and_text_chunk_limit_are_kind_specific() {
        let mut text = Assembler::new(offer(), 1, ContentKind::Text).expect("text");
        assert_eq!(
            text.push(&offer(), 0, 65, b"x"),
            Err(AssemblyError::BadChunkCount)
        );
        assert!(Assembler::new(offer(), crate::MAX_IMAGE_BYTES + 1, ContentKind::Png).is_err());
        let mut png = vec![0; crate::MAX_IMAGE_BYTES as usize];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let mut assembler =
            Assembler::new(offer(), png.len() as u64, ContentKind::Png).expect("PNG");
        for (index, chunk) in png.chunks(CHUNK_BYTES).enumerate() {
            assembler
                .push(&offer(), index as u32, 2048, chunk)
                .expect("image chunk");
        }
        assert_eq!(
            assembler.finish(&sha256(&png)).expect("PNG").into_bytes(),
            png
        );
        for bytes in [&b""[..], &b"bad PNG!"[..], &b"\x89PNG\r\n\x1a"[..]] {
            let mut assembler =
                Assembler::new(offer(), bytes.len() as u64, ContentKind::Png).expect("size");
            assembler.push(&offer(), 0, 1, bytes).expect("chunk");
            assert_eq!(
                assembler.finish(&sha256(bytes)),
                Err(AssemblyError::Invalid)
            );
        }
        let mut short = Assembler::new(offer(), 10, ContentKind::Png).expect("size");
        short
            .push(&offer(), 0, 1, b"\x89PNG\r\n\x1a\n")
            .expect("chunk");
        assert_eq!(
            short.finish(&sha256(b"\x89PNG\r\n\x1a\n")),
            Err(AssemblyError::Invalid)
        );
    }

    #[test]
    fn utf8_may_span_network_chunks() {
        let bytes = "я".as_bytes();
        let mut assembler = Assembler::new(offer(), 2, ContentKind::Text).expect("text");
        assembler.push(&offer(), 0, 2, &bytes[..1]).expect("prefix");
        assembler.push(&offer(), 1, 2, &bytes[1..]).expect("suffix");
        assert_eq!(
            assembler.finish(&sha256(bytes)),
            Ok(ClipboardContent::Text("я".into()))
        );
    }

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
        let mut assembler = Assembler::new(offer(), payload.len() as u64, ContentKind::Text)?;
        for (index, chunk) in chunks.iter().enumerate() {
            assembler.push(&offer(), index as u32, chunks.len() as u32, chunk)?;
        }
        match assembler.finish(&sha256(payload))? {
            ClipboardContent::Text(text) => Ok(text),
            ClipboardContent::Image { .. } => unreachable!("text assembler"),
        }
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
            Assembler::new(offer(), MAX_BYTES + 1, ContentKind::Text).err(),
            Some(AssemblyError::TooLarge)
        );
    }

    #[test]
    fn every_limit_is_enforced() {
        let mut assembler = Assembler::new(offer(), MAX_BYTES, ContentKind::Text).expect("new");
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
        let mut small = Assembler::new(offer(), 4, ContentKind::Text).expect("new");
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
        let mut assembler =
            Assembler::new(offer(), payload.len() as u64, ContentKind::Text).expect("new");
        assembler.push(&offer(), 0, 1, payload).expect("one chunk");
        assert_eq!(
            assembler.finish(&sha256(b"other")).err(),
            Some(AssemblyError::HashMismatch)
        );
        // Invalid UTF-8 is refused even with a correct hash.
        let invalid = [0xffu8, 0xfe];
        let mut assembler = Assembler::new(offer(), 2, ContentKind::Text).expect("new");
        assembler.push(&offer(), 0, 1, &invalid).expect("chunk");
        assert_eq!(
            assembler.finish(&sha256(&invalid)).err(),
            Some(AssemblyError::Invalid)
        );
    }

    #[test]
    fn a_truncated_transfer_is_not_applied() {
        let mut assembler = Assembler::new(offer(), 10, ContentKind::Text).expect("new");
        assembler.push(&offer(), 0, 2, b"abcde").expect("first");
        // Two chunks were promised; finishing now must not apply anything.
        assert_eq!(
            assembler.finish(&sha256(b"abcde")).err(),
            Some(AssemblyError::Invalid)
        );
    }

    #[test]
    fn a_short_payload_with_a_matching_hash_does_not_fulfil_the_offer() {
        for payload in [&b"abc"[..], &b""[..]] {
            let mut assembler =
                Assembler::new(offer(), 10, ContentKind::Text).expect("valid offered size");
            assert_eq!(
                assembler.push(&offer(), 0, 1, payload),
                Ok(AssemblyOutcome::Ready)
            );
            assert_eq!(
                assembler.finish(&sha256(payload)),
                Err(AssemblyError::Invalid),
                "a correct hash cannot replace the promised byte count"
            );
        }
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
