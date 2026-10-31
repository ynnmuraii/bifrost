//! Directional media key handoff. Canonical context/labels live in audio-model::keys.
use crate::crypto::{CryptoError, DirectionMaterial, Opener, Sealer};
/// Forward means media sender→receiver, never TLS dialer→acceptor.
pub enum Endpoint {
    MediaSender,
    MediaReceiver,
}
pub struct SessionKeys {
    pub send: Sealer,
    pub receive: Opener,
}
impl SessionKeys {
    pub fn new(
        endpoint: Endpoint,
        session_id: [u8; 16],
        now_ms: u64,
        forward: DirectionMaterial,
        reverse: DirectionMaterial,
    ) -> Result<Self, CryptoError> {
        let (send, receive) = match endpoint {
            Endpoint::MediaSender => (forward, reverse),
            Endpoint::MediaReceiver => (reverse, forward),
        };
        Ok(Self {
            send: Sealer::new(send, session_id, now_ms)?,
            receive: Opener::new(receive, session_id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_model::PacketKind;
    use zeroize::Zeroizing;
    fn material(byte: u8) -> DirectionMaterial {
        DirectionMaterial::new(Zeroizing::new([byte; 32]), Zeroizing::new([byte; 4]))
    }
    #[test]
    fn direction_pairs_interoperate_but_reflection_fails() {
        let mut source =
            SessionKeys::new(Endpoint::MediaSender, [3; 16], 0, material(1), material(2)).unwrap();
        let mut listener = SessionKeys::new(
            Endpoint::MediaReceiver,
            [3; 16],
            0,
            material(1),
            material(2),
        )
        .unwrap();
        let mut packet = [0; 1200];
        let n = source
            .send
            .seal(0, PacketKind::Keepalive, 0, 0, &[4], &mut packet)
            .unwrap();
        let mut reflected = packet;
        assert_eq!(
            source.receive.open(&mut reflected[..n], |_| true).err(),
            Some(CryptoError::Authentication)
        );
        assert_eq!(
            listener.receive.open(&mut packet[..n], |_| true).unwrap().1,
            &[4]
        );
        let n = listener
            .send
            .seal(0, PacketKind::Keepalive, 0, 0, &[5], &mut packet)
            .unwrap();
        assert_eq!(
            source.receive.open(&mut packet[..n], |_| true).unwrap().1,
            &[5]
        );
    }
}
