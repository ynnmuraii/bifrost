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
pub(crate) enum Direction {
    Forward,
    Reverse,
}
impl DirectionMaterial {
    pub fn new(key: Zeroizing<[u8; 32]>, prefix: Zeroizing<[u8; 4]>) -> Self {
        Self { key, prefix }
    }
    // A strict epoch makes key uniqueness independent of an unbounded worker tombstone set.
    pub(crate) fn bind_epoch(
        self,
        session: [u8; 16],
        epoch: u32,
        direction: Direction,
    ) -> Result<Self, CryptoError> {
        let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"bifrost-audio-worker-rekey-v1");
        let prk = salt.extract(self.key.as_ref());
        let epoch = epoch.to_be_bytes();
        let direction: &[u8] = match direction {
            Direction::Forward => b"forward",
            Direction::Reverse => b"reverse",
        };
        let info = [direction, session.as_slice(), epoch.as_slice()];
        let mut key = Zeroizing::new([0; 32]);
        prk.expand(&info, ring::hkdf::HKDF_SHA256)
            .map_err(|_| CryptoError::Key)?
            .fill(key.as_mut())
            .map_err(|_| CryptoError::Key)?;
        Ok(Self {
            key,
            prefix: self.prefix,
        })
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

pub const KEY_LIFETIME_MS: u64 = 1_800_000;
pub const ROTATE_AFTER_MS: u64 = KEY_LIFETIME_MS - 60_000;

pub struct Sealer {
    key: LessSafeKey,
    prefix: Zeroizing<[u8; 4]>,
    session_id: [u8; 16],
    next: u64,
    created_ms: u64,
    epoch: u32,
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
            epoch: 1,
        })
    }
    pub(crate) fn for_epoch(mut self, epoch: u32) -> Self {
        self.epoch = epoch;
        self
    }
    pub(crate) fn epoch(&self) -> u32 {
        self.epoch
    }
    pub(crate) fn session_id(&self) -> [u8; 16] {
        self.session_id
    }
    pub(crate) fn expired(&self, now_ms: u64) -> bool {
        self.next >= PACKET_NUMBER_LIMIT
            || now_ms < self.created_ms
            || now_ms - self.created_ms >= KEY_LIFETIME_MS
    }
    pub(crate) fn rotation_due(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_ms) >= ROTATE_AFTER_MS
            || self.next >= PACKET_NUMBER_LIMIT.saturating_sub(4096)
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
        if self.expired(now_ms) {
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
            key_epoch: self.epoch,
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
    epoch: u32,
    expires_ms: u64,
    previous: Option<Box<Opener>>,
}
impl Opener {
    pub fn new(material: DirectionMaterial, session_id: [u8; 16]) -> Result<Self, CryptoError> {
        let (key, prefix) = material.into_key()?;
        Ok(Self {
            key,
            prefix,
            session_id,
            replay: ReplayWindow::new(),
            epoch: 1,
            expires_ms: KEY_LIFETIME_MS,
            previous: None,
        })
    }
    pub(crate) fn prepare(
        &mut self,
        mut next: Opener,
        epoch: u32,
        now_ms: u64,
    ) -> Result<(), CryptoError> {
        if epoch
            != self
                .epoch
                .checked_add(1)
                .ok_or(CryptoError::RotationRequired)?
            || now_ms >= self.expires_ms
            || self.previous.is_some()
        {
            return Err(CryptoError::Key);
        }
        next.epoch = epoch;
        next.expires_ms = now_ms
            .checked_add(KEY_LIFETIME_MS)
            .ok_or(CryptoError::Key)?;
        let mut previous = std::mem::replace(self, next);
        previous.expires_ms = previous.expires_ms.min(now_ms.saturating_add(10_000));
        self.previous = Some(Box::new(previous));
        Ok(())
    }
    pub(crate) fn activate(&mut self, epoch: u32, now_ms: u64) -> Result<(), CryptoError> {
        if self.epoch != epoch {
            return Err(CryptoError::Key);
        }
        if let Some(previous) = &mut self.previous {
            previous.expires_ms = previous.expires_ms.min(now_ms.saturating_add(2000));
        }
        Ok(())
    }
    pub(crate) fn expire(&mut self, now_ms: u64) -> Result<(), CryptoError> {
        if now_ms >= self.expires_ms {
            return Err(CryptoError::RotationRequired);
        }
        if self
            .previous
            .as_ref()
            .is_some_and(|key| now_ms >= key.expires_ms)
        {
            self.previous = None;
        }
        Ok(())
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
        let key = if header.key_epoch == self.epoch {
            self
        } else {
            self.previous
                .as_deref_mut()
                .filter(|key| key.epoch == header.key_epoch)
                .ok_or(CryptoError::UnknownSession)?
        };
        key.replay
            .check(header.packet_number)
            .map_err(|_| CryptoError::Replay)?;
        let (aad, body) = datagram.split_at_mut(HEADER_LEN);
        let plaintext = key
            .key
            .open_in_place(
                nonce(&key.prefix, header.packet_number),
                Aad::from(&*aad),
                body,
            )
            .map_err(|_| CryptoError::Authentication)?;
        key.replay
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
    fn epochs_keep_replay_independent_and_never_extend_old_key_deadline() {
        let session = [1; 16];
        let mut old_send = Sealer::new(material(), session, 0).unwrap();
        let mut receive = Opener::new(material(), session).unwrap();
        let mut old_packet = [0; 1200];
        let n = old_send
            .seal(1_799_998, PacketKind::Keepalive, 0, 0, &[], &mut old_packet)
            .unwrap();
        receive.open(&mut old_packet[..n], |_| true).unwrap();
        let fresh = || DirectionMaterial::new(Zeroizing::new([17; 32]), Zeroizing::new([19; 4]));
        receive
            .prepare(Opener::new(fresh(), session).unwrap(), 2, 1_799_999)
            .unwrap();
        let mut send = Sealer::new(fresh(), session, 1_799_999)
            .unwrap()
            .for_epoch(2);
        let mut packet = [0; 1200];
        let m = send
            .seal(1_799_999, PacketKind::Keepalive, 0, 0, &[], &mut packet)
            .unwrap();
        assert_eq!(validate_datagram(&packet[..m]).unwrap().packet_number, 0);
        let original = packet;
        receive.open(&mut packet[..m], |_| true).unwrap();
        packet = original;
        assert_eq!(
            receive.open(&mut packet[..m], |_| true).err(),
            Some(CryptoError::Replay)
        );
        receive.activate(2, 1_799_999).unwrap();
        receive.expire(1_800_000).unwrap();
        let n = old_send
            .seal(1_799_999, PacketKind::Keepalive, 0, 0, &[], &mut old_packet)
            .unwrap();
        assert_eq!(
            receive.open(&mut old_packet[..n], |_| true).err(),
            Some(CryptoError::UnknownSession)
        );
        assert_eq!(
            old_send.seal(1_800_000, PacketKind::Keepalive, 0, 0, &[], &mut old_packet),
            Err(CryptoError::RotationRequired)
        );
        assert!(send
            .seal(1_800_000, PacketKind::Keepalive, 0, 0, &[], &mut packet)
            .is_ok());
        assert!(receive.expire(3_599_999).is_err());
    }
    #[test]
    fn epoch_binding_keeps_key_nonce_pairs_unique_without_key_history_growth() {
        let initial = material();
        let second = material()
            .bind_epoch([1; 16], 2, Direction::Forward)
            .unwrap();
        let third = material()
            .bind_epoch([1; 16], 3, Direction::Forward)
            .unwrap();
        let other_session = material()
            .bind_epoch([2; 16], 2, Direction::Forward)
            .unwrap();
        let reverse = material()
            .bind_epoch([1; 16], 2, Direction::Reverse)
            .unwrap();
        assert!(second.key.as_ref() != reverse.key.as_ref());
        assert!(initial.key.as_ref() != second.key.as_ref());
        assert!(second.key.as_ref() != third.key.as_ref());
        assert!(second.key.as_ref() != other_session.key.as_ref());
        assert_eq!(second.prefix, third.prefix);
    }
    #[test]
    fn three_hundred_epochs_have_unique_keys_and_bounded_independent_replay_state() {
        let session = [1; 16];
        let mut receiver = Opener::new(material(), session).unwrap();
        let mut fingerprints = std::collections::BTreeSet::new();
        for epoch in 2..=301 {
            let now = u64::from(epoch) * 3000;
            receiver.expire(now).unwrap();
            let next = material()
                .bind_epoch(session, epoch, Direction::Forward)
                .unwrap();
            let hash: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, next.key.as_ref())
                .as_ref()
                .try_into()
                .unwrap();
            assert!(fingerprints.insert(hash));
            receiver
                .prepare(Opener::new(next, session).unwrap(), epoch, now)
                .unwrap();
            assert!(receiver.previous.as_ref().unwrap().previous.is_none());
            assert!(receiver
                .prepare(Opener::new(material(), session).unwrap(), epoch, now)
                .is_err());
            assert!(receiver
                .prepare(Opener::new(material(), session).unwrap(), epoch + 2, now)
                .is_err());
            receiver.activate(epoch, now).unwrap();
            let mut sender = Sealer::new(
                material()
                    .bind_epoch(session, epoch, Direction::Forward)
                    .unwrap(),
                session,
                now,
            )
            .unwrap()
            .for_epoch(epoch);
            let mut packet = [0; 1200];
            let length = sender
                .seal(now, PacketKind::Keepalive, 0, 0, &[], &mut packet)
                .unwrap();
            assert_eq!(
                validate_datagram(&packet[..length]).unwrap().packet_number,
                0
            );
            let original = packet;
            receiver.open(&mut packet[..length], |_| true).unwrap();
            packet = original;
            assert_eq!(
                receiver.open(&mut packet[..length], |_| true).err(),
                Some(CryptoError::Replay)
            );
        }
        assert_eq!(fingerprints.len(), 300);
    }
    #[test]
    fn rotation_due_precedes_but_does_not_relax_hard_deadline() {
        let mut send = Sealer::new(material(), [1; 16], 23).unwrap();
        assert!(!send.rotation_due(23 + ROTATE_AFTER_MS - 1));
        assert!(send.rotation_due(23 + ROTATE_AFTER_MS));
        let mut packet = [0; 1200];
        assert!(send
            .seal(
                23 + KEY_LIFETIME_MS - 1,
                PacketKind::Keepalive,
                0,
                0,
                &[],
                &mut packet
            )
            .is_ok());
        assert_eq!(
            send.seal(
                23 + KEY_LIFETIME_MS,
                PacketKind::Keepalive,
                0,
                0,
                &[],
                &mut packet
            ),
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
