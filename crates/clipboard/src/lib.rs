//! Pure clipboard contracts: policy, typed payloads, bounded assembly, echo and pacing.
//!
//! Text retains its 1 MiB / 64 network chunk cap. Opt-in PNG uses a distinct
//! 32 MiB / 2048 chunk cap; no image decoder or native clipboard access lives here.
//! The wire digest always covers the original bytes. PNG validation checks only its
//! eight-byte signature; native adapters must enforce their own safe decode limits.
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
//! * [`assembly`] receives chunks and returns typed content only after exact size,
//!   SHA-256 and representation validation.
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

/// Exact wire MIME for UTF-8 text.
pub const MIME: &str = "text/plain;charset=utf-8";
/// Text payload hard cap. This does not grow when image support is enabled.
pub const MAX_BYTES: u64 = 1024 * 1024;
/// Maximum network chunk size for either kind.
pub const CHUNK_BYTES: usize = 16 * 1024;
/// Text network chunk count cap.
pub const MAX_CHUNKS: usize = 64;
pub const IMAGE_MIME: &str = "image/png";
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_IMAGE_CHUNKS: usize = 2048;
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_IMAGE_RATE_BYTES_PER_S: u64 = 8 * 1024 * 1024;
/// `docs/05-protocol.md`: «decoded frame ≤32 KiB».
pub const DECODED_FRAME_BYTES: usize = 32 * 1024;
/// `docs/05-protocol.md`: «rate limit ≤256 KiB/s на канал по умолчанию».
pub const DEFAULT_RATE_BYTES_PER_S: u64 = 256 * 1024;
/// `docs/05-protocol.md`: the binding `ttl` of a clipboard channel.
pub const CHANNEL_TTL_MS: u64 = 60_000;
/// Maximum simultaneous incoming offers across all authenticated peers, including empty ones.
pub const MAX_INCOMING_OFFERS: usize = 8;
/// Text reservation budget, independent of images.
pub const MAX_INCOMING_TEXT_BYTES: u64 = 4 * MAX_BYTES;
pub const MAX_INCOMING_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_INCOMING_IMAGE_OFFERS: usize = 2;
/// Absolute offer lifetime, including deferred focus and transfer; chunks never renew it.
pub const INCOMING_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Why a transfer did not happen.
///
/// The codes are the wire ones (`docs/05-protocol.md`, «Ошибки и завершение»), because the
/// same value is what a `ClipboardCancel` carries and what the GUI shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardError {
    /// `CLIPBOARD_DISABLED`: opt-in on both sides (ADR-008), so one side is enough to stop
    /// the transfer.
    Disabled,
    /// `CLIPBOARD_TOO_LARGE`: over the configured or per-kind hard limit.
    TooLarge,
    /// `CLIPBOARD_HASH_MISMATCH`: the `ClipboardComplete.sha256` does not match.
    HashMismatch,
    /// `CLIPBOARD_INVALID`: not UTF-8, a wrong MIME, or a chunk out of order.
    Invalid,
    /// `CLIPBOARD_SECRET`: marked by a password manager and `skip_secret_marked = true`.
    Secret,
    /// `CLIPBOARD_SUPERSEDED`: a newer local copy won over this incoming offer.
    Superseded,
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
            ClipboardError::Superseded => "CLIPBOARD_SUPERSEDED",
        }
    }
}

/// The SHA-256 of a payload, as `ClipboardComplete.sha256` carries it.
pub fn sha256(payload: &[u8]) -> Vec<u8> {
    use ring::digest::{digest, SHA256};
    digest(&SHA256, payload).as_ref().to_vec()
}

/// Validated wire representation. MIME matching is deliberately exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Png,
}

