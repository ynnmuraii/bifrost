//! The clipboard, minus the operating system: limits, secrets, echo suppression, chunking
//! and pacing, as pure logic with the platform behind a trait.
//!
//! `docs/04-decisions.md` ADR-008 is the contract: «CLIPBOARD/Unicode text, UTF-8 ≤1 MiB,
//! явное включение на обеих сторонах. Выключается отдельно от KVM. Ограниченный transfer
//! rate, origin/version/hash для подавления эха; не сохранять историю. Контент, помеченный
//! менеджером паролей, по умолчанию не передаётся.» `docs/05-protocol.md` («Clipboard
//! канал») fixes the wire shape: `ClipboardOffer` → `ClipboardRequest` → `ClipboardData`
//! chunks → `ClipboardComplete{sha256}`, payload ≤1 MiB, chunk ≤16 KiB, decoded frame
//! ≤32 KiB, one transfer at a time per channel, ≤256 KiB/s per channel.
//!
//! Everything in this crate is deliberately free of sockets, of Win32, of X11 and of D-Bus:
//! the decisions a reviewer has to trust — what may be sent, what may be applied, when the
//! echo stops — are pure functions over values, and each platform adapter is a thin shim
//! that produces those values. That is also what makes the properties testable on any host:
//! `docs/07-quality-security.md` («Clipboard») lists the cases, and they are here.
//!
//! # The two sides
//!
//! * [`source`] is the node that copied something: it decides whether the content may leave
//!   the machine at all ([`policy`]), offers it, and paces the chunks ([`rate`]).
//! * [`sink`] is the node that receives an offer: it decides whether to ask for it, assembles
//!   the chunks ([`assembly`]) and applies the text only after the hash and UTF-8 check.
//!
//! [`echo`] sits between them: it is what keeps a fan-out from turning into a loop.

pub mod assembly;
pub mod echo;
pub mod policy;
pub mod rate;
pub mod source;

pub use assembly::{Assembler, AssemblyError, AssemblyOutcome};
pub use echo::{Echo, EchoDecision};
pub use policy::{Content, Decision, Marker, Policy};
pub use rate::Rate;
pub use source::{Incoming, Offer, Refusal, Source};

/// The MIME type of every payload: ADR-008 allows text and nothing else.
pub const MIME: &str = "text/plain;charset=utf-8";
/// `docs/05-protocol.md`: «Payload ≤1 MiB».
pub const MAX_BYTES: u64 = 1024 * 1024;
/// `docs/05-protocol.md`: «chunk ≤16 KiB».
pub const CHUNK_BYTES: usize = 16 * 1024;
/// `docs/05-protocol.md`: `chunk_count` is `1..64`.
pub const MAX_CHUNKS: usize = 64;
/// `docs/05-protocol.md`: «decoded frame ≤32 KiB».
pub const DECODED_FRAME_BYTES: usize = 32 * 1024;
/// `docs/05-protocol.md`: «rate limit ≤256 KiB/s на канал по умолчанию».
pub const DEFAULT_RATE_BYTES_PER_S: u64 = 256 * 1024;
/// `docs/05-protocol.md`: the binding `ttl` of a clipboard channel.
pub const CHANNEL_TTL_MS: u64 = 60_000;

/// Why a transfer did not happen.
///
/// The codes are the wire ones (`docs/05-protocol.md`, «Ошибки и завершение»), because the
/// same value is what a `ClipboardCancel` carries and what the GUI shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardError {
    /// `CLIPBOARD_DISABLED`: opt-in on both sides (ADR-008), so one side is enough to stop
    /// the transfer.
    Disabled,
    /// `CLIPBOARD_TOO_LARGE`: over `max_bytes` (never over the 1 MiB protocol limit).
    TooLarge,
    /// `CLIPBOARD_HASH_MISMATCH`: the `ClipboardComplete.sha256` does not match.
    HashMismatch,
    /// `CLIPBOARD_INVALID`: not UTF-8, a wrong MIME, or a chunk out of order.
    Invalid,
    /// `CLIPBOARD_SECRET`: marked by a password manager and `skip_secret_marked = true`.
    Secret,
}

impl ClipboardError {
    /// The `ErrorCode` name on the wire.
    pub const fn code(self) -> &'static str {
        match self {
            ClipboardError::Disabled => "CLIPBOARD_DISABLED",
            ClipboardError::TooLarge => "CLIPBOARD_TOO_LARGE",
            ClipboardError::HashMismatch => "CLIPBOARD_HASH_MISMATCH",
            ClipboardError::Invalid => "CLIPBOARD_INVALID",
            ClipboardError::Secret => "CLIPBOARD_SECRET",
        }
    }
}

/// The SHA-256 of a payload, as `ClipboardComplete.sha256` carries it.
pub fn sha256(payload: &[u8]) -> Vec<u8> {
    use ring::digest::{digest, SHA256};
    digest(&SHA256, payload).as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_are_the_documented_ones() {
        // `black_box` because these are constants: the point is that the *documented*
        // numbers are what the code uses, and a const-folded assertion would be the
        // compiler agreeing with itself.
        use std::hint::black_box;
        assert_eq!(black_box(MAX_BYTES), 1_048_576);
        assert_eq!(black_box(CHUNK_BYTES), 16_384);
        assert_eq!(black_box(MAX_CHUNKS), 64);
        assert_eq!(black_box(DECODED_FRAME_BYTES), 32_768);
        assert_eq!(black_box(DEFAULT_RATE_BYTES_PER_S), 262_144);
        assert_eq!(black_box(CHANNEL_TTL_MS), 60_000);
        // 64 chunks of 16 KiB are exactly the 1 MiB payload limit, and the frame limit is
        // twice the chunk limit, which is what makes one chunk one frame.
        assert_eq!(
            black_box(MAX_CHUNKS) * black_box(CHUNK_BYTES),
            black_box(MAX_BYTES) as usize
        );
        assert!(black_box(CHUNK_BYTES) * 2 <= black_box(DECODED_FRAME_BYTES));
        assert_eq!(black_box(MIME), "text/plain;charset=utf-8");
    }

    #[test]
    fn the_error_codes_are_the_wire_names() {
        for (error, code) in [
            (ClipboardError::Disabled, "CLIPBOARD_DISABLED"),
            (ClipboardError::TooLarge, "CLIPBOARD_TOO_LARGE"),
            (ClipboardError::HashMismatch, "CLIPBOARD_HASH_MISMATCH"),
            (ClipboardError::Invalid, "CLIPBOARD_INVALID"),
            (ClipboardError::Secret, "CLIPBOARD_SECRET"),
        ] {
            assert_eq!(error.code(), code);
        }
    }

    #[test]
    fn the_hash_is_sha256_and_matches_the_payload() {
        assert_eq!(sha256(b"").len(), 32);
        assert_eq!(
            sha256(b"abc"),
            hex("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_ne!(sha256(b"abc"), sha256(b"abd"));
    }

    /// Lower-case hex of a known digest, for the one vector above.
    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2)
            .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex"))
            .collect()
    }
}
