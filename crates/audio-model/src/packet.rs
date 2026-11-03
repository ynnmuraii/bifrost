//! Version-one wire framing. Parsing validates structure, never authenticity.

use core::fmt;

pub const HEADER_LEN: usize = 44;
pub const TAG_LEN: usize = 16;
pub const MAX_DATAGRAM_LEN: usize = 1200;
pub const MAX_PAYLOAD_LEN: usize = MAX_DATAGRAM_LEN - HEADER_LEN - TAG_LEN;
pub const MAGIC: [u8; 4] = *b"BFA1";
pub const VERSION: u8 = 1;
/// Exclusive per-direction, per-key counter limit; rotate before reaching it.
pub const PACKET_NUMBER_LIMIT: u64 = 1 << 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    Opus = 1,
    PathChallenge = 2,
    PathResponse = 3,
    ReceiverReport = 4,
    Keepalive = 5,
}

impl TryFrom<u8> for PacketKind {
    type Error = PacketError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Opus),
            2 => Ok(Self::PathChallenge),
            3 => Ok(Self::PathResponse),
            4 => Ok(Self::ReceiverReport),
            5 => Ok(Self::Keepalive),
            _ => Err(PacketError::UnsupportedKind(value)),
        }
    }
}

/// The 44-byte authenticated additional data, with implicit magic and version.
///
/// Packet numbers start at zero at the sender and never wrap under a key.
/// The media AEAD owner constructs nonces from its monotonic per-key counter.
/// Rekey-capable sessions use strictly increasing epochs with distinct keys and replay windows.
/// Timestamps are opaque modulo-2^32 sample counts, not packet numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: PacketKind,
    pub flags: u16,
    pub session_id: [u8; 16],
    pub key_epoch: u32,
    pub packet_number: u64,
    pub media_timestamp: u32,
    pub frame_samples: u16,
    pub payload_len: u16,
}

impl Header {
    /// Encodes a valid header into fixed storage without allocation.
    /// Does not encode or authenticate ciphertext or its tag.
    pub fn encode(&self) -> Result<[u8; HEADER_LEN], PacketError> {
        self.validate()?;
        let mut bytes = [0; HEADER_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.kind as u8;
        bytes[6..8].copy_from_slice(&self.flags.to_be_bytes());
        bytes[8..24].copy_from_slice(&self.session_id);
        bytes[24..28].copy_from_slice(&self.key_epoch.to_be_bytes());
        bytes[28..36].copy_from_slice(&self.packet_number.to_be_bytes());
        bytes[36..40].copy_from_slice(&self.media_timestamp.to_be_bytes());
        bytes[40..42].copy_from_slice(&self.frame_samples.to_be_bytes());
        bytes[42..44].copy_from_slice(&self.payload_len.to_be_bytes());
        Ok(bytes)
    }

    /// Parses exactly 44 header bytes and validates version-one field rules.
    ///
    /// This does not establish that a payload/tag exists or authenticates. Use
    /// [`validate_datagram`] on the complete datagram before crypto processing;
    /// neither operation authorizes device actions or replay-window updates.
    pub fn decode(bytes: &[u8]) -> Result<Self, PacketError> {
        if bytes.len() != HEADER_LEN {
            return Err(PacketError::InvalidHeaderLength);
        }
        if bytes[..4] != MAGIC {
            return Err(PacketError::InvalidMagic);
        }
        if bytes[4] != VERSION {
            return Err(PacketError::UnsupportedVersion(bytes[4]));
        }
        let header = Self {
            kind: PacketKind::try_from(bytes[5])?,
            flags: u16::from_be_bytes(field(bytes, 6)),
            session_id: field(bytes, 8),
            key_epoch: u32::from_be_bytes(field(bytes, 24)),
            packet_number: u64::from_be_bytes(field(bytes, 28)),
            media_timestamp: u32::from_be_bytes(field(bytes, 36)),
            frame_samples: u16::from_be_bytes(field(bytes, 40)),
            payload_len: u16::from_be_bytes(field(bytes, 42)),
        };
        header.validate()?;
        Ok(header)
    }

    fn validate(&self) -> Result<(), PacketError> {
        if self.flags != 0 {
            return Err(PacketError::UnsupportedFlags(self.flags));
        }
        if self.packet_number >= PACKET_NUMBER_LIMIT {
            return Err(PacketError::RotationRequired);
        }
        if self.key_epoch == 0 {
            return Err(PacketError::UnsupportedKeyEpoch(self.key_epoch));
        }
        if usize::from(self.payload_len) > MAX_PAYLOAD_LEN {
            return Err(PacketError::PayloadTooLarge);
        }
        if self.kind == PacketKind::Opus {
            if !matches!(self.frame_samples, 480 | 960) {
                return Err(PacketError::InvalidMediaFields);
            }
        } else if self.frame_samples != 0 || self.media_timestamp != 0 {
            return Err(PacketError::InvalidMediaFields);
        }
        Ok(())
    }
}

// Only called after the fixed header length has been established.
fn field<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut result = [0; N];
    result.copy_from_slice(&bytes[offset..offset + N]);
    result
}

