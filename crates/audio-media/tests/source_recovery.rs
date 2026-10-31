//! Dry capture over localhost only: delayed validation and hostile input cannot stall Source.
use audio_backend::{
    dry::{DryBackend, DryStream, ScriptEvent},
    AudioFormat, Capabilities, CaptureSpec, Device, DeviceId, DeviceSelection, Direction,
};
use audio_media::{
    crypto::{DirectionMaterial, Opener, Sealer},
    runtime::{start_source, MediaSession, SessionConfig, SessionMaterial, STOP_DEADLINE},
    socket::MediaSocket,
};
use audio_model::{jitter::Profile, packet::MAX_DATAGRAM_LEN, PacketKind};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const FRAME: Duration = Duration::from_millis(10);
fn material() -> SessionMaterial {
    SessionMaterial {
        forward: DirectionMaterial::new(Zeroizing::new([11; 32]), Zeroizing::new([12; 4])),
        reverse: DirectionMaterial::new(Zeroizing::new([13; 32]), Zeroizing::new([14; 4])),
    }
}
struct Fixture {
    source: MediaSession<DryStream>,
    address: SocketAddr,
    peer: UdpSocket,
    opener: Opener,
    sealer: Sealer,
    token: Vec<u8>,
    previous_media_timestamp: Option<u32>,
}
impl Fixture {
    fn new() -> Self {
        let mut backend = DryBackend::new(Capabilities {
            system_capture: true,
            ..Default::default()
        });
        let id = DeviceId::new("DRY_RECOVERY").unwrap();
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        };
        backend
            .apply(ScriptEvent::UpsertDevice(Device {
                id: id.clone(),
                label: "Dry recovery".into(),
                direction: Direction::Render,
                format,
            }))
            .unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        let source = start_source(
            &mut backend,
            CaptureSpec::Monitor {
                sink: DeviceSelection::Explicit(id),
                format,
            },
            SessionConfig {
                socket: MediaSocket::new(socket).unwrap(),
                peer: peer.local_addr().unwrap(),
                session_id: [17; 16],
                profile: Profile::Ethernet,
                authorized: Arc::new(AtomicBool::new(true)),
            },
            material(),
        )
        .unwrap();
        Self {
            source,
            address,
            peer,
            opener: Opener::new(material().forward, [17; 16]).unwrap(),
            sealer: Sealer::new(material().reverse, [17; 16], 0).unwrap(),
            token: Vec::new(),
            previous_media_timestamp: None,
        }
    }
    fn capture(&mut self) {
        self.source.stream_mut().unwrap().pump(480).unwrap();
    }
    fn drain(&mut self) -> usize {
        let mut bytes = [0; MAX_DATAGRAM_LEN];
        let mut media = 0;
        while let Ok((n, from)) = self.peer.recv_from(&mut bytes) {
            assert_eq!(from, self.address);
            let (header, payload) = self.opener.open(&mut bytes[..n], |_| true).unwrap();
            match header.kind {
                PacketKind::PathChallenge => {
                    self.token.clear();
                    self.token.extend_from_slice(payload);
                }
                PacketKind::Opus => {
                    if let Some(previous) = self.previous_media_timestamp {
                        assert_eq!(header.media_timestamp.wrapping_sub(previous), 480);
                    } else {
                        assert_eq!(
                            header.media_timestamp, 0,
                            "preactivation data skipped media time"
                        );
                    }
                    self.previous_media_timestamp = Some(header.media_timestamp);
                    media += 1;
                }
                _ => {}
            }
        }
        media
    }
    fn validate(&mut self) {
        assert!(!self.token.is_empty());
        let mut reply = [0; MAX_DATAGRAM_LEN];
        let n = self
            .sealer
            .seal(0, PacketKind::PathResponse, 0, 0, &self.token, &mut reply)
            .unwrap();
        self.peer.send_to(&reply[..n], self.address).unwrap();
    }
}

#[test]
fn delayed_validation_discards_preactivation_capture_and_starts_immediately() {
    let mut f = Fixture::new();
    let start = Instant::now();
    let mut capture_due = Duration::ZERO;
    while start.elapsed() < Duration::from_millis(550) {
        while capture_due <= start.elapsed() {
            f.capture();
            capture_due += FRAME;
        }
        assert_eq!(f.drain(), 0, "media before authentication");
        thread::sleep(Duration::from_micros(250));
    }
    assert_eq!(
        f.source.snapshot().capture_dropped_frames,
        0,
        "preactivation ring was not drained"
    );
    let validated = Instant::now();
    f.validate();
    let mut first = None;
    let mut packets = 0;
    while validated.elapsed() < Duration::from_millis(120) {
        while capture_due <= start.elapsed() {
            f.capture();
            capture_due += FRAME;
        }
        let received = f.drain();
        if received > 0 {
            first.get_or_insert_with(|| validated.elapsed());
        }
        packets += received;
        thread::sleep(Duration::from_micros(250));
    }
    f.source.stop().unwrap();
    assert!(
        first.is_some_and(|delay| delay <= FRAME),
        "first media delay: {first:?}"
    );
    assert!(
        packets >= 10,
        "Source stalled after fresh post-overflow capture: {packets} packets"
    );
}

