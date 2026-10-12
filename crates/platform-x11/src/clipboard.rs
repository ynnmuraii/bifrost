//! The X11 clipboard: the `INCR` protocol as a pure machine, and (on Linux) the CLIPBOARD
//! selection behind it.
//!
//! Redirection for `SelectionRequest`/`SelectionNotify` is not a thing in X11: a selection is
//! a property on the *requestor's* window, so a payload larger than the requestor's maximum
//! request size is transferred by the ICCCM `INCR` convention — the owner answers with the
//! property type `INCR` and a 32-bit total size, then writes the data in chunks, each chunk
//! acknowledged by the requestor deleting the property, and a zero-length chunk ends it.
//!
//! That protocol is pure bookkeeping, so it lives here without x11rb and is unit-tested on
//! any host: the adapter below is the `open()`/property/property-notify plumbing around it.
//!
//! `x-kde-passwordManagerHint` with the value `secret` is KDE's way of marking an item the
//! password manager owns; `clipboard::Marker::KdeSecret` is what it maps to, and the
//! classification here is the same pure decision Windows has.

use std::ops::Range;

/// The default maximum request size this node will push through one property write.
///
/// ICCCM leaves the value to the requestor (`MAX-REQUEST-SIZE`); 64 KiB is what X clients
/// commonly advertise, and it keeps one property write inside the server's usual buffer.
pub const DEFAULT_MAX_REQUEST_SIZE: usize = 64 * 1024;

/// `x-kde-passwordManagerHint`: the property KDE's password managers set.
pub const KDE_PASSWORD_HINT: &str = "x-kde-passwordManagerHint";
/// The value that means "this is a secret".
pub const KDE_SECRET: &str = "secret";

/// Why an `INCR` transfer was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncrError {
    /// The requestor was handed more bytes than the owner announced.
    Overrun,
    /// A chunk arrived after the transfer had already ended.
    AfterEnd,
    /// The announced total was zero: an empty selection is not an `INCR` transfer.
    Empty,
}

/// Whether a payload needs the `INCR` convention at all.
///
/// Exactly at the requestor's maximum it fits in one property write, so only a strictly
/// larger payload is streamed.
pub fn needs_incr(size: usize, max_request_size: usize) -> bool {
    size > max_request_size
}

/// The owner's half: which slice to write next, and when the transfer is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    total: usize,
    chunk: usize,
    sent: usize,
}

impl Owner {
    /// Starts an `INCR` transfer of `total` bytes, `chunk` bytes per property write.
    pub fn start(total: usize, chunk: usize) -> Result<Self, IncrError> {
        if total == 0 {
            return Err(IncrError::Empty);
        }
        Ok(Owner {
            total,
            chunk: chunk.max(1),
            sent: 0,
        })
    }

    /// The next slice to write, or an empty range when the transfer is finished.
    ///
    /// An empty range is the documented zero-length chunk: it is written once, and the
    /// requestor's delete of the property after it completes the transfer.
    pub fn next_slice(&mut self) -> Range<usize> {
        let start = self.sent;
        let end = (start + self.chunk).min(self.total);
        self.sent = end;
        start..end
    }

    /// Whether everything has already been handed over.
    pub fn is_finished(&self) -> bool {
        self.sent >= self.total
    }

    /// The total this transfer announced.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// The requestor's half: how much of the announced payload has arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requestor {
    total: usize,
    received: usize,
    finished: bool,
}

impl Requestor {
    /// Starts collecting an `INCR` payload that announced `total` bytes.
    pub fn start(total: usize) -> Result<Self, IncrError> {
        if total == 0 {
            return Err(IncrError::Empty);
        }
        Ok(Requestor {
            total,
            received: 0,
            finished: false,
        })
    }

    /// Takes one property write's worth of bytes.
    pub fn accept(&mut self, bytes: usize) -> Result<(), IncrError> {
        // A zero-length write is the end marker. It is legal once the announced total has
        // arrived *and* while it has not — that is how a truncating owner is noticed rather
        // than silently accepted.
        if bytes == 0 {
            self.finished = true;
            return Ok(());
        }
        if self.finished {
            return Err(IncrError::AfterEnd);
        }
        if self.received + bytes > self.total {
            return Err(IncrError::Overrun);
        }
        self.received += bytes;
        if self.received == self.total {
            self.finished = true;
        }
        Ok(())
    }

    /// Whether the end marker arrived.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// How many bytes are still expected.
    pub fn expected(&self) -> usize {
        self.total.saturating_sub(self.received)
    }

