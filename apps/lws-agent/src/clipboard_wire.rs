//! The clipboard's IPC wire form: chunking out, assembly in.
//!
//! Local IPC uses bounded 32 KiB chunks, exact MIME and size metadata, and a final digest.
//! Legacy messages with empty MIME and no size remain text-only.

use std::ops::Range;
use std::sync::Arc;

#[cfg(test)]
use clipboard::MAX_BYTES;
use clipboard::{ClipboardContent, ContentKind, Marker};
use protocol::ipc;

/// The largest payload one IPC frame carries.
pub const CHUNK_BYTES: usize = 32768;
/// Text allows at most 64 IPC chunks; PNG uses its separate limit below.
/// 64 × 32 KiB reaches 2 MiB, deliberately more than the 1 MiB the policy refuses: a copy over
/// the limit is still *reported*, so core can answer `CLIPBOARD_TOO_LARGE` instead of the agent
/// silently dropping it.
pub const MAX_CHUNKS: u32 = 64;
pub const MAX_IMAGE_CHUNKS: u32 = 1024;

pub fn max_chunks(kind: ContentKind) -> u32 {
    match kind {
        ContentKind::Text => MAX_CHUNKS,
        ContentKind::Png => MAX_IMAGE_CHUNKS,
    }
}

fn frame_kind(mime: &str) -> Result<ContentKind, InboundError> {
    if mime.is_empty() {
        Ok(ContentKind::Text)
    } else {
        ContentKind::from_mime(mime).map_err(|_| InboundError::Invalid)
    }
}

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
/// The caller bounds the content to its agent-to-core framing ceiling.
pub fn frames(
    content: Arc<ClipboardContent>,
    origin_version: u64,
    markers: &[Marker],
) -> impl ExactSizeIterator<Item = ipc::ClipboardChanged> + use<> {
    let payload = content.as_bytes();
    let count = chunk_count(payload.len());
    debug_assert!(count <= max_chunks(content.kind()));
    let names: Vec<String> = markers
        .iter()
        .map(|marker| marker_name(*marker).to_owned())
        .collect();
    let size_bytes = payload.len() as u64;
    let sha256 = clipboard::sha256(payload);
    (0..count).map(move |index| {
        let payload = content.as_bytes();
        let range = chunk_range(payload.len(), index);
        ipc::ClipboardChanged {
            origin_version,
            chunk_index: index,
            chunk_count: count,
            data: payload[range].to_vec(),
            size_bytes,
            mime: content.kind().mime().to_owned(),
            markers: names.clone(),
            sha256: if index + 1 == count {
                sha256.clone()
            } else {
                Vec::new()
            },
        }
    })
}

/// Why an inbound `SetClipboardText` transfer was dropped.
///
/// [`InboundError::code`] is the `AgentError.code` core sees. The transfer is dropped and the
/// selection is not touched on any of these: unverified content is never applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundError {
    /// `chunk_count` outside the content kind's IPC range.
    BadChunkCount,
    /// A chunk larger than 32768 bytes.
    ChunkTooLarge,
    /// The assembled apply payload exceeds the content kind's hard limit.
    TooLarge,
    /// A chunk arrived out of order, or a transfer was finished early.
    OutOfOrder,
    /// `origin_version` or `chunk_count` disagreed with the chunk that opened the transfer.
    KeyChanged,
    /// The last chunk's hash does not match the assembled payload.
    HashMismatch,
    /// Invalid MIME, size, hash placement, UTF-8 or PNG signature.
    Invalid,
}

