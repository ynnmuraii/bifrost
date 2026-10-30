//! Single source of truth for the fixed-width auxiliary key-derivation context.
//! Hash the format preimage with SHA-256 in the caller; this crate stays dependency-free.
use crate::jitter::Profile;
pub const FORWARD_KEY_LABEL: &[u8] = b"bifrost-audio-v1/forward-key";
pub const REVERSE_KEY_LABEL: &[u8] = b"bifrost-audio-v1/reverse-key";
pub const FORWARD_NONCE_LABEL: &[u8] = b"bifrost-audio-v1/forward-nonce";
pub const REVERSE_NONCE_LABEL: &[u8] = b"bifrost-audio-v1/reverse-nonce";
pub const CONTEXT_LEN: usize = 225;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    Opus = 1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Aead {
    ChaCha20Poly1305 = 1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    System = 1,
    Microphone = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u16,
    pub frame_samples: u16,
    pub bitrate: u32,
    pub fec: bool,
    pub aead: Aead,
    pub profile: Profile,
    pub max_datagram: u16,
}
impl Format {
    /// SHA-256 preimage: u8/u32/u16/u16/u32/u8/u8/u8/u16, all integers BE.
    pub fn canonical_bytes(self) -> [u8; 18] {
        let mut out = [0; 18];
        out[0] = self.codec as u8;
        out[1..5].copy_from_slice(&self.sample_rate.to_be_bytes());
        out[5..7].copy_from_slice(&self.channels.to_be_bytes());
        out[7..9].copy_from_slice(&self.frame_samples.to_be_bytes());
        out[9..13].copy_from_slice(&self.bitrate.to_be_bytes());
        out[13] = u8::from(self.fec);
        out[14] = self.aead as u8;
        out[15] = self.profile as u8;
        out[16..18].copy_from_slice(&self.max_datagram.to_be_bytes());
        out
    }
}
pub struct SessionContext {
    pub parent_connection_id: [u8; 16],
    pub group_id: [u8; 16],
    pub session_id: [u8; 16],
    pub listener_nonce: [u8; 32],
    pub source_nonce: [u8; 32],
    pub nodes: [[u8; 32]; 2],
    pub kind: StreamKind,
    pub format_sha256: [u8; 32],
}
impl SessionContext {
    /// Fixed layout required by audio v1; preserves both nonces in their semantic role order.
    pub fn encode(&self) -> [u8; CONTEXT_LEN] {
        let mut out = [0; CONTEXT_LEN];
        let mut offset = 0;
        let mut append = |bytes: &[u8]| {
            out[offset..offset + bytes.len()].copy_from_slice(bytes);
            offset += bytes.len();
        };
        append(b"bifrost-audio-v1");
        append(&self.parent_connection_id);
        append(&self.group_id);
        append(&self.session_id);
        append(&self.listener_nonce);
        append(&self.source_nonce);
        let (a, b) = if self.nodes[0] <= self.nodes[1] {
            (&self.nodes[0], &self.nodes[1])
        } else {
            (&self.nodes[1], &self.nodes[0])
        };
        append(a);
        append(b);
        append(&[self.kind as u8]);
        append(&self.format_sha256);
        out
    }
}