impl ContentKind {
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Text => MIME,
            Self::Png => IMAGE_MIME,
        }
    }

    pub fn from_mime(mime: &str) -> Result<Self, ClipboardError> {
        match mime {
            MIME => Ok(Self::Text),
            IMAGE_MIME => Ok(Self::Png),
            _ => Err(ClipboardError::Invalid),
        }
    }

    pub const fn hard_max_bytes(self) -> u64 {
        match self {
            Self::Text => MAX_BYTES,
            Self::Png => MAX_IMAGE_BYTES,
        }
    }

    pub const fn max_network_chunks(self) -> usize {
        match self {
            Self::Text => MAX_CHUNKS,
            Self::Png => MAX_IMAGE_CHUNKS,
        }
    }
}

/// Owned, platform-neutral clipboard bytes; Debug never exposes the payload.
///
/// Adapters convert native DIB/TIFF to PNG before byte limits are applied. Observation may
/// use polling rather than events. A future macOS adapter treats `ConcealedType` as secret;
/// `TransientType` and `AutoGeneratedType` remain non-secret advisory hints, not refusals.
#[derive(Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image { png: Vec<u8> },
}

impl std::fmt::Debug for ClipboardContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardContent")
            .field("kind", &self.kind())
            .field("len", &self.len())
            .finish()
    }
}

impl ClipboardContent {
    pub fn kind(&self) -> ContentKind {
        match self {
            Self::Text(_) => ContentKind::Text,
            Self::Image { .. } => ContentKind::Png,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Image { png } => png,
        }
    }

    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Text(text) => text.into_bytes(),
            Self::Image { png } => png,
        }
    }

    pub fn from_bytes(kind: ContentKind, bytes: Vec<u8>) -> Result<Self, ClipboardError> {
        if bytes.len() as u64 > kind.hard_max_bytes() {
            return Err(ClipboardError::TooLarge);
        }
        match kind {
            ContentKind::Text => String::from_utf8(bytes)
                .map(Self::Text)
                .map_err(|_| ClipboardError::Invalid),
            ContentKind::Png if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
                Ok(Self::Image { png: bytes })
            }
            ContentKind::Png => Err(ClipboardError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_contract_is_exact_and_debug_is_payload_free() {
        for kind in [ContentKind::Text, ContentKind::Png] {
            assert_eq!(ContentKind::from_mime(kind.mime()), Ok(kind));
        }
        for mime in ["", "text/plain", "IMAGE/PNG", "image/png;charset=utf-8"] {
            assert_eq!(ContentKind::from_mime(mime), Err(ClipboardError::Invalid));
        }
        assert_eq!(ContentKind::Text.hard_max_bytes(), 1_048_576);
        assert_eq!(ContentKind::Png.hard_max_bytes(), 33_554_432);
        assert_eq!(ContentKind::Text.max_network_chunks(), 64);
        assert_eq!(ContentKind::Png.max_network_chunks(), 2048);
        assert_eq!(MAX_INCOMING_TEXT_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_INCOMING_IMAGE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_INCOMING_IMAGE_OFFERS, 2);
        assert_eq!(MAX_INCOMING_OFFERS, 8);
        let text = ClipboardContent::from_bytes(ContentKind::Text, b"private payload".to_vec())
            .expect("UTF-8");
        assert!(!format!("{text:?}").contains("private payload"));
        assert!(!text.is_empty());
        assert_eq!(text.kind(), ContentKind::Text);
        assert_eq!(text.into_bytes(), b"private payload");
        assert!(ClipboardContent::from_bytes(ContentKind::Text, vec![0xff]).is_err());
        for png in [vec![], b"\x89PNG\r\n\x1a".to_vec(), b"not a PNG".to_vec()] {
            assert_eq!(
                ClipboardContent::from_bytes(ContentKind::Png, png),
                Err(ClipboardError::Invalid)
            );
        }
        let png = b"\x89PNG\r\n\x1a\npayload".to_vec();
        let image = ClipboardContent::from_bytes(ContentKind::Png, png.clone()).expect("signature");
        assert_eq!(image.kind(), ContentKind::Png);
        assert_eq!(image.as_bytes(), png);
        assert_eq!(image.into_bytes(), png);
    }
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
            (ClipboardError::Superseded, "CLIPBOARD_SUPERSEDED"),
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