impl InboundError {
    /// The `AgentError.code` this becomes.
    pub const fn code(self) -> &'static str {
        match self {
            InboundError::HashMismatch => "CLIPBOARD_HASH_MISMATCH",
            InboundError::TooLarge => "CLIPBOARD_TOO_LARGE",
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
            InboundError::BadChunkCount => "chunk_count exceeds the content kind limit",
            InboundError::ChunkTooLarge => "a chunk carried more than 32768 bytes",
            InboundError::TooLarge => "the clipboard apply payload exceeds its kind limit",
            InboundError::OutOfOrder => "a chunk arrived out of order",
            InboundError::KeyChanged => "the chunks disagree about version, count, MIME or size",
            InboundError::HashMismatch => "the whole-payload sha256 does not match",
            InboundError::Invalid => "the clipboard metadata or payload is invalid",
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
/// The opening chunk fixes version, count, MIME and optional size. Later chunks must agree;
/// changing metadata never silently restarts an existing transfer.
///
/// Apply has its own cumulative cap: the larger agent-to-core framing ceiling permits
/// reporting oversized copies, not applying them to the local selection.
pub struct Inbound {
    origin_version: u64,
    count: u32,
    kind: ContentKind,
    mime: String,
    size: Option<u64>,
    next: u32,
    payload: Vec<u8>,
}

impl std::fmt::Debug for Inbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inbound")
            .field("kind", &self.kind)
            .field("len", &self.payload.len())
            .finish()
    }
}

impl Inbound {
    pub fn kind(&self) -> ContentKind {
        self.kind
    }

    pub fn origin_version(&self) -> u64 {
        self.origin_version
    }

    /// Opens a transfer with its first chunk.
    pub fn start(frame: &ipc::SetClipboardText) -> Result<Self, InboundError> {
        let kind = frame_kind(&frame.mime)?;
        if kind == ContentKind::Png && frame.size_bytes.is_none() {
            return Err(InboundError::Invalid);
        }
        if frame
            .size_bytes
            .is_some_and(|size| size > kind.hard_max_bytes())
        {
            return Err(InboundError::TooLarge);
        }
        if frame.chunk_count == 0 || frame.chunk_count > max_chunks(kind) {
            return Err(InboundError::BadChunkCount);
        }
        if frame.data.len() > CHUNK_BYTES {
            return Err(InboundError::ChunkTooLarge);
        }
        Ok(Inbound {
            origin_version: frame.origin_version,
            count: frame.chunk_count,
            kind,
            mime: frame.mime.clone(),
            size: frame.size_bytes,
            next: 0,
            payload: Vec::new(),
        })
    }

    /// Whether `frame` agrees with all metadata fixed by the opening chunk.
    pub fn accepts(&self, frame: &ipc::SetClipboardText) -> bool {
        frame.origin_version == self.origin_version
            && frame.chunk_count == self.count
            && frame.mime == self.mime
            && frame.size_bytes == self.size
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
        let limit = self
            .size
            .unwrap_or(self.kind.hard_max_bytes())
            .min(self.kind.hard_max_bytes());
        if frame.data.len() as u64 > limit.saturating_sub(self.payload.len() as u64) {
            return Err(InboundError::TooLarge);
        }
        if (self.next + 1 == self.count && frame.sha256.len() != 32)
            || (self.next + 1 != self.count && !frame.sha256.is_empty())
        {
            return Err(InboundError::Invalid);
        }
        self.payload.extend_from_slice(&frame.data);
        self.next += 1;
        if self.next == self.count {
            Ok(Progress::Ready)
        } else {
            Ok(Progress::NeedMore)
        }
    }

