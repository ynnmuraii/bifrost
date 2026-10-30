//! Nonces are constructed here, never accepted from packet senders.
use audio_model::{
    packet::{HEADER_LEN, MAX_PAYLOAD_LEN, PACKET_NUMBER_LIMIT, TAG_LEN},
    validate_datagram, Header, PacketKind, ReplayWindow,
};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use zeroize::Zeroizing;

/// Single-use directional handoff. No Debug/Clone/Copy; input buffers erase on drop.
/// ring's expanded internal key state does not promise zeroization.
pub struct DirectionMaterial {
    key: Zeroizing<[u8; 32]>,
    prefix: Zeroizing<[u8; 4]>,
}
impl DirectionMaterial {
    pub fn new(key: Zeroizing<[u8; 32]>, prefix: Zeroizing<[u8; 4]>) -> Self {
        Self { key, prefix }
    }
    fn into_key(self) -> Result<(LessSafeKey, Zeroizing<[u8; 4]>), CryptoError> {
        let key = UnboundKey::new(&aead::CHACHA20_POLY1305, self.key.as_ref())
            .map_err(|_| CryptoError::Key)?;
        Ok((LessSafeKey::new(key), self.prefix))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    Key,
    RotationRequired,
    BufferTooSmall,
    InvalidPacket,
    UnknownSession,
    Replay,
    Authentication,
}
impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CryptoError {}
fn nonce(prefix: &[u8; 4], number: u64) -> Nonce {
    let mut bytes = [0; 12];
    bytes[..4].copy_from_slice(prefix);
    bytes[4..].copy_from_slice(&number.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

pub struct Sealer {
    key: LessSafeKey,
    prefix: Zeroizing<[u8; 4]>,
    session_id: [u8; 16],
    next: u64,
    created_ms: u64,
}
impl Sealer {
    pub fn new(
        material: DirectionMaterial,
        session_id: [u8; 16],
        now_ms: u64,
    ) -> Result<Self, CryptoError> {
        let (key, prefix) = material.into_key()?;
        Ok(Self {
            key,
            prefix,
            session_id,
            next: 0,
            created_ms: now_ms,
        })
    }
    /// Writes header/ciphertext/tag into caller storage. Failed sealing still burns its counter.
    /// `now_ms` must use the same monotonic caller clock as construction.
    pub fn seal(
        &mut self,
        now_ms: u64,
        kind: PacketKind,
        timestamp: u32,
        frame_samples: u16,
        plaintext: &[u8],
        output: &mut [u8],
    ) -> Result<usize, CryptoError> {
        if self.next >= PACKET_NUMBER_LIMIT
            || now_ms < self.created_ms
            || now_ms - self.created_ms >= 1_800_000
        {
            return Err(CryptoError::RotationRequired);
        }
        if plaintext.len() > MAX_PAYLOAD_LEN {
            return Err(CryptoError::InvalidPacket);
        }
        let len = HEADER_LEN + plaintext.len() + TAG_LEN;
        if output.len() < len {
            return Err(CryptoError::BufferTooSmall);
        }
        let header = Header {
            kind,
            flags: 0,
            session_id: self.session_id,
            key_epoch: 1,
            packet_number: self.next,
            media_timestamp: timestamp,
            frame_samples,
            payload_len: plaintext.len() as u16,
        };
        let aad = header.encode().map_err(|_| CryptoError::InvalidPacket)?;
        let number = self.next;
        self.next += 1;
        output[..HEADER_LEN].copy_from_slice(&aad);
        let payload = &mut output[HEADER_LEN..HEADER_LEN + plaintext.len()];
        payload.copy_from_slice(plaintext);
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce(&self.prefix, number), Aad::from(aad), payload)
            .map_err(|_| CryptoError::Authentication)?;
        output[HEADER_LEN + plaintext.len()..len].copy_from_slice(tag.as_ref());
        Ok(len)
    }
}
pub struct Opener {
    key: LessSafeKey,
    prefix: Zeroizing<[u8; 4]>,
    session_id: [u8; 16],
    replay: ReplayWindow,
}
impl Opener {
    pub fn new(material: DirectionMaterial, session_id: [u8; 16]) -> Result<Self, CryptoError> {
        let (key, prefix) = material.into_key()?;
        Ok(Self {
            key,
            prefix,
            session_id,
            replay: ReplayWindow::new(),
        })
    }
    /// Validation precedes the caller's installed/committed-session lookup hook and replay check.
    /// Failure has no network output and never commits replay state. The datagram is scratch storage.
    pub fn open<'a>(
        &mut self,
        datagram: &'a mut [u8],
        installed: impl FnOnce(&Header) -> bool,
    ) -> Result<(Header, &'a [u8]), CryptoError> {
        let header = validate_datagram(datagram).map_err(|_| CryptoError::InvalidPacket)?;
        if header.session_id != self.session_id || !installed(&header) {
            return Err(CryptoError::UnknownSession);
        }
        self.replay
            .check(header.packet_number)
            .map_err(|_| CryptoError::Replay)?;
        let (aad, body) = datagram.split_at_mut(HEADER_LEN);
        let plaintext = self
            .key
            .open_in_place(
                nonce(&self.prefix, header.packet_number),
                Aad::from(&*aad),
                body,
            )
            .map_err(|_| CryptoError::Authentication)?;
        self.replay
            .commit(header.packet_number)
            .map_err(|_| CryptoError::Replay)?;
        Ok((header, plaintext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn material() -> DirectionMaterial {
        DirectionMaterial::new(Zeroizing::new([7; 32]), Zeroizing::new([9; 4]))
    }
    #[test]
    fn tampering_does_not_commit_replay_and_reordering_works() {
        let mut sealer = Sealer::new(material(), [1; 16], 0).unwrap();
        let mut opener = Opener::new(material(), [1; 16]).unwrap();
        let mut a = [0; 1200];
        let mut b = [0; 1200];
        let n = sealer
            .seal(0, PacketKind::Opus, 0, 480, &[1, 2, 3], &mut a)
            .unwrap();
        let m = sealer
            .seal(10, PacketKind::Opus, 480, 480, &[4], &mut b)
            .unwrap();
        let original = a;
        a[44] ^= 1;
        assert_eq!(
            opener.open(&mut a[..n], |_| true).err(),
            Some(CryptoError::Authentication)
        );
        assert_eq!(opener.open(&mut b[..m], |_| true).unwrap().1, &[4]);
        a = original;
        assert_eq!(opener.open(&mut a[..n], |_| true).unwrap().1, &[1, 2, 3]);
        a = original;
        assert_eq!(
            opener.open(&mut a[..n], |_| true).err(),
            Some(CryptoError::Replay)
        );
    }
    #[test]
    fn rotation_and_session_lookup_fail_closed() {
        let mut s = Sealer::new(material(), [1; 16], 0).unwrap();
        let mut out = [0; 1200];
        let n = s
            .seal(0, PacketKind::Keepalive, 0, 0, &[], &mut out)
            .unwrap();
        let mut o = Opener::new(material(), [1; 16]).unwrap();
        assert_eq!(
            o.open(&mut out[..n], |_| false).err(),
            Some(CryptoError::UnknownSession)
        );
        assert!(o.open(&mut out[..n], |_| true).is_ok());
        assert_eq!(
            s.seal(1_800_000, PacketKind::Keepalive, 0, 0, &[], &mut out),
            Err(CryptoError::RotationRequired)
        );
        s.next = PACKET_NUMBER_LIMIT - 1;
        let n = s
            .seal(0, PacketKind::Keepalive, 0, 0, &[], &mut out)
            .unwrap();
        assert_eq!(
            validate_datagram(&out[..n]).unwrap().packet_number,
            PACKET_NUMBER_LIMIT - 1
        );
        assert_eq!(
            s.seal(0, PacketKind::Keepalive, 0, 0, &[], &mut out),
            Err(CryptoError::RotationRequired)
        );
    }
    #[test]
    fn every_datagram_byte_is_authenticated_without_replay_poisoning() {
        let mut sealer = Sealer::new(material(), [1; 16], 0).unwrap();
        let mut packet = [0; 1200];
        let n = sealer
            .seal(0, PacketKind::Opus, 0, 480, &[1, 2, 3], &mut packet)
            .unwrap();
        for index in 0..n {
            let mut opener = Opener::new(material(), [1; 16]).unwrap();
            let mut changed = packet;
            changed[index] ^= 1;
            assert!(
                opener.open(&mut changed[..n], |_| true).is_err(),
                "byte {index}"
            );
            let mut original = packet;
            assert!(
                opener.open(&mut original[..n], |_| true).is_ok(),
                "poisoned byte {index}"
            );
        }
        // Prefix followed by packet number in network order, with no host-endian ambiguity.
        assert_eq!(
            nonce(&[1, 2, 3, 4], 0x0102030405060708).as_ref(),
            &[1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8]
        );
    }
}