    /// Whether the announced total arrived in full.
    pub fn is_complete(&self) -> bool {
        self.finished && self.received == self.total
    }
}

/// Classifies the KDE hint's *value*, as read from the selection's property.
///
/// Pure: the adapter reads the property, this decides.
pub fn kde_hint_is_secret(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .map(|text| text.trim_matches('\0').eq_ignore_ascii_case(KDE_SECRET))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_exclusive() {
        assert!(!needs_incr(1000, DEFAULT_MAX_REQUEST_SIZE));
        assert!(!needs_incr(
            DEFAULT_MAX_REQUEST_SIZE,
            DEFAULT_MAX_REQUEST_SIZE
        ));
        assert!(needs_incr(
            DEFAULT_MAX_REQUEST_SIZE + 1,
            DEFAULT_MAX_REQUEST_SIZE
        ));
    }

    #[test]
    fn the_owners_slices_cover_exactly_the_payload() {
        let payload = 300 * 1024;
        let mut owner = Owner::start(payload, 256 * 1024).expect("started");
        let mut covered = 0;
        let mut slices = Vec::new();
        loop {
            let slice = owner.next_slice();
            if slice.is_empty() {
                break;
            }
            assert!(slice.end - slice.start <= 256 * 1024, "chunk too big");
            covered += slice.end - slice.start;
            slices.push(slice);
        }
        assert_eq!(covered, payload);
        assert_eq!(slices.len(), 2, "300 KiB in 256 KiB chunks: two writes");
        assert!(owner.is_finished());
    }

    #[test]
    fn an_incr_transfer_must_not_be_empty() {
        assert_eq!(Owner::start(0, 1024).err(), Some(IncrError::Empty));
        assert_eq!(Requestor::start(0).err(), Some(IncrError::Empty));
    }

    #[test]
    fn the_requestor_accepts_chunks_and_a_zero_length_end_marker() {
        let mut requestor = Requestor::start(300 * 1024).expect("started");
        assert_eq!(requestor.expected(), 300 * 1024);
        requestor.accept(256 * 1024).expect("first chunk");
        assert!(!requestor.is_finished());
        assert_eq!(requestor.expected(), 44 * 1024);
        requestor.accept(44 * 1024).expect("second chunk");
        assert!(requestor.is_finished());
        assert!(requestor.is_complete());
        // The explicit end marker after the full payload is part of the protocol.
        requestor.accept(0).expect("end marker");
        assert!(requestor.is_complete());
        // Anything after the end is refused.
        assert_eq!(requestor.accept(1).err(), Some(IncrError::AfterEnd));
    }

    #[test]
    fn an_owner_that_sends_more_than_it_announced_is_refused() {
        let mut requestor = Requestor::start(1024).expect("started");
        assert_eq!(requestor.accept(2048).err(), Some(IncrError::Overrun));
        // A truncated transfer ends with the marker but is not complete.
        requestor.accept(512).expect("partial");
        requestor.accept(0).expect("end marker");
        assert!(requestor.is_finished());
        assert!(
            !requestor.is_complete(),
            "512 of 1024 bytes is not complete"
        );
    }

    #[test]
    fn a_long_transfer_round_trips_byte_for_byte() {
        // The machine's two halves drive one another, which is what the adapter does with
        // X properties.
        let payload: Vec<u8> = (0..(300 * 1024)).map(|byte| (byte % 251) as u8).collect();
        let mut owner = Owner::start(payload.len(), 64 * 1024).expect("owner");
        let mut requestor = Requestor::start(payload.len()).expect("requestor");
        let mut collected = Vec::with_capacity(payload.len());
        loop {
            let slice = owner.next_slice();
            let end = slice.is_empty();
            collected.extend_from_slice(&payload[slice.clone()]);
            requestor
                .accept(slice.end - slice.start)
                .expect("the requestor takes the chunk");
            if end {
                break;
            }
        }
        assert_eq!(collected, payload);
        assert!(requestor.is_complete());
        assert!(owner.is_finished());
    }

    #[test]
    fn the_kde_hint_marks_a_secret_only_with_the_documented_value() {
        assert!(kde_hint_is_secret(b"secret"));
        assert!(kde_hint_is_secret(b"SECRET\0"));
        assert!(!kde_hint_is_secret(b" secret "), "the value is exact");
        assert!(!kde_hint_is_secret(b""));
        assert!(!kde_hint_is_secret(b"normal"));
        // Not UTF-8: never a secret, and never a panic.
        assert!(!kde_hint_is_secret(&[0xff, 0xfe]));
    }
}