#[test]
fn hostile_input_does_not_starve_source_pacing_or_stop() {
    let mut f = Fixture::new();
    let start = Instant::now();
    while f.token.is_empty() {
        f.capture();
        f.drain();
        assert!(start.elapsed() < Duration::from_secs(1));
        thread::sleep(FRAME);
    }
    f.validate();
    let mut packet = [0; MAX_DATAGRAM_LEN];
    // A valid structure from the pinned peer, but an invalid tag. A foreign socket
    // also injects it; tuple rejection must not monopolize the sole socket reader.
    let n = f
        .sealer
        .seal(0, PacketKind::Keepalive, 0, 0, &[], &mut packet)
        .unwrap();
    packet[n - 1] ^= 1;
    let pinned = f.peer.try_clone().unwrap();
    let address = f.address;
    let running = Arc::new(AtomicBool::new(true));
    let send = running.clone();
    let flood = thread::spawn(move || {
        let foreign = UdpSocket::bind("127.0.0.1:0").unwrap();
        foreign.set_nonblocking(true).unwrap();
        while send.load(Ordering::Acquire) {
            for _ in 0..64 {
                let _ = pinned.send_to(&packet[..n], address);
                let _ = foreign.send_to(&packet[..n], address);
            }
            thread::yield_now();
        }
    });
    let measured = Instant::now();
    let mut capture_due = Duration::ZERO;
    let mut packets = 0;
    while measured.elapsed() < Duration::from_millis(600) {
        while capture_due <= measured.elapsed() {
            f.capture();
            capture_due += FRAME;
        }
        packets += f.drain();
        thread::sleep(Duration::from_micros(250));
    }
    let stop = Instant::now();
    let result = f.source.stop();
    let elapsed = stop.elapsed();
    running.store(false, Ordering::Release);
    flood.join().unwrap();
    result.unwrap();
    assert!(elapsed < STOP_DEADLINE, "stop under flood: {elapsed:?}");
    assert!(
        packets >= 45,
        "flood starved Source pacing: {packets} packets"
    );
}

#[test]
fn silent_capture_keeps_media_grid_and_resumes_without_a_burst() {
    let mut f = Fixture::new();
    let start = Instant::now();
    while f.token.is_empty() {
        f.drain();
        assert!(start.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(1));
    }
    f.validate();
    let measured = Instant::now();
    let mut bytes = [0; MAX_DATAGRAM_LEN];
    let mut previous = None;
    let mut silent_packets = 0;
    let mut resumed_packets = 0;
    let mut capture_due = Duration::from_millis(1100);
    let mut first_received = None;
    while measured.elapsed() < Duration::from_millis(1350) {
        if measured.elapsed() >= capture_due {
            f.capture();
            capture_due += FRAME;
        }
        while let Ok((n, _)) = f.peer.recv_from(&mut bytes) {
            let (header, _) = f.opener.open(&mut bytes[..n], |_| true).unwrap();
            if header.kind != PacketKind::Opus {
                continue;
            }
            if let Some(previous) = previous {
                assert_eq!(header.media_timestamp - previous, 480);
            } else {
                assert_eq!(header.media_timestamp, 0);
                first_received = Some(measured.elapsed());
            }
            previous = Some(header.media_timestamp);
            if measured.elapsed() < Duration::from_millis(1100) {
                silent_packets += 1;
            } else {
                resumed_packets += 1;
            }
        }
        thread::sleep(Duration::from_micros(250));
    }
    let health = f.source.snapshot();
    f.source.stop().unwrap();
    assert!(first_received.is_some_and(|delay| delay <= FRAME));
    assert!(
        silent_packets >= 100,
        "silence stopped UDP: {silent_packets}"
    );
    assert!(
        (20..=28).contains(&resumed_packets),
        "resume burst/stall: {resumed_packets}"
    );
    assert_eq!(health.health.discontinuities, 0);
    assert_eq!(health.health.lost_frames, 0);
    assert_eq!(health.health.late_frames, 0);
}
