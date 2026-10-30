//! Protocol constants in one place: ports, ALPN identifiers, version and the
//! hard limits the wire format is not allowed to exceed.
//!
//! Every value is quoted from the design documents; the crate never invents a
//! limit. `docs/` is not part of the build, so the numbers live here and the
//! `.proto` copies live in `proto/`.
//!
//! * `docs/05-protocol.md`:
//!   * «Все узлы слушают два порта на своём настроенном LAN-интерфейсе — 47901
//!     input/control и 47902 clipboard».
//!   * «ALPN: `bifrost/input/1`, `bifrost/clipboard/1` и `bifrost/pair/1`.»
//!   * «Каждый фрейм: 4-byte unsigned big-endian длина + serialized `Envelope`
//!     Protobuf; для pairing — отдельный `PairingEnvelope` с лимитом 8192 bytes.
//!     Frame length input/control 1..65536 bytes. Не выделять память до проверки
//!     длины.»
//!   * «Payload ≤1 MiB, chunk ≤16 KiB, decoded frame ≤32 KiB».
//!   * «`InputBatch` содержит до 128 событий и только для Active lease.»
//!   * «жёсткий предел `N_max = 4`, `max_members ≤ 4`».
//! * `docs/ipc.proto`: «Framing: 4-byte unsigned big-endian length + serialized
//!   AgentFrame / GuiFrame. Max frame: 65536 bytes. Never sent over the network.»
//! * `docs/protocol.proto` comments: `node_id` exactly 32, `connection_id`
//!   exactly 16, `lease_id` exactly 16 on lease messages, `display_name` UTF-8
//!   ≤ 128 bytes, `Hello.displays` max 16, `ClipboardData.chunk_count` 1..64,
//!   `ClipboardOffer.size_bytes` ≤ 1048576.

/// TCP port for the input/control link. `05-protocol.md`.
pub const PORT_INPUT_CONTROL: u16 = 47901;
/// TCP port for clipboard links. `05-protocol.md`.
pub const PORT_CLIPBOARD: u16 = 47902;

/// ALPN for the input/control link. `05-protocol.md`.
///
/// The name changed in 0.4.0 (`lan-workspace/input/1` → `bifrost/input/1`)
/// together with the major bump, so 0.4.0 and 0.3.x refuse each other at the TLS
/// handshake — before either side can see the other's `Hello`.
pub const ALPN_INPUT: &str = "bifrost/input/1";
/// ALPN for the clipboard link. `05-protocol.md`.
pub const ALPN_CLIPBOARD: &str = "bifrost/clipboard/1";
/// ALPN for the pairing channel; carries no runtime payload. `05-protocol.md`.
pub const ALPN_PAIR: &str = "bifrost/pair/1";
/// Prompt-only pairing requests; never permits runtime traffic or trust changes.
pub const ALPN_PAIR_REQUEST: &str = "bifrost/pair-request/1";

/// Wire major version ("Сетевой протокол v1", 0.4.0). Both Hello versions must
/// be equal in `major`; the negotiated `minor` is `min` of both sides.
pub const PROTOCOL_MAJOR: u32 = 1;
/// Wire minor version.
///
/// Reset to 0 with the 0.4.0 major bump. `crates/transport/src/clipboard.rs`
/// still requires an exact major **and** minor, so a future minor bump still
/// breaks the clipboard channel until that check is capability-gated.
pub const PROTOCOL_MINOR: u32 = 0;

/// `Envelope` frame body limit on the input/control channel: 1..=65536 bytes.
pub const MAX_FRAME_INPUT_CONTROL: usize = 65_536;
/// `Envelope` frame body limit on the clipboard channel: decoded frame ≤ 32 KiB.
pub const MAX_FRAME_CLIPBOARD: usize = 32_768;
/// `PairingEnvelope` frame body limit on the pairing channel: 8192 bytes.
pub const MAX_FRAME_PAIRING: usize = 8_192;
/// `AgentFrame`/`GuiFrame` body limit on local IPC: 65536 bytes.
pub const MAX_FRAME_IPC: usize = 65_536;

/// Text payload hard cap, never widened by image support.
pub const MAX_CLIPBOARD_PAYLOAD_BYTES: u64 = 1_048_576;
pub const MAX_CLIPBOARD_IMAGE_PAYLOAD_BYTES: u64 = 33_554_432;
/// Clipboard transfer chunk cap: `ClipboardData.data` ≤ 16 KiB.
pub const MAX_CLIPBOARD_CHUNK_BYTES: usize = 16_384;
pub const MIN_CLIPBOARD_CHUNK_COUNT: u32 = 1;
pub const MAX_CLIPBOARD_TEXT_CHUNK_COUNT: u32 = 64;
pub const MAX_CLIPBOARD_IMAGE_CHUNK_COUNT: u32 = 2048;
/// Stateless ceiling; accepted offers enforce the smaller per-kind count.
pub const MAX_CLIPBOARD_CHUNK_COUNT: u32 = MAX_CLIPBOARD_IMAGE_CHUNK_COUNT;

/// `InputBatch.events` lower bound (`1..128`).
pub const MIN_INPUT_BATCH_EVENTS: usize = 1;
/// `InputBatch.events` upper bound (`1..128`).
pub const MAX_INPUT_BATCH_EVENTS: usize = 128;

/// `Hello.node_id` and `OwnerState.owner_node_id` length: SHA-256 of SPKI DER.
pub const NODE_ID_LEN: usize = 32;
/// `Envelope.connection_id` length, chosen by the dialing node.
pub const CONNECTION_ID_LEN: usize = 16;
/// `Envelope.lease_id` length on lease messages; empty otherwise.
pub const LEASE_ID_LEN: usize = 16;
/// `Hello.display_name` maximum UTF-8 length in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 128;
/// `Hello.displays` maximum length.
pub const MAX_HELLO_DISPLAYS: usize = 16;

/// Hard membership cap `N_max = 4`; a fifth node is refused with
/// `ERROR_CODE_MEMBERSHIP_FULL`.
pub const N_MAX: u8 = 4;
/// Membership supported and tested by the MVP.
pub const MAX_MEMBERS_TESTED: u8 = 3;
