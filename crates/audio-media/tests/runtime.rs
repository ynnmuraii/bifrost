//! Only explicitly pumped Dry streams and ephemeral IPv4 loopback sockets are used.
use audio_backend::{
    dry::{DryBackend, ScriptEvent},
    AudioFormat, Backend, Capabilities, CaptureSpec, Device, DeviceId, DeviceSelection, Direction,
    RenderRequest,
};
use audio_media::{
    crypto::DirectionMaterial,
    runtime::{
        start_listener, start_source, MediaEvent, SessionConfig, SessionMaterial, StopReason,
        STOP_DEADLINE,
    },
};
use audio_model::{
    jitter::Profile,
    packet::{HEADER_LEN, MAX_DATAGRAM_LEN},
    Header, PacketKind,
};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
fn material() -> SessionMaterial {
    SessionMaterial {
        forward: DirectionMaterial::new(Zeroizing::new([1; 32]), Zeroizing::new([2; 4])),
        reverse: DirectionMaterial::new(Zeroizing::new([3; 32]), Zeroizing::new([4; 4])),
    }
}
fn socket() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").unwrap()
}
fn backend(rate: u32) -> (DryBackend, DeviceId) {
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        ..Capabilities::default()
    });
    let id = DeviceId::new("DRY_RENDER").unwrap();
    backend
        .apply(ScriptEvent::UpsertDevice(Device {
            id: id.clone(),
            label: "Dry render".into(),
            direction: Direction::Render,
            format: AudioFormat {
                sample_rate: rate,
                channels: 2,
            },
        }))
        .unwrap();
    while backend.poll_event().is_some() {}
    (backend, id)
}
struct Relay {
    socket: UdpSocket,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Relay {
    fn start(source: SocketAddr, listener: SocketAddr, loss: bool) -> Self {
        let socket = socket();
        socket
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let worker_socket = socket.try_clone().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        let worker = thread::spawn(move || {
            let mut packet = [0; MAX_DATAGRAM_LEN + 1];
            let mut rng = 17u64;
            let mut sequence = 0;
            let mut held = None::<Vec<u8>>;
            while !running.load(Ordering::Acquire) {
                let Ok((n, peer)) = worker_socket.recv_from(&mut packet) else {
                    continue;
                };
                let target = if peer == source {
                    listener
                } else if peer == listener {
                    source
                } else {
                    continue;
                };
                let is_media =
                    peer == source && n >= HEADER_LEN && packet[5] == PacketKind::Opus as u8;
                if is_media && loss {
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    sequence += 1;
                    if rng >> 32 < u64::from(u32::MAX) / 20 {
                        continue;
                    }
                    if sequence % 23 == 5 {
                        held = Some(packet[..n].to_vec());
                        continue;
                    }
                }
                worker_socket.send_to(&packet[..n], target).unwrap();
                if is_media {
                    if let Some(previous) = held.take() {
                        worker_socket.send_to(&previous, listener).unwrap();
                    }
                }
            }
        });
        Self {
            socket,
            stop,
            thread: Some(worker),
        }
    }
    fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}
#[derive(Default)]
struct Energy {
    sum: f64,
    count: u64,
    real: f64,
    imaginary: f64,
}
impl Energy {
    fn add(&mut self, samples: impl Iterator<Item = f32>, frequency: f64, rate: u32) {
        for x in samples {
            assert!(x.is_finite());
            self.sum += f64::from(x).powi(2);
            let phase =
                std::f64::consts::TAU * frequency * (self.count / 2) as f64 / f64::from(rate);
            self.real += f64::from(x) * phase.cos();
            self.imaginary += f64::from(x) * phase.sin();
            self.count += 1;
        }
    }
    fn rms(&self) -> f64 {
        (self.sum / self.count.max(1) as f64).sqrt()
    }
    fn tone_amplitude(&self) -> f64 {
        2.0 * self.real.hypot(self.imaginary) / self.count.max(1) as f64
    }
}

#[test]
fn dry_udp_engines_resample_mix_control_ignore_injection_and_stop() {
    for (capture_rate, render_rate, profile, loss) in [
        (48_000, 48_000, Profile::Ethernet, false),
        (44_100, 44_100, Profile::Ethernet, false),
        (48_000, 48_000, Profile::Wifi, true),
    ] {
        exercise(capture_rate, render_rate, profile, loss);
    }
}
fn exercise(capture_rate: u32, render_rate: u32, profile: Profile, loss: bool) {
    let (mut source_backend, source_id) = backend(capture_rate);
    let (mut listener_backend, listener_id) = backend(render_rate);
    let source_socket = socket();
    let listener_socket = socket();
    let listener_address = listener_socket.local_addr().unwrap();
    let relay = Relay::start(source_socket.local_addr().unwrap(), listener_address, loss);
    let mut source = start_source(
        &mut source_backend,
        CaptureSpec::Monitor {
            sink: DeviceSelection::Explicit(source_id),
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            authorized: Arc::new(AtomicBool::new(true)),
            socket: source_socket,
            peer: relay.address(),
            session_id: [9; 16],
            profile,
        },
        material(),
    )
    .unwrap();
    let mut listener = start_listener(
        &mut listener_backend,
        RenderRequest {
            device: DeviceSelection::Explicit(listener_id.clone()),
            requested: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            authorized: Arc::new(AtomicBool::new(true)),
            socket: listener_socket,
            peer: relay.address(),
            session_id: [9; 16],
            profile,
        },
        material(),
    )
    .unwrap();
    let foreign = socket();
    let mut injected = [0; 64];
    let header = Header {
        kind: PacketKind::Opus,
        flags: 0,
        session_id: [9; 16],
        key_epoch: 1,
        packet_number: 1_000_000,
        media_timestamp: 0,
        frame_samples: profile.frame_samples(),
        payload_len: 4,
    };
    injected[..HEADER_LEN].copy_from_slice(&header.encode().unwrap());
    let start = Instant::now();
    let mut capture_due = Duration::ZERO;
    let mut render_due = Duration::ZERO;
    let mut captures = 0;
    let mut source_started = false;
    let mut listener_started = false;
    let mut loud = Energy::default();
    let mut quiet = Energy::default();
    let mut muted = Energy::default();
    let mut resumed = Energy::default();
    let mut gain_set = false;
    let mut mute_set = false;
    let mut unmute_set = false;
    let mut injection_done = false;
    while start.elapsed() < Duration::from_millis(2400) {
        let elapsed = start.elapsed();
        if elapsed >= Duration::from_millis(900) && !gain_set {
            listener.set_gain(0.25).unwrap();
            gain_set = true;
        }
        if elapsed >= Duration::from_millis(1300) && !mute_set {
            listener.set_muted(true).unwrap();
            mute_set = true;
        }
        if elapsed >= Duration::from_millis(1700) && !unmute_set {
            listener.set_gain(1.0).unwrap();
            listener.set_muted(false).unwrap();
            unmute_set = true;
        }
        if elapsed >= Duration::from_millis(600) && !injection_done {
            for _ in 0..8 {
                foreign.send_to(&injected, listener_address).unwrap();
                relay.socket.send_to(&injected, listener_address).unwrap();
            }
            relay
                .socket
                .send_to(&[0; MAX_DATAGRAM_LEN + 100], listener_address)
                .unwrap();
            injection_done = true;
        }
        if elapsed >= capture_due {
            // Native rate is authoritative, and native blocks need not match Opus frame size.
            let frames = if capture_rate == 44_100 || captures % 2 == 0 {
                441
            } else {
                480
            };
            source.stream_mut().unwrap().pump(frames).unwrap();
            captures += 1;
            capture_due += Duration::from_secs_f64(frames as f64 / f64::from(capture_rate));
        }
        if elapsed >= render_due {
            let frames = (render_rate / 100) as usize;
            let stream = listener.stream_mut().unwrap();
            stream.pump(frames).unwrap();
            let samples = stream.rendered_samples().collect::<Vec<_>>();
            let tail = &samples[samples.len().saturating_sub(frames * 2)..];
            let ms = elapsed.as_millis();
            if (400..800).contains(&ms) {
                loud.add(
                    tail.iter().copied(),
                    f64::from(capture_rate) / 128.0,
                    render_rate,
                );
            }
            if (1100..1250).contains(&ms) {
                quiet.add(
                    tail.iter().copied(),
                    f64::from(capture_rate) / 128.0,
                    render_rate,
                );
            }
            if (1500..1650).contains(&ms) {
                muted.add(
                    tail.iter().copied(),
                    f64::from(capture_rate) / 128.0,
                    render_rate,
                );
            }
            if (1950..2350).contains(&ms) {
                resumed.add(
                    tail.iter().copied(),
                    f64::from(capture_rate) / 128.0,
                    render_rate,
                );
            }
            render_due += Duration::from_millis(10);
        }
        while let Some(event) = source.poll_event() {
            match event {
                MediaEvent::Started => source_started = true,
                MediaEvent::Stopped { reason } => panic!("source stopped: {reason:?}"),
                _ => {}
            }
        }
        while let Some(event) = listener.poll_event() {
            match event {
                MediaEvent::Started => listener_started = true,
                MediaEvent::Stopped { reason } => panic!("listener stopped: {reason:?}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    let source_health = source.snapshot();
    let listener_health = listener.snapshot();
    println!("{capture_rate}->{render_rate} {profile:?} loss={loss} RMS loud={:.3} quiet={:.3} muted={:.6} resumed={:.3}; health={:?}",loud.rms(),quiet.rms(),muted.rms(),resumed.rms(),listener_health.health);
    assert!(source_started && listener_started);
    assert!(listener_health.health.received_packets > 50);
    assert!(source_health.health.received_packets > 30);
    assert!(listener_health.rejected_datagrams >= 16);
    assert!(listener_health.health.lost_frames < 40);
    assert!(loud.rms() > 0.08);
    // Dry's stereo sawtooth repeats every 128 native frames: its fundamental must arrive,
    // not merely arbitrary nonzero PCM or a plausible counter.
    assert!(
        loud.tone_amplitude() > 0.1,
        "missing Dry fundamental: {}",
        loud.tone_amplitude()
    );
    assert!(quiet.rms() < loud.rms() * 0.55 && quiet.rms() > loud.rms() * 0.08);
    assert!(muted.rms() < 0.0001);
    assert!(resumed.rms() > loud.rms() * 0.5);
    listener_backend
        .apply(ScriptEvent::InvalidateDevice(listener_id))
        .unwrap();
    while let Some(event) = listener_backend.poll_event() {
        listener.on_backend_event(&event);
    }
    let stopped = Instant::now();
    listener.stop().unwrap();
    source.stop().unwrap();
    assert!(stopped.elapsed() < STOP_DEADLINE);
    let events = std::iter::from_fn(|| listener.poll_event()).collect::<Vec<_>>();
    assert!(events.contains(&MediaEvent::DeviceInvalidated));
    assert!(events.contains(&MediaEvent::Stopped {
        reason: StopReason::DeviceInvalidated
    }));
    source.stop().unwrap();
    listener.stop().unwrap();
}

#[test]
fn native_snapshot_invalidation_stops_without_a_forwarded_backend_event() {
    let (mut backend, id) = backend(48_000);
    let peer = socket();
    let mut listener = start_listener(
        &mut backend,
        RenderRequest {
            device: DeviceSelection::Explicit(id.clone()),
            requested: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            authorized: Arc::new(AtomicBool::new(true)),
            socket: socket(),
            peer: peer.local_addr().unwrap(),
            session_id: [9; 16],
            profile: Profile::Ethernet,
        },
        material(),
    )
    .unwrap();
    backend.apply(ScriptEvent::InvalidateDevice(id)).unwrap();
    assert_eq!(
        listener.snapshot().native.status,
        audio_backend::StreamStatus::DeviceInvalidated
    );
    listener.stop().unwrap();
    let events = std::iter::from_fn(|| listener.poll_event()).collect::<Vec<_>>();
    assert!(events.contains(&MediaEvent::DeviceInvalidated));
    assert!(events.contains(&MediaEvent::Stopped {
        reason: StopReason::DeviceInvalidated
    }));
}

#[test]
fn unvalidated_path_times_out_without_transmitting_captured_audio() {
    let (mut backend, id) = backend(48_000);
    let peer = socket();
    let mut source = start_source(
        &mut backend,
        CaptureSpec::Monitor {
            sink: DeviceSelection::Explicit(id),
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            authorized: Arc::new(AtomicBool::new(true)),
            socket: socket(),
            peer: peer.local_addr().unwrap(),
            session_id: [9; 16],
            profile: Profile::Ethernet,
        },
        material(),
    )
    .unwrap();
    let start = Instant::now();
    let mut stopped = false;
    while start.elapsed() < Duration::from_secs(3) {
        source.stream_mut().unwrap().pump(480).unwrap();
        while let Some(event) = source.poll_event() {
            assert_ne!(event, MediaEvent::Started);
            if event
                == (MediaEvent::Stopped {
                    reason: StopReason::PathTimeout,
                })
            {
                stopped = true;
            }
        }
        if stopped {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(stopped);
    source.stop().unwrap();
    assert!(source.snapshot().sent_datagrams <= 3);
    peer.set_read_timeout(Some(Duration::from_millis(2)))
        .unwrap();
    let mut packet = [0; MAX_DATAGRAM_LEN + 1];
    let mut challenges = 0;
    while let Ok((n, _)) = peer.recv_from(&mut packet) {
        let header = audio_model::validate_datagram(&packet[..n]).unwrap();
        assert_eq!(header.kind, PacketKind::PathChallenge);
        challenges += 1;
    }
    assert_eq!(challenges, 3);
}

#[test]
fn authorization_gate_rejects_start_and_revokes_existing_render() {
    let (mut backend, id) = backend(48_000);
    let peer = socket();
    let config = |authorized| SessionConfig {
        socket: socket(),
        peer: peer.local_addr().unwrap(),
        session_id: [33; 16],
        profile: Profile::Ethernet,
        authorized,
    };
    let request = || RenderRequest {
        device: DeviceSelection::Explicit(id.clone()),
        requested: AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        },
    };
    assert!(matches!(
        start_listener(
            &mut backend,
            request(),
            config(Arc::new(AtomicBool::new(false))),
            material()
        ),
        Err(audio_media::runtime::EngineError::Unauthorized)
    ));
    let authorized = Arc::new(AtomicBool::new(true));
    let mut session = start_listener(
        &mut backend,
        request(),
        config(authorized.clone()),
        material(),
    )
    .unwrap();
    authorized.store(false, Ordering::Release);
    session.stream_mut().unwrap().pump(480).unwrap();
    assert!(session
        .stream_mut()
        .unwrap()
        .rendered_samples()
        .all(|sample| sample == 0.0));
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if let Some(MediaEvent::Stopped { reason }) = session.poll_event() {
            assert_eq!(reason, StopReason::AuthorizationLost);
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    session.stop().unwrap();
}
