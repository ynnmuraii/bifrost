//! Post-decode limit checks.
//!
//! `05-protocol.md`: «После decode проверять численные пределы, oneof, IDs,
//! finite coordinates и capabilities; корректный Protobuf ещё не валидное
//! сообщение приложения. Пустой `payload` (все поля oneof неизвестны
//! отправителю) — это `ERROR_CODE_INVALID_FRAME`.»
//!
//! This module checks only the *sizes and identifier lengths* the wire contract
//! fixes — the part that belongs to the framing/limits layer. Oneof handling is
//! [`Envelope::payload`] being `Some`; coordinate, topology and capability
//! semantics belong to the lease FSM and to `geometry`.

use crate::limits::{
    CONNECTION_ID_LEN, LEASE_ID_LEN, MAX_CLIPBOARD_CHUNK_BYTES, MAX_CLIPBOARD_CHUNK_COUNT,
    MAX_CLIPBOARD_IMAGE_PAYLOAD_BYTES, MAX_CLIPBOARD_PAYLOAD_BYTES, MAX_DISPLAY_NAME_BYTES,
    MAX_HELLO_DISPLAYS, MAX_INPUT_BATCH_EVENTS, MIN_CLIPBOARD_CHUNK_COUNT, MIN_INPUT_BATCH_EVENTS,
    NODE_ID_LEN,
};
use crate::wire;

/// A decoded message that is structurally valid Protobuf but violates a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// `Envelope.payload` is `None`: no oneof arm the sender knew about.
    EmptyPayload,
    /// `connection_id` is not exactly 16 bytes.
    ConnectionIdLen { len: usize },
    /// `lease_id` is neither empty nor exactly 16 bytes.
    LeaseIdLen { len: usize },
    /// `node_id`/`owner_node_id` is not exactly 32 bytes.
    NodeIdLen { len: usize },
    /// `Hello.display_name` exceeds 128 UTF-8 bytes.
    DisplayNameTooLong { len: usize },
    /// `Hello.displays` exceeds 16 entries.
    TooManyDisplays { got: usize },
    /// `InputBatch.events` is outside 1..=128.
    InputBatchSize { got: usize },
    /// `ClipboardOffer.size_bytes` exceeds its MIME's hard cap.
    ClipboardPayloadTooLarge { got: u64 },
    /// Only the two exact, supported MIME strings are valid.
    ClipboardMime,
    /// `ClipboardData.data` exceeds 16 KiB.
    ClipboardChunkTooLarge { got: usize },
    /// `ClipboardData.chunk_count` is outside 1..=2048 (stateless ceiling).
    ClipboardChunkCount { got: u32 },
    /// `ClipboardData.chunk_index` is not below `chunk_count`.
    ClipboardChunkIndex { index: u32, count: u32 },
}

impl Violation {
    /// The wire [`wire::ErrorCode`] the receiver answers with.
    pub const fn error_code(self) -> wire::ErrorCode {
        match self {
            Violation::ClipboardPayloadTooLarge { .. }
            | Violation::ClipboardChunkTooLarge { .. } => wire::ErrorCode::ClipboardTooLarge,
            Violation::EmptyPayload
            | Violation::ConnectionIdLen { .. }
            | Violation::LeaseIdLen { .. }
            | Violation::NodeIdLen { .. }
            | Violation::DisplayNameTooLong { .. }
            | Violation::TooManyDisplays { .. }
            | Violation::InputBatchSize { .. }
            | Violation::ClipboardMime
            | Violation::ClipboardChunkCount { .. }
            | Violation::ClipboardChunkIndex { .. } => wire::ErrorCode::InvalidFrame,
        }
    }
}

/// Check the framing-level limits of a decoded [`wire::Envelope`]: a non-empty
/// oneof and the identifier lengths.
///
/// `lease_id` is required to be either empty or exactly 16 bytes; which messages
/// must carry a lease is the lease FSM's rule (KVM-032), not the framing layer's.
pub fn envelope(envelope: &wire::Envelope) -> Result<(), Violation> {
    if envelope.payload.is_none() {
        return Err(Violation::EmptyPayload);
    }
    if envelope.connection_id.len() != CONNECTION_ID_LEN {
        return Err(Violation::ConnectionIdLen {
            len: envelope.connection_id.len(),
        });
    }
    let lease_len = envelope.lease_id.len();
    if lease_len != 0 && lease_len != LEASE_ID_LEN {
        return Err(Violation::LeaseIdLen { len: lease_len });
    }
    Ok(())
}

/// Check `Hello` identifier and length limits.
pub fn hello(hello: &wire::Hello) -> Result<(), Violation> {
    if hello.node_id.len() != NODE_ID_LEN {
        return Err(Violation::NodeIdLen {
            len: hello.node_id.len(),
        });
    }
    let name_len = hello.display_name.len();
    if name_len > MAX_DISPLAY_NAME_BYTES {
        return Err(Violation::DisplayNameTooLong { len: name_len });
    }
    if hello.displays.len() > MAX_HELLO_DISPLAYS {
        return Err(Violation::TooManyDisplays {
            got: hello.displays.len(),
        });
    }
    Ok(())
}

/// Check the `InputBatch` event count (1..=128).
pub fn input_batch(batch: &wire::InputBatch) -> Result<(), Violation> {
    let got = batch.events.len();
    if !(MIN_INPUT_BATCH_EVENTS..=MAX_INPUT_BATCH_EVENTS).contains(&got) {
        return Err(Violation::InputBatchSize { got });
    }
    Ok(())
}

/// Check exact MIME and its hard payload cap before allocating.
pub fn clipboard_offer(offer: &wire::ClipboardOffer) -> Result<(), Violation> {
    let max = match offer.mime.as_str() {
        "text/plain;charset=utf-8" => MAX_CLIPBOARD_PAYLOAD_BYTES,
        "image/png" => MAX_CLIPBOARD_IMAGE_PAYLOAD_BYTES,
        _ => return Err(Violation::ClipboardMime),
    };
    if offer.size_bytes > max {
        return Err(Violation::ClipboardPayloadTooLarge {
            got: offer.size_bytes,
        });
    }
    Ok(())
}

/// Check stateless `ClipboardData` count (1..=2048), index and chunk size.
/// (≤ 16 KiB).
pub fn clipboard_data(data: &wire::ClipboardData) -> Result<(), Violation> {
    if !(MIN_CLIPBOARD_CHUNK_COUNT..=MAX_CLIPBOARD_CHUNK_COUNT).contains(&data.chunk_count) {
        return Err(Violation::ClipboardChunkCount {
            got: data.chunk_count,
        });
    }
    if data.chunk_index >= data.chunk_count {
        return Err(Violation::ClipboardChunkIndex {
            index: data.chunk_index,
            count: data.chunk_count,
        });
    }
    if data.data.len() > MAX_CLIPBOARD_CHUNK_BYTES {
        return Err(Violation::ClipboardChunkTooLarge {
            got: data.data.len(),
        });
    }
    Ok(())
}
