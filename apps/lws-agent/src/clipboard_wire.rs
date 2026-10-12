//! The clipboard's IPC wire form: chunking out, assembly in.
//!
//! ADR-008 keeps clipboard content off the input path; between core and the agent it still
//! has to travel, and an `AgentFrame` is at most 64 KiB, so both `ClipboardChanged` (agent →
//! core) and `SetClipboardText` (core → agent) carry a payload in chunks of ≤ 32768 bytes
//! with `chunk_count` in `1..=64`, and `sha256` over the whole payload on the last chunk only
//! (`crates/protocol/proto/ipc.proto`, «clipboard over local IPC»). The two directions are not
//! symmetric: `ClipboardChanged` also carries `size_bytes` on every chunk (the *agent* is the
//! one that knows the size of what it copied, and core needs it to verify the assembly),
//! while `SetClipboardText` does not (core's own framing knows it, so a receiver gets it from
//! the chunk count and the 1 MiB limit instead).
//!
//! [`clipboard`]'s `Assembler` is the same idea for the *network* channel, and it is not
//! reusable here: it frames 16 KiB chunks, allows `1..=64` of them, keys a transfer by a
//! 16-byte offer id and caps the payload at what the offer promised. The IPC's numbers are
//! different (`32 KiB`, `1..=64`) and the key is the sender's `origin_version`, so this module
//! is the small equivalent that speaks the IPC's own numbers — deliberately still pure, so the
//! framing and the assembly are unit-tested on any host.

use std::ops::Range;

use clipboard::{Marker, MAX_BYTES};
use protocol::ipc;

/// The largest payload one frame carries (`ipc.proto`: «`data`: UTF-8 text, <= 32768 bytes»).
pub const CHUNK_BYTES: usize = 32768;
/// `chunk_count` is `1..=64` on this channel — the same range the clipboard channel uses.
/// 64 × 32 KiB reaches 2 MiB, deliberately more than the 1 MiB the policy refuses: a copy over
/// the limit is still *reported*, so core can answer `CLIPBOARD_TOO_LARGE` instead of the agent
/// silently dropping it.
pub const MAX_CHUNKS: u32 = 64;

/// The marker name this one travels as (`ipc.proto`:
/// `exclude_from_monitor | no_history | viewer_ignore | kde_secret | portal_secret`).
pub fn marker_name(marker: Marker) -> &'static str {
    match marker {
        Marker::ExcludeFromMonitor => "exclude_from_monitor",
        Marker::NoHistory => "no_history",
        Marker::ViewerIgnore => "viewer_ignore",
        Marker::KdeSecret => "kde_secret",
        Marker::PortalSecret => "portal_secret",
    }
}

/// The marker a wire name stands for, or `None` for a name this build does not know.
///
/// The inverse of [`marker_name`], used by the dry clipboard's `in.markers` file so a smoke can
/// carry exactly the vocabulary the frame carries; a name outside the five documented ones is
/// ignored rather than guessed at.
pub fn marker_from_name(name: &str) -> Option<Marker> {
    match name {
        "exclude_from_monitor" => Some(Marker::ExcludeFromMonitor),
        "no_history" => Some(Marker::NoHistory),
        "viewer_ignore" => Some(Marker::ViewerIgnore),
        "kde_secret" => Some(Marker::KdeSecret),
        "portal_secret" => Some(Marker::PortalSecret),
        _ => None,
    }
}

/// How many chunks a payload of `size` bytes takes.
///
/// `ceil(size / 32768)`, and at least one: `chunk_count = 0` means "release the selection" in
/// `SetClipboardText`, so an empty payload an agent sends is one empty chunk, not zero chunks.
pub fn chunk_count(size: usize) -> u32 {
    (size.div_ceil(CHUNK_BYTES).max(1)) as u32
}

/// The byte range chunk `index` carries; empty for an index past the payload.
pub fn chunk_range(size: usize, index: u32) -> Range<usize> {
    let start = (index as usize).saturating_mul(CHUNK_BYTES).min(size);
    let end = (start + CHUNK_BYTES).min(size);
    start..end
}

