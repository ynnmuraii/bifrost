//! Sans-IO framing for `4-byte big-endian length || Protobuf body` frames.
//!
//! The codec is pure byte-slice work: no I/O, no async, no allocation while
//! decoding, and never any allocation before the declared length has been
//! checked against the channel limit (`05-protocol.md`: «Не выделять память до
//! проверки длины»).
//!
//! Decoding is incremental: [`FrameDecoder::decode`] returns `Ok(None)` until a
//! whole frame is buffered, `Err` as soon as a present length prefix is
//! malformed (zero or above the channel limit — the check needs only the four
//! prefix bytes, not the body), and `Ok(Some(frame))` exactly once per frame.
//! The returned [`Frame`] borrows the input buffer, so a decoder can split a
//! read buffer into frames without copying.

use crate::limits::{
    ALPN_CLIPBOARD, ALPN_INPUT, ALPN_PAIR, MAX_FRAME_CLIPBOARD, MAX_FRAME_INPUT_CONTROL,
    MAX_FRAME_IPC, MAX_FRAME_PAIRING,
};
use crate::wire;

/// Bytes of the big-endian length prefix in front of every frame body.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// A logical frame channel; each one has its own maximum body length and, for
/// the network channels, its own ALPN identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// `Envelope` over the input/control link, ALPN [`ALPN_INPUT`].
    InputControl,
    /// `Envelope` over a clipboard link, ALPN [`ALPN_CLIPBOARD`].
    Clipboard,
    /// `PairingEnvelope` over the pairing channel, ALPN [`ALPN_PAIR`].
    Pairing,
    /// `AgentFrame`/`GuiFrame` over a local IPC channel; never on the network.
    Ipc,
}

impl Channel {
    /// Every channel, for exhaustive tests and diagnostics.
    pub const ALL: [Channel; 4] = [
        Channel::InputControl,
        Channel::Clipboard,
        Channel::Pairing,
        Channel::Ipc,
    ];

    /// Maximum body length in bytes: `Frame length input/control 1..65536`,
    /// clipboard `decoded frame ≤32 KiB`, pairing `8192 bytes`, IPC `65536`.
    pub const fn max_frame_len(self) -> usize {
        match self {
            Channel::InputControl => MAX_FRAME_INPUT_CONTROL,
            Channel::Clipboard => MAX_FRAME_CLIPBOARD,
            Channel::Pairing => MAX_FRAME_PAIRING,
            Channel::Ipc => MAX_FRAME_IPC,
        }
    }

    /// ALPN identifier for the network channels; `None` for local IPC.
    pub const fn alpn(self) -> Option<&'static str> {
        match self {
            Channel::InputControl => Some(ALPN_INPUT),
            Channel::Clipboard => Some(ALPN_CLIPBOARD),
            Channel::Pairing => Some(ALPN_PAIR),
            Channel::Ipc => None,
        }
    }
}

/// A framing-level failure. All of them are wire contract violations and map to
/// [`wire::ErrorCode::InvalidFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// A frame is never empty: zero-length body (decode) or empty payload
    /// (encode).
    Empty,
    /// Declared or actual body length exceeds the channel maximum.
    TooLarge { len: usize, max: usize },
    /// The body could not be written into the reserved output buffer. Unreachable
    /// for `Vec` output; kept so the encoder stays total.
    EncodeFailed,
}

impl FrameError {
    /// The [`wire::ErrorCode`] the protocol specifies for this failure.
    pub const fn error_code(self) -> wire::ErrorCode {
        wire::ErrorCode::InvalidFrame
    }
}

/// One decoded frame; borrows the buffer it was decoded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    body: &'a [u8],
}

impl<'a> Frame<'a> {
    /// The Protobuf body without the length prefix.
    pub const fn body(self) -> &'a [u8] {
        self.body
    }

    /// Body length in bytes (always `1..=channel.max_frame_len()`).
    pub const fn len(self) -> usize {
        self.body.len()
    }

    /// A frame is never empty, so this is always `false`; it exists so `len`
    /// reads naturally.
    pub const fn is_empty(self) -> bool {
        false
    }

    /// Total bytes this frame occupies in the stream, prefix included.
    pub const fn consumed_len(self) -> usize {
        LENGTH_PREFIX_LEN + self.body.len()
    }
}

