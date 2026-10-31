//! Real encrypted UDP, Dry capture only: a silent control peer must not pace Source media.
use audio_backend::{
    dry::{DryBackend, ScriptEvent},
    AudioFormat, Capabilities, CaptureSpec, Device, DeviceId, DeviceSelection, Direction,
};
use audio_media::{
    crypto::{DirectionMaterial, Opener, Sealer},
    runtime::{start_source, SessionConfig, SessionMaterial},
};
use audio_model::{jitter::Profile, packet::MAX_DATAGRAM_LEN, PacketKind};
use std::{
    net::UdpSocket,
    sync::{atomic::AtomicBool, Arc},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

fn material() -> SessionMaterial {
    SessionMaterial {
        forward: DirectionMaterial::new(Zeroizing::new([1; 32]), Zeroizing::new([2; 4])),
        reverse: DirectionMaterial::new(Zeroizing::new([3; 32]), Zeroizing::new([4; 4])),
    }
}

#[test]
fn source_keeps_100_fps_when_control_peer_is_silent() {
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        ..Capabilities::default()
    });
    let id = DeviceId::new("DRY_CADENCE").unwrap();
    let format = AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    };
    backend
        .apply(ScriptEvent::UpsertDevice(Device {
            id: id.clone(),
            label: "Dry cadence".into(),
            direction: Direction::Render,
            format,
        }))
        .unwrap();
    let source_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let source_address = source_socket.local_addr().unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut source = start_source(
        &mut backend,
        CaptureSpec::Monitor {
            sink: DeviceSelection::Explicit(id),
            format,
        },
        SessionConfig {
            socket: audio_media::socket::MediaSocket::new(source_socket).unwrap(),
            peer: peer.local_addr().unwrap(),
            session_id: [9; 16],
            profile: Profile::Ethernet,
            authorized: Arc::new(AtomicBool::new(true)),
        },
        material(),
    )
    .unwrap();
    let mut opener = Opener::new(material().forward, [9; 16]).unwrap();
    let mut sealer = Sealer::new(material().reverse, [9; 16], 0).unwrap();
    let start = Instant::now();
    let warmup = Duration::from_secs(1);
    let duration = Duration::from_secs(5);
    let mut capture_due = Duration::ZERO;
    let mut packet = [0; MAX_DATAGRAM_LEN];
    let mut reply = [0; MAX_DATAGRAM_LEN];
    let mut replied = false;
    let mut measured = 0_u64;
    let mut timestamp_first = None;
    let mut timestamp_last = 0;
    while start.elapsed() < warmup + duration {
        let elapsed = start.elapsed();
        while capture_due <= elapsed {
            source.stream_mut().unwrap().pump(480).unwrap();
            capture_due += Duration::from_millis(10);
        }
        while let Ok((n, sender)) = peer.recv_from(&mut packet) {
            assert_eq!(sender, source_address);
            let (header, payload) = opener.open(&mut packet[..n], |_| true).unwrap();
            if header.kind == PacketKind::PathChallenge && !replied {
                let n = sealer
                    .seal(
                        elapsed.as_millis() as u64,
                        PacketKind::PathResponse,
                        0,
                        0,
                        payload,
                        &mut reply,
                    )
                    .unwrap();
                peer.send_to(&reply[..n], source_address).unwrap();
                replied = true;
                // No more incoming traffic whatsoever: not even receiver reports.
            }
            if header.kind == PacketKind::Opus && elapsed >= warmup {
                measured += 1;
                timestamp_first.get_or_insert(header.media_timestamp);
                timestamp_last = header.media_timestamp;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    let health = source.snapshot();
    source.stop().unwrap();
    let fps = measured as f64 / duration.as_secs_f64();
    println!("silent-control source: measured_packets={measured} duration_s=5 fps={fps:.3} capture_dropped_frames={}", health.capture_dropped_frames);
    assert!(replied, "authenticated path never validated");
    assert!(
        (99.0..=101.0).contains(&fps),
        "control starvation changed media cadence: {fps} fps"
    );
    assert_eq!(health.capture_dropped_frames, 0);
    assert_eq!(
        timestamp_last.wrapping_sub(timestamp_first.unwrap()),
        (measured - 1) as u32 * 480,
        "the sender skipped media frames despite timely capture"
    );
}