/// The `ClipboardChanged` frames for one payload, in order.
///
/// `origin_version` is this agent's own counter for this change; `size_bytes` and `markers`
/// travel on every chunk and `sha256` only on the last one, which is what lets core verify the
/// whole payload once instead of after every frame.
///
/// The caller bounds `text` to [`MAX_BYTES`]: a longer payload would need more than
/// [`MAX_CHUNKS`] frames and the protocol has no room for it.
pub fn frames(text: &str, origin_version: u64, markers: &[Marker]) -> Vec<ipc::ClipboardChanged> {
    let payload = text.as_bytes();
    let count = chunk_count(payload.len());
    debug_assert!(
        count <= MAX_CHUNKS,
        "the payload must be within the 1 MiB protocol limit before it is framed"
    );
    let names: Vec<String> = markers
        .iter()
        .map(|marker| marker_name(*marker).to_owned())
        .collect();
    let size_bytes = payload.len() as u64;
    let sha256 = clipboard::sha256(payload);
    (0..count)
        .map(|index| {
            let range = chunk_range(payload.len(), index);
            ipc::ClipboardChanged {
                origin_version,
                chunk_index: index,
                chunk_count: count,
                data: payload[range].to_vec(),
                size_bytes,
                markers: names.clone(),
                sha256: if index + 1 == count {
                    sha256.clone()
                } else {
                    Vec::new()
                },
            }
        })
        .collect()
}

/// Why an inbound `SetClipboardText` transfer was dropped.
///
/// [`InboundError::code`] is the `AgentError.code` core sees. The transfer is dropped and the
/// selection is not touched on any of these: unverified text is never applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundError {
    /// `chunk_count` outside `1..=64`.
    BadChunkCount,
    /// A chunk larger than 32768 bytes.
    ChunkTooLarge,
    /// A chunk arrived out of order, or a transfer was finished early.
    OutOfOrder,
    /// `origin_version` or `chunk_count` disagreed with the chunk that opened the transfer.
    KeyChanged,
    /// The last chunk's hash does not match the assembled payload.
    HashMismatch,
    /// The hash is missing where it is required, or the payload is not UTF-8.
    Invalid,
}

impl InboundError {
    /// The `AgentError.code` this becomes.
    pub const fn code(self) -> &'static str {
        match self {
            InboundError::HashMismatch => "CLIPBOARD_HASH_MISMATCH",
            InboundError::BadChunkCount
            | InboundError::ChunkTooLarge
            | InboundError::OutOfOrder
            | InboundError::KeyChanged
            | InboundError::Invalid => "CLIPBOARD_INVALID",
        }
    }

    /// Payload-free, actionable text for the error frame.
    pub const fn detail(self) -> &'static str {
        match self {
            InboundError::BadChunkCount => "chunk_count is outside 1..=64",
            InboundError::ChunkTooLarge => "a chunk carried more than 32768 bytes",
            InboundError::OutOfOrder => "a chunk arrived out of order",
            InboundError::KeyChanged => "the chunks disagree about version or count",
            InboundError::HashMismatch => "the whole-payload sha256 does not match",
            InboundError::Invalid => "the payload is not the promised UTF-8 text",
        }
    }
}

/// The state of a transfer after one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// More chunks are expected.
    NeedMore,
    /// The last chunk arrived; the hash on it is what finishes the transfer.
    Ready,
}

/// Assembles the chunks of one inbound `SetClipboardText` transfer.
///
/// Keyed by the sender's `origin_version`: the chunk that opens a transfer fixes version and
/// count, and every later chunk has to agree — a transfer that changes key mid-flight is
/// dropped, not restarted silently.
///
/// Two limits make the assembled payload bounded without a size check of its own: at most
/// [`MAX_CHUNKS`] chunks of at most [`CHUNK_BYTES`] each is exactly [`MAX_BYTES`], so nothing
/// larger can be assembled from frames that pass [`Inbound::push`].
#[derive(Debug)]
pub struct Inbound {
    origin_version: u64,
    count: u32,
    next: u32,
    payload: Vec<u8>,
}

impl Inbound {
    /// Opens a transfer with its first chunk.
    pub fn start(frame: &ipc::SetClipboardText) -> Result<Self, InboundError> {
        debug_assert!(MAX_CHUNKS as u64 * CHUNK_BYTES as u64 >= MAX_BYTES);
        if frame.chunk_count == 0 || frame.chunk_count > MAX_CHUNKS {
            return Err(InboundError::BadChunkCount);
        }
        if frame.data.len() > CHUNK_BYTES {
            return Err(InboundError::ChunkTooLarge);
        }
        Ok(Inbound {
            origin_version: frame.origin_version,
            count: frame.chunk_count,
            next: 0,
            payload: Vec::new(),
        })
    }

    /// The version this transfer is assembling.
    pub fn origin_version(&self) -> u64 {
        self.origin_version
    }