/// Incremental, allocation-free frame decoder for one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDecoder {
    channel: Channel,
}

impl FrameDecoder {
    /// A decoder for `channel`; the channel fixes the body length limit.
    pub const fn new(channel: Channel) -> Self {
        Self { channel }
    }

    /// The channel this decoder validates against.
    pub const fn channel(self) -> Channel {
        self.channel
    }

    /// Split at most the first frame off `buf`.
    ///
    /// * `Ok(None)` — more bytes are needed; nothing was consumed.
    /// * `Ok(Some(frame))` — one frame; retry with `&buf[frame.consumed_len()..]`.
    /// * `Err(_)` — the declared length is not a valid frame length for this
    ///   channel. The connection must be closed with
    ///   [`FrameError::error_code`]; a partial body is never buffered.
    pub fn decode<'a>(self, buf: &'a [u8]) -> Result<Option<Frame<'a>>, FrameError> {
        if buf.len() < LENGTH_PREFIX_LEN {
            return Ok(None);
        }
        let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len == 0 {
            return Err(FrameError::Empty);
        }
        let max = self.channel.max_frame_len();
        if len > max {
            return Err(FrameError::TooLarge { len, max });
        }
        let end = LENGTH_PREFIX_LEN + len;
        if buf.len() < end {
            return Ok(None);
        }
        Ok(Some(Frame {
            body: &buf[LENGTH_PREFIX_LEN..end],
        }))
    }
}

/// Frame `payload` for `channel` and append it to `out`.
///
/// The length is validated before anything is written, so an oversized payload
/// leaves `out` untouched and never grows it.
pub fn encode_to(channel: Channel, payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    let len = payload.len();
    if len == 0 {
        return Err(FrameError::Empty);
    }
    let max = channel.max_frame_len();
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    out.reserve(LENGTH_PREFIX_LEN + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Frame `payload` for `channel` into a fresh buffer.
///
/// The capacity is reserved after the limit check, never before it.
pub fn encode(channel: Channel, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    encode_to(channel, payload, &mut out)?;
    Ok(out)
}

/// Frame an encoded prost message for `channel`.
///
/// `encoded_len` is exact for prost messages, so the length is checked from the
/// message itself and the body is written once into a exactly-sized buffer.
pub fn encode_message<M: prost::Message>(channel: Channel, msg: &M) -> Result<Vec<u8>, FrameError> {
    let len = msg.encoded_len();
    if len == 0 {
        return Err(FrameError::Empty);
    }
    let max = channel.max_frame_len();
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    let mut out = Vec::with_capacity(LENGTH_PREFIX_LEN + len);
    out.extend_from_slice(&(len as u32).to_be_bytes());
    msg.encode(&mut out).map_err(|_| FrameError::EncodeFailed)?;
    Ok(out)
}

/// A message decode failure: either framing, or a body that is not a valid
/// message of the expected type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The length prefix is not a valid frame length for the channel.
    Frame(FrameError),
    /// The length prefix was valid but the body does not decode into `M`.
    Malformed,
}

impl From<FrameError> for DecodeError {
    fn from(err: FrameError) -> Self {
        DecodeError::Frame(err)
    }
}

impl DecodeError {
    /// Both cases are `ERROR_CODE_INVALID_FRAME` on the wire.
    pub const fn error_code(self) -> wire::ErrorCode {
        wire::ErrorCode::InvalidFrame
    }
}

/// Decode one message of type `M` from the front of `buf`.
///
/// Returns `Ok(None)` while the frame is incomplete, so a caller can keep
/// reading; the delegated frame limit bounds how much memory `M::decode` may
/// consume.
pub fn decode_message<M>(channel: Channel, buf: &[u8]) -> Result<Option<M>, DecodeError>
where
    M: prost::Message + Default,
{
    match FrameDecoder::new(channel).decode(buf)? {
        None => Ok(None),
        Some(frame) => M::decode(frame.body())
            .map(Some)
            .map_err(|_| DecodeError::Malformed),
    }
}