/// Validates the complete datagram's header and exact ciphertext-plus-tag length.
///
/// Ciphertext and the 16-byte tag remain opaque and untouched. A successful
/// result is not authentication, does not validate kind-specific plaintext or
/// negotiated Opus parameters, and must not advance replay state. Even an empty
/// payload requires its tag. Trailing bytes and truncated tags are rejected.
///
/// Before AEAD, the caller must also require a known active `header.session_id`
/// and call the replay window's non-mutating `check`. AAD is exactly the original
/// datagram bytes `0..HEADER_LEN` (44 bytes), not a serialized replacement header.
/// Commit replay state only after a valid tag. Never reply to a packet that
/// failed any structural, session, replay, or authentication verification.
pub fn validate_datagram(datagram: &[u8]) -> Result<Header, PacketError> {
    if datagram.len() > MAX_DATAGRAM_LEN {
        return Err(PacketError::DatagramTooLarge);
    }
    if datagram.len() < HEADER_LEN + TAG_LEN {
        return Err(PacketError::InvalidDatagramLength);
    }
    let payload_len = usize::from(u16::from_be_bytes(field(datagram, 42)));
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(PacketError::PayloadTooLarge);
    }
    if datagram.len() != HEADER_LEN + payload_len + TAG_LEN {
        return Err(PacketError::InvalidDatagramLength);
    }
    Header::decode(&datagram[..HEADER_LEN])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    InvalidHeaderLength,
    InvalidDatagramLength,
    InvalidMagic,
    UnsupportedVersion(u8),
    UnsupportedKind(u8),
    UnsupportedFlags(u16),
    UnsupportedKeyEpoch(u32),
    InvalidMediaFields,
    PayloadTooLarge,
    DatagramTooLarge,
    RotationRequired,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidHeaderLength => f.write_str("audio header must be exactly 44 bytes"),
            Self::InvalidDatagramLength => {
                f.write_str("audio datagram length does not match header and tag")
            }
            Self::InvalidMagic => f.write_str("invalid audio packet magic"),
            Self::UnsupportedVersion(value) => {
                write!(f, "unsupported audio packet version {value}")
            }
            Self::UnsupportedKind(value) => write!(f, "unsupported audio packet kind {value}"),
            Self::UnsupportedFlags(value) => write!(f, "unsupported audio packet flags {value}"),
            Self::UnsupportedKeyEpoch(value) => write!(f, "unsupported audio key epoch {value}"),
            Self::InvalidMediaFields => {
                f.write_str("invalid audio timestamp or frame sample count")
            }
            Self::PayloadTooLarge => f.write_str("audio payload exceeds 1140 bytes"),
            Self::DatagramTooLarge => f.write_str("audio datagram exceeds 1200 bytes"),
            Self::RotationRequired => f.write_str("audio packet number requires a new session"),
        }
    }
}

impl std::error::Error for PacketError {}