    /// Whether `frame` belongs to this transfer (same version *and* same count).
    pub fn accepts(&self, frame: &ipc::SetClipboardText) -> bool {
        frame.origin_version == self.origin_version && frame.chunk_count == self.count
    }

    /// Takes the next chunk.
    pub fn push(&mut self, frame: &ipc::SetClipboardText) -> Result<Progress, InboundError> {
        if !self.accepts(frame) {
            return Err(InboundError::KeyChanged);
        }
        if frame.data.len() > CHUNK_BYTES {
            return Err(InboundError::ChunkTooLarge);
        }
        if frame.chunk_index != self.next {
            return Err(InboundError::OutOfOrder);
        }
        self.payload.extend_from_slice(&frame.data);
        self.next += 1;
        if self.next == self.count {
            Ok(Progress::Ready)
        } else {
            Ok(Progress::NeedMore)
        }
    }

    /// The text the last chunk finishes, or why it is not applied.
    ///
    /// The last chunk is the only one that may finish a transfer, its hash covers the whole
    /// payload, and the assembled bytes have to be valid UTF-8 — the checks
    /// `docs/05-protocol.md` puts before a selection is set («проверка sha256 и UTF-8 до
    /// установки selection»).
    pub fn finish(self, last: &ipc::SetClipboardText) -> Result<String, InboundError> {
        if self.next != self.count || last.chunk_index + 1 != self.count {
            return Err(InboundError::OutOfOrder);
        }
        if last.sha256.len() != 32 {
            return Err(InboundError::Invalid);
        }
        if clipboard::sha256(&self.payload) != last.sha256 {
            return Err(InboundError::HashMismatch);
        }
        String::from_utf8(self.payload).map_err(|_| InboundError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `SetClipboardText`, with the hash only where the protocol puts it.
    fn chunk(
        version: u64,
        index: u32,
        count: u32,
        payload: &[u8],
        hash: Option<&[u8]>,
    ) -> ipc::SetClipboardText {
        let mut frame = ipc::SetClipboardText {
            origin_version: version,
            chunk_index: index,
            chunk_count: count,
            data: payload.to_vec(),
            sha256: Vec::new(),
        };
        if index + 1 == count {
            frame.sha256 = hash.map(<[u8]>::to_vec).unwrap_or_default();
        }
        frame
    }

    /// The frames of `text`, as `SetClipboardText` (the agent's own framing, read back).
    fn outbound(text: &str, version: u64) -> Vec<ipc::SetClipboardText> {
        frames(text, version, &[])
            .into_iter()
            .map(|frame| ipc::SetClipboardText {
                origin_version: frame.origin_version,
                chunk_index: frame.chunk_index,
                chunk_count: frame.chunk_count,
                data: frame.data,
                sha256: frame.sha256,
            })
            .collect()
    }

    /// Drives the whole inbound path for a payload, in order.
    fn assemble(version: u64, text: &str) -> Result<String, InboundError> {
        let frames = outbound(text, version);
        let mut inbound = Inbound::start(&frames[0])?;
        for (index, frame) in frames.iter().enumerate() {
            let progress = inbound.push(frame)?;
            if index + 1 == frames.len() {
                assert_eq!(progress, Progress::Ready);
                return inbound.finish(frame);
            }
            assert_eq!(progress, Progress::NeedMore);
        }
        unreachable!("the chunk plan is never empty")
    }

    #[test]
    fn the_chunk_count_is_the_documented_ceiling() {
        assert_eq!(chunk_count(0), 1, "an empty payload is one empty chunk");
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_BYTES), 1);
        assert_eq!(chunk_count(CHUNK_BYTES + 1), 2);
        assert_eq!(
            chunk_count(MAX_BYTES as usize),
            32,
            "1 MiB is 32 frames of 32 KiB"
        );
        assert_eq!(
            MAX_CHUNKS as usize * CHUNK_BYTES,
            2 * MAX_BYTES as usize,
            "the ceiling is 2 MiB on purpose: past the 1 MiB the policy refuses, so a copy over \
             the limit still has to be *reported* rather than dropped here"
        );
        assert_eq!(chunk_range(100, 0), 0..100);
        assert_eq!(chunk_range(100, 1), 100..100);
    }

