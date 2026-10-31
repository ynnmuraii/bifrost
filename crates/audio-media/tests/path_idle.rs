//! Simulated conntrack time keeps the 36-second idle case deterministic and device-free.
use audio_media::{
    control::ReceiverReport,
    crypto::{DirectionMaterial, Opener, Sealer},
    runtime::PATH_KEEPALIVE_INTERVAL,
};
use audio_model::{packet::MAX_DATAGRAM_LEN, PacketKind};
use std::time::Duration;
use zeroize::Zeroizing;

struct Conntrack {
    outbound: [Option<Duration>; 2],
}
impl Conntrack {
    fn send(&mut self, from: usize, now: Duration) -> bool {
        self.outbound[from] = Some(now);
        self.outbound[1 - from]
            .is_some_and(|last| now.saturating_sub(last) < Duration::from_secs(30))
    }
}
fn material(direction: u8) -> DirectionMaterial {
    DirectionMaterial::new(
        Zeroizing::new([direction; 32]),
        Zeroizing::new([direction; 4]),
    )
}
#[test]
fn authenticated_idle_traffic_retains_both_firewall_mappings_beyond_thirty_seconds() {
    let id = [73; 16];
    let mut source = Sealer::new(material(1), id, 0).unwrap();
    let mut listener = Sealer::new(material(2), id, 0).unwrap();
    let mut source_rx = Opener::new(material(2), id).unwrap();
    let mut listener_rx = Opener::new(material(1), id).unwrap();
    let mut firewall = Conntrack {
        outbound: [None; 2],
    };
    let mut packet = [0; MAX_DATAGRAM_LEN];
    let source_token = [11; 16];
    let listener_token = [12; 16];
    // Listener opens its own outbound state first; the first peer-bound packet may be lost.
    listener
        .seal(
            0,
            PacketKind::PathChallenge,
            0,
            0,
            &listener_token,
            &mut packet,
        )
        .unwrap();
    assert!(!firewall.send(1, Duration::ZERO));
    let n = source
        .seal(
            0,
            PacketKind::PathChallenge,
            0,
            0,
            &source_token,
            &mut packet,
        )
        .unwrap();
    assert!(firewall.send(0, Duration::ZERO));
    assert_eq!(
        listener_rx.open(&mut packet[..n], |_| true).unwrap().1,
        source_token
    );
    let n = listener
        .seal(
            0,
            PacketKind::PathResponse,
            0,
            0,
            &source_token,
            &mut packet,
        )
        .unwrap();
    assert!(firewall.send(1, Duration::ZERO));
    assert_eq!(
        source_rx.open(&mut packet[..n], |_| true).unwrap().1,
        source_token
    );
    let n = listener
        .seal(
            500,
            PacketKind::PathChallenge,
            0,
            0,
            &listener_token,
            &mut packet,
        )
        .unwrap();
    assert!(firewall.send(1, Duration::from_millis(500)));
    assert_eq!(
        source_rx.open(&mut packet[..n], |_| true).unwrap().1,
        listener_token
    );
    let n = source
        .seal(
            500,
            PacketKind::PathResponse,
            0,
            0,
            &listener_token,
            &mut packet,
        )
        .unwrap();
    assert!(firewall.send(0, Duration::from_millis(500)));
    assert_eq!(
        listener_rx.open(&mut packet[..n], |_| true).unwrap().1,
        listener_token
    );
    let report = ReceiverReport {
        received: 0,
        lost: 0,
        late: 0,
        jitter_samples: 0,
        buffer_samples: 1440,
    }
    .encode();
    assert!(PATH_KEEPALIVE_INTERVAL <= Duration::from_secs(1));
    let mut now = Duration::from_millis(500);
    while now < Duration::from_secs(36) {
        now += PATH_KEEPALIVE_INTERVAL;
        let ms = now.as_millis() as u64;
        let n = source
            .seal(ms, PacketKind::Keepalive, 0, 0, &[], &mut packet)
            .unwrap();
        assert!(firewall.send(0, now));
        assert_eq!(
            listener_rx.open(&mut packet[..n], |_| true).unwrap().0.kind,
            PacketKind::Keepalive
        );
        let n = listener
            .seal(ms, PacketKind::ReceiverReport, 0, 0, &report, &mut packet)
            .unwrap();
        assert!(firewall.send(1, now));
        assert_eq!(
            source_rx.open(&mut packet[..n], |_| true).unwrap().0.kind,
            PacketKind::ReceiverReport
        );
    }
    // Negative control: without the invariant, the same firewall really expires.
    assert!(!firewall.send(0, now + Duration::from_secs(30)));
}