    /// Checks exact declared size, final digest, and the content kind's byte representation.
    pub fn finish(self, last: &ipc::SetClipboardText) -> Result<ClipboardContent, InboundError> {
        if !self.accepts(last) {
            return Err(InboundError::KeyChanged);
        }
        if self
            .size
            .is_some_and(|size| size != self.payload.len() as u64)
        {
            return Err(InboundError::Invalid);
        }
        if self.next != self.count || last.chunk_index.checked_add(1) != Some(self.count) {
            return Err(InboundError::OutOfOrder);
        }
        if last.sha256.len() != 32 {
            return Err(InboundError::Invalid);
        }
        if clipboard::sha256(&self.payload) != last.sha256 {
            return Err(InboundError::HashMismatch);
        }
        ClipboardContent::from_bytes(self.kind, self.payload).map_err(|_| InboundError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(
        content: &ClipboardContent,
        origin_version: u64,
        markers: &[Marker],
    ) -> Vec<ipc::ClipboardChanged> {
        super::frames(Arc::new(content.clone()), origin_version, markers).collect()
    }

    fn png_frame(payload: &[u8]) -> ipc::SetClipboardText {
        ipc::SetClipboardText {
            origin_version: 1,
            chunk_index: 0,
            chunk_count: 1,
            data: payload.to_vec(),
            sha256: clipboard::sha256(payload),
            mime: ContentKind::Png.mime().to_owned(),
            size_bytes: Some(payload.len() as u64),
        }
    }

    #[test]
    fn png_round_trips_exact_bytes_and_debug_never_contains_payload() {
        let png = b"\x89PNG\r\n\x1a\nprivate-image-payload\xff\0";
        let frame = png_frame(png);
        let mut inbound = Inbound::start(&frame).unwrap();
        inbound.push(&frame).unwrap();
        let debug = format!("{inbound:?}");
        assert!(debug.contains("Png"));
        assert!(!debug.contains("private"));
        let content = inbound.finish(&frame).unwrap();
        assert_eq!(content.kind(), ContentKind::Png);
        assert_eq!(content.as_bytes(), png);
        assert!(!format!("{content:?}").contains("private"));
        let changed = frames(&content, 2, &[]);
        assert_eq!(changed[0].mime, "image/png");
        assert_eq!(changed[0].size_bytes, png.len() as u64);
        assert_eq!(changed[0].data, png);
        assert_eq!(changed[0].sha256, clipboard::sha256(png));
    }

    #[test]
    fn mime_size_and_hash_metadata_are_strict_on_every_chunk() {
        let mut frame = png_frame(b"\x89PNG\r\n\x1a\n");
        frame.mime = "image/jpeg".to_owned();
        assert_eq!(Inbound::start(&frame).unwrap_err(), InboundError::Invalid);
        frame.mime = "image/png".to_owned();
        frame.size_bytes = None;
        assert_eq!(Inbound::start(&frame).unwrap_err(), InboundError::Invalid);
        frame.size_bytes = Some(ContentKind::Png.hard_max_bytes() + 1);
        assert_eq!(Inbound::start(&frame).unwrap_err(), InboundError::TooLarge);
        frame.size_bytes = Some(9);
        let mut inbound = Inbound::start(&frame).unwrap();
        inbound.push(&frame).unwrap();
        assert_eq!(inbound.finish(&frame).unwrap_err(), InboundError::Invalid);
        frame.size_bytes = Some(8);
        frame.chunk_count = 2;
        let mut inbound = Inbound::start(&frame).unwrap();
        assert_eq!(inbound.push(&frame).unwrap_err(), InboundError::Invalid);
        frame.sha256.clear();
        inbound.push(&frame).unwrap();
        let mut next = frame.clone();
        next.chunk_index = 1;
        next.data.clear();
        next.sha256 = clipboard::sha256(&frame.data);
        next.mime.clear();
        assert_eq!(inbound.push(&next).unwrap_err(), InboundError::KeyChanged);
        next.mime = frame.mime.clone();
        next.size_bytes = None;
        assert_eq!(inbound.push(&next).unwrap_err(), InboundError::KeyChanged);
    }

    #[test]
    fn png_signature_hash_and_kind_chunk_limits_are_enforced() {
        let invalid = png_frame(b"not png!");
        let mut inbound = Inbound::start(&invalid).unwrap();
        inbound.push(&invalid).unwrap();
        assert_eq!(inbound.finish(&invalid).unwrap_err(), InboundError::Invalid);
        let mut frame = png_frame(b"\x89PNG\r\n\x1a\n");
        frame.sha256 = clipboard::sha256(b"wrong");
        let mut inbound = Inbound::start(&frame).unwrap();
        inbound.push(&frame).unwrap();
        assert_eq!(
            inbound.finish(&frame).unwrap_err(),
            InboundError::HashMismatch
        );
        for count in [MAX_CHUNKS + 1, MAX_IMAGE_CHUNKS] {
            frame.chunk_count = count;
            assert!(Inbound::start(&frame).is_ok());
        }
        frame.chunk_count = MAX_IMAGE_CHUNKS + 1;
        assert_eq!(
            Inbound::start(&frame).unwrap_err(),
            InboundError::BadChunkCount
        );
        frame.chunk_count = MAX_CHUNKS + 1;
        frame.mime = ContentKind::Text.mime().to_owned();
        assert_eq!(
            Inbound::start(&frame).unwrap_err(),
            InboundError::BadChunkCount
        );
    }

    #[test]
    fn maximum_png_ipc_payload_is_accepted_without_expanding_the_text_limit() {
        let mut payload = vec![0; ContentKind::Png.hard_max_bytes() as usize];
        payload[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let digest = clipboard::sha256(&payload);
        let mut frame = ipc::SetClipboardText {
            origin_version: 1,
            chunk_index: 0,
            chunk_count: MAX_IMAGE_CHUNKS,
            data: Vec::new(),
            sha256: Vec::new(),
            mime: ContentKind::Png.mime().to_owned(),
            size_bytes: Some(payload.len() as u64),
        };
        let mut inbound = Inbound::start(&frame).unwrap();
        for index in 0..MAX_IMAGE_CHUNKS {
            frame.chunk_index = index;
            frame.data = payload[chunk_range(payload.len(), index)].to_vec();
            if index + 1 == MAX_IMAGE_CHUNKS {
                frame.sha256 = digest.clone();
            }
            inbound.push(&frame).unwrap();
        }
        assert_eq!(inbound.finish(&frame).unwrap().as_bytes(), payload);
    }

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
            ..Default::default()
        };
        if index + 1 == count {
            frame.sha256 = hash.map(<[u8]>::to_vec).unwrap_or_default();
        }
        frame
    }

    /// The frames of `text`, as `SetClipboardText` (the agent's own framing, read back).
    fn outbound(text: &str, version: u64) -> Vec<ipc::SetClipboardText> {
        frames(&ClipboardContent::Text(text.to_owned()), version, &[])
            .into_iter()
            .map(|frame| ipc::SetClipboardText {
                origin_version: frame.origin_version,
                chunk_index: frame.chunk_index,
                chunk_count: frame.chunk_count,
                data: frame.data,
                sha256: frame.sha256,
                mime: frame.mime,
                size_bytes: Some(frame.size_bytes),
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
                return inbound.finish(frame).map(|content| match content {
                    ClipboardContent::Text(text) => text,
                    ClipboardContent::Image { .. } => unreachable!("text fixture"),
                });
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
        let frames = frames(&ClipboardContent::Text(text.clone()), 9, &[]);
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
        let frames = frames(&ClipboardContent::Text(String::new()), 1, &[]);
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
        let every = frames(&ClipboardContent::Text("x".to_owned()), 3, &markers);
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
        assert_eq!(
            inbound.finish(&second).expect("verified").as_bytes(),
            text.as_bytes()
        );
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
        assert_eq!(
            inbound.finish(&good).expect("verified").as_bytes(),
            text.as_bytes()
        );

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
        assert_eq!(inbound.push(&missing).unwrap_err(), InboundError::Invalid);
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
        let hash = clipboard::sha256(&vec![b'a'; MAX_BYTES as usize]);
        for index in 1..ONE_MIB_CHUNKS {
            inbound
                .push(&chunk(
                    1,
                    index,
                    ONE_MIB_CHUNKS,
                    &vec![b'a'; CHUNK_BYTES],
                    Some(&hash),
                ))
                .expect("still within the limit");
        }
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
    fn apply_refuses_the_first_byte_over_one_mebibyte() {
        let payload = vec![b'a'; CHUNK_BYTES];
        for count in [33, MAX_CHUNKS] {
            let first = chunk(1, 0, count, &payload, None);
            let mut inbound = Inbound::start(&first).expect("valid first chunk");
            for index in 0..32 {
                inbound
                    .push(&chunk(1, index, count, &payload, None))
                    .expect("one mebibyte is accepted");
            }
            assert_eq!(
                inbound.push(&chunk(1, 32, count, b"x", None)),
                Err(InboundError::TooLarge)
            );
            assert_eq!(inbound.payload.len(), MAX_BYTES as usize);
        }
        assert_eq!(InboundError::TooLarge.code(), "CLIPBOARD_TOO_LARGE");
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