    /// A 1 MiB payload is 32 chunks, none larger than 32768 bytes, and the hash travels on
    /// the last one only.
    #[test]
    fn one_mebibyte_is_thirty_two_chunks_and_only_the_last_carries_the_hash() {
        let text = "a".repeat(MAX_BYTES as usize);
        let frames = frames(&text, 9, &[]);
        assert_eq!(frames.len(), 32);
        let hash = clipboard::sha256(text.as_bytes());
        for (index, frame) in frames.iter().enumerate() {
            assert!(frame.data.len() <= CHUNK_BYTES, "chunk {index} is too big");
            assert_eq!(frame.chunk_index, index as u32);
            assert_eq!(frame.chunk_count, 32);
            assert_eq!(frame.size_bytes, MAX_BYTES);
            assert_eq!(frame.origin_version, 9);
            if index + 1 == frames.len() {
                assert_eq!(frame.sha256, hash, "the last chunk carries the hash");
            } else {
                assert!(frame.sha256.is_empty(), "chunk {index} must carry no hash");
            }
        }
        // The chunks are exactly the payload, in order.
        let joined: Vec<u8> = frames.iter().flat_map(|frame| frame.data.clone()).collect();
        assert_eq!(joined, text.as_bytes());
        // And they round-trip through the inbound side.
        assert_eq!(assemble(9, &text).expect("1 MiB"), text);
    }

    #[test]
    fn an_empty_payload_is_one_empty_chunk() {
        let frames = frames("", 1, &[]);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].data.is_empty());
        assert_eq!(frames[0].chunk_count, 1);
        assert_eq!(frames[0].size_bytes, 0);
        assert_eq!(frames[0].sha256, clipboard::sha256(b""));
        assert_eq!(assemble(1, "").expect("empty"), "");
    }

    #[test]
    fn the_markers_travel_on_every_chunk_with_their_wire_names() {
        let markers = [Marker::KdeSecret, Marker::ExcludeFromMonitor];
        let every = frames("x", 3, &markers);
        for frame in &every {
            assert_eq!(frame.markers, vec!["kde_secret", "exclude_from_monitor"]);
        }
        // The names are the ones the proto comment documents, in the enum's own order.
        let all = [
            Marker::ExcludeFromMonitor,
            Marker::NoHistory,
            Marker::ViewerIgnore,
            Marker::KdeSecret,
            Marker::PortalSecret,
        ];
        let names: Vec<&str> = all.iter().map(|marker| marker_name(*marker)).collect();
        assert_eq!(
            names,
            vec![
                "exclude_from_monitor",
                "no_history",
                "viewer_ignore",
                "kde_secret",
                "portal_secret"
            ]
        );
        // The mapping is exact in both directions, and an unknown name is not guessed at.
        for marker in all {
            assert_eq!(marker_from_name(marker_name(marker)), Some(marker));
        }
        assert_eq!(marker_from_name("kde-secret"), None);
        assert_eq!(marker_from_name(""), None);
    }

    #[test]
    fn a_chunked_payload_round_trips_including_multibyte_text() {
        for text in ["", "hello", "привет, мир", &"x".repeat(100 * 1024)] {
            assert_eq!(assemble(4, text).expect("assembled"), text);
        }
    }

    /// The required one: a chunk out of sequence is refused and nothing is applied.
    #[test]
    fn an_out_of_sequence_chunk_is_refused() {
        let text = "abcdefgh";
        // Two chunks instead of one, so there is an order to get wrong.
        let first = chunk(5, 0, 2, b"abcd", None);
        let second = chunk(5, 1, 2, b"efgh", Some(&clipboard::sha256(b"abcdefgh")));

        let mut inbound = Inbound::start(&first).expect("a valid opening chunk");
        // Index 1 while index 0 is expected: the transfer is dropped where it is.
        assert_eq!(inbound.push(&second).unwrap_err(), InboundError::OutOfOrder);
        assert_eq!(InboundError::OutOfOrder.code(), "CLIPBOARD_INVALID");
        // A transfer may not be finished before its last chunk arrived.
        let inbound = Inbound::start(&first).expect("a valid opening chunk");
        assert_eq!(
            inbound.finish(&first).unwrap_err(),
            InboundError::OutOfOrder
        );

        // The same frames in the right order do apply.
        let mut inbound = Inbound::start(&first).expect("a valid opening chunk");
        assert_eq!(inbound.push(&first).expect("chunk 0"), Progress::NeedMore);
        assert_eq!(inbound.push(&second).expect("chunk 1"), Progress::Ready);
        assert_eq!(inbound.finish(&second).expect("verified"), text);
    }

    /// The other required one: a chunk whose hash does not cover the payload is refused.
    #[test]
    fn a_mismatched_sha256_is_refused_and_nothing_is_applied() {
        let text = "секрет";
        let good = chunk(
            7,
            0,
            1,
            text.as_bytes(),
            Some(&clipboard::sha256(text.as_bytes())),
        );
        let mut inbound = Inbound::start(&good).expect("valid opening chunk");
        assert_eq!(inbound.push(&good).expect("one chunk"), Progress::Ready);
        assert_eq!(inbound.finish(&good).expect("verified"), text);

        let mut wrong = good.clone();
        wrong.sha256 = clipboard::sha256(b"another payload");
        let mut inbound = Inbound::start(&wrong).expect("valid opening chunk");
        inbound.push(&wrong).expect("one chunk");
        assert_eq!(
            inbound.finish(&wrong).unwrap_err(),
            InboundError::HashMismatch
        );
        assert_eq!(InboundError::HashMismatch.code(), "CLIPBOARD_HASH_MISMATCH");

        // A missing hash on the last chunk is refused too, with no panic.
        let mut missing = good.clone();
        missing.sha256 = Vec::new();
        let mut inbound = Inbound::start(&missing).expect("valid opening chunk");
        inbound.push(&missing).expect("one chunk");
        assert_eq!(inbound.finish(&missing).unwrap_err(), InboundError::Invalid);
    }

    #[test]
    fn invalid_utf8_is_refused_even_with_the_right_hash() {
        let data = vec![0xffu8, 0xfe];
        let frame = chunk(1, 0, 1, &data, Some(&clipboard::sha256(&data)));
        let mut inbound = Inbound::start(&frame).expect("valid");
        inbound.push(&frame).expect("one chunk");
        assert_eq!(inbound.finish(&frame).unwrap_err(), InboundError::Invalid);
        assert_eq!(InboundError::Invalid.code(), "CLIPBOARD_INVALID");
    }

    #[test]
    fn every_limit_is_enforced_before_anything_is_allocated() {
        assert_eq!(
            Inbound::start(&chunk(1, 0, 0, b"abcd", None)).unwrap_err(),
            InboundError::BadChunkCount,
            "chunk_count 0 is a release, not a transfer"
        );
        assert_eq!(
            Inbound::start(&chunk(1, 0, MAX_CHUNKS + 1, b"abcd", None)).unwrap_err(),
            InboundError::BadChunkCount
        );
        assert_eq!(
            Inbound::start(&chunk(1, 0, 1, &vec![0u8; CHUNK_BYTES + 1], None)).unwrap_err(),
            InboundError::ChunkTooLarge
        );

        // The 1 MiB boundary: 32 chunks of 32 KiB assemble into exactly the payload the
        // policy still sends.
        const ONE_MIB_CHUNKS: u32 = 32;
        let chunked = chunk(1, 0, ONE_MIB_CHUNKS, &vec![b'a'; CHUNK_BYTES], None);
        let mut inbound = Inbound::start(&chunked).expect("the first chunk is within every limit");
        assert_eq!(
            inbound.push(&chunked).expect("chunk 0"),
            Progress::NeedMore,
            "31 more chunks are expected"
        );
        for index in 1..ONE_MIB_CHUNKS {
            inbound
                .push(&chunk(1, index, ONE_MIB_CHUNKS, &vec![b'a'; CHUNK_BYTES], None))
                .expect("still within the limit");
        }
        let hash = clipboard::sha256(&vec![b'a'; MAX_BYTES as usize]);
        let last = chunk(
            1,
            ONE_MIB_CHUNKS - 1,
            ONE_MIB_CHUNKS,
            &vec![b'a'; CHUNK_BYTES],
            Some(&hash),
        );
        assert_eq!(
            inbound.finish(&last).expect("1 MiB of UTF-8").len(),
            MAX_BYTES as usize
        );
    }

    #[test]
    fn a_transfer_that_changes_its_key_mid_flight_is_dropped() {
        let first = chunk(2, 0, 2, b"ab", None);
        let mut inbound = Inbound::start(&first).expect("valid");
        inbound.push(&first).expect("chunk 0");
        // A different version on the next chunk is not "the rest of a transfer".
        assert_eq!(
            inbound.push(&chunk(3, 1, 2, b"cd", None)).unwrap_err(),
            InboundError::KeyChanged
        );
        // Neither is a different chunk count.
        assert_eq!(
            inbound.push(&chunk(2, 1, 3, b"cd", None)).unwrap_err(),
            InboundError::KeyChanged
        );
        // With the key it opened with, the rest of the transfer still goes through.
        assert_eq!(
            inbound
                .push(&chunk(2, 1, 2, b"cd", Some(&clipboard::sha256(b"abcd"))))
                .expect("chunk 1"),
            Progress::Ready
        );
    }
}
