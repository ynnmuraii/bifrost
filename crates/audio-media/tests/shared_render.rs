//! Explicit Dry pumping and loopback UDP only; no native devices are opened.
use audio_backend::{
    dry::{DryBackend, DryMuteGuard, DryStream, ScriptEvent},
    *,
};
use audio_media::{
    crypto::DirectionMaterial,
    runtime::{
        start_source, EngineError, ListenerGroup, MediaEvent, SessionConfig, SessionMaterial,
        StopReason,
    },
};
use audio_model::jitter::Profile;
use std::{
    net::UdpSocket,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

struct ToneBackend {
    dry: DryBackend,
    frequency: f64,
    render_opens: usize,
}
impl ToneBackend {
    fn new(frequency: f64, rate: u32) -> Self {
        let mut dry = DryBackend::new(Capabilities {
            system_capture: true,
            exclude_own_process_tree: true,
            ..Capabilities::default()
        });
        let id = DeviceId::new("DRY_OUTPUT").unwrap();
        dry.apply(ScriptEvent::UpsertDevice(Device {
            id: id.clone(),
            label: "Dry output".into(),
            direction: Direction::Render,
            format: AudioFormat {
                sample_rate: rate,
                channels: 2,
            },
        }))
        .unwrap();
        let endpoints = DefaultEndpoints {
            render: Some(id),
            capture: None,
        };
        dry.apply(ScriptEvent::SetDefaults(Defaults {
            multimedia: endpoints.clone(),
            communications: endpoints,
        }))
        .unwrap();
        while dry.poll_event().is_some() {}
        Self {
            dry,
            frequency,
            render_opens: 0,
        }
    }
}
impl Backend for ToneBackend {
    type Stream = DryStream;
    type Mute = DryMuteGuard;
    fn capabilities(&self) -> Capabilities {
        self.dry.capabilities()
    }
    fn probe_capabilities(&mut self) -> CapabilityProbe {
        self.dry.probe_capabilities()
    }
    fn devices(&self) -> Result<Vec<Device>, BackendError> {
        self.dry.devices()
    }
    fn defaults(&self) -> Result<Defaults, BackendError> {
        self.dry.defaults()
    }
    fn start_capture(
        &mut self,
        request: CaptureSpec,
        mut callback: CaptureCallback,
    ) -> Result<DryStream, BackendError> {
        let frequency = self.frequency;
        let mut scratch = [0.0; MAX_CAPTURE_BLOCK_FRAMES * 2];
        let mut position = 0u64;
        self.dry.start_capture(
            request,
            Box::new(move |block| {
                for (index, frame) in scratch[..block.frames * 2]
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .enumerate()
                {
                    let sample =
                        (std::f64::consts::TAU * frequency * (position + index as u64) as f64
                            / f64::from(block.format.sample_rate))
                        .sin() as f32
                            * 0.22;
                    *frame = [sample; 2];
                }
                position += block.frames as u64;
                callback(CaptureBlock {
                    samples: &scratch[..block.frames * 2],
                    ..block
                });
            }),
        )
    }
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<DryStream, BackendError> {
        let stream = self.dry.start_render(request, callback)?;
        self.render_opens += 1;
        Ok(stream)
    }
    fn capture_mute(&self, selection: &DeviceSelection) -> Result<MuteSnapshot, BackendError> {
        self.dry.capture_mute(selection)
    }
    fn acquire_mute(&mut self, selection: &DeviceSelection) -> Result<DryMuteGuard, BackendError> {
        self.dry.acquire_mute(selection)
    }
    fn media_sessions(&self) -> Result<Vec<MediaInfo>, BackendError> {
        self.dry.media_sessions()
    }
    fn current_media_session(&self) -> Result<Option<MediaSessionToken>, BackendError> {
        self.dry.current_media_session()
    }
    fn media_command(&mut self, request: MediaCommandRequest) -> Result<(), BackendError> {
        self.dry.media_command(request)
    }
    fn cancel_media_commands(&mut self) {
        self.dry.cancel_media_commands();
    }
    fn configure_ptt(
        &mut self,
        hotkey: Option<PttHotkey>,
        reserved: &[PttHotkey],
    ) -> Result<(), BackendError> {
        self.dry.configure_ptt(hotkey, reserved)
    }
    fn poll_event(&mut self) -> Option<Event> {
        self.dry.poll_event()
    }
}
fn socket() -> Arc<audio_media::socket::MediaSocket> {
    audio_media::socket::MediaSocket::new(UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap()
}
fn material(index: u8) -> SessionMaterial {
    SessionMaterial {
        forward: DirectionMaterial::new(
            Zeroizing::new([10 + index; 32]),
            Zeroizing::new([30 + index; 4]),
        ),
        reverse: DirectionMaterial::new(
            Zeroizing::new([20 + index; 32]),
            Zeroizing::new([40 + index; 4]),
        ),
    }
}
fn request() -> RenderRequest {
    RenderRequest {
        device: DeviceSelection::FollowDefault(DefaultRole::Multimedia),
        requested: AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        },
    }
}
fn amplitude(samples: &[f32], frequency: f64, rate: u32) -> f64 {
    let coefficient = 2.0 * (std::f64::consts::TAU * frequency / f64::from(rate)).cos();
    let (mut previous, mut previous2) = (0.0, 0.0);
    for &sample in samples {
        let next = f64::from(sample) + coefficient * previous - previous2;
        previous2 = previous;
        previous = next;
    }
    2.0 * (previous * previous + previous2 * previous2 - coefficient * previous * previous2)
        .max(0.0)
        .sqrt()
        / samples.len() as f64
}
#[test]
fn three_independent_profiles_share_one_output_and_keep_other_tones_on_gain_mute_leave() {
    for rate in [48_000, 44_100] {
        exercise(rate, false);
    }
}
#[test]
fn three_sources_share_one_socket_and_one_listener_port() {
    exercise(48_000, true);
}
fn exercise(rate: u32, shared_source: bool) {
    let frequencies = [600.0, 1100.0, 1700.0];
    let mut output_backend = ToneBackend::new(0.0, rate);
    let mut group = ListenerGroup::new(&mut output_backend, request()).unwrap();
    let mut sources = Vec::new();
    let mut listeners = Vec::new();
    let listener_socket = socket();
    let listener_address = listener_socket.local_addr().unwrap();
    let shared_source_socket = shared_source.then(socket);
    let mut inject_from_b = None;
    for (index, &frequency) in frequencies.iter().enumerate() {
        let source_socket = if let Some(socket) = &shared_source_socket {
            socket.clone()
        } else {
            let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
            if index == 1 {
                inject_from_b = Some(raw.try_clone().unwrap());
            }
            audio_media::socket::MediaSocket::new(raw).unwrap()
        };
        let source_address = source_socket.local_addr().unwrap();
        let profile = if index == 1 {
            Profile::Wifi
        } else {
            Profile::Ethernet
        };
        let config = |socket, peer| SessionConfig {
            socket,
            peer,
            session_id: [index as u8 + 1; 16],
            profile,
            authorized: Arc::new(AtomicBool::new(true)),
        };
        listeners.push(
            group
                .join(
                    config(listener_socket.clone(), source_address),
                    material(index as u8),
                )
                .unwrap(),
        );
        let mut backend = ToneBackend::new(frequency, 48_000);
        sources.push(
            start_source(
                &mut backend,
                CaptureSpec::SystemExcludingOwnProcessTree {
                    format: AudioFormat {
                        sample_rate: 48_000,
                        channels: 2,
                    },
                },
                config(source_socket, listener_address),
                material(index as u8),
            )
            .unwrap(),
        );
    }
    assert!(matches!(
        group.join(
            SessionConfig {
                socket: socket(),
                peer: socket().local_addr().unwrap(),
                session_id: [9; 16],
                profile: Profile::Wifi,
                authorized: Arc::new(AtomicBool::new(true))
            },
            material(8)
        ),
        Err(EngineError::GroupFull)
    ));
    assert!(!group.accepts(&RenderRequest {
        device: DeviceSelection::Explicit(DeviceId::new("OTHER").unwrap()),
        ..request()
    }));
    let mut windows: [Vec<f32>; 4] = std::array::from_fn(|_| Vec::new());
    let mut actions = [false; 6];
    let start = Instant::now();
    let mut due = Duration::ZERO;
    let mut last_position = 0;
    let mut injected = false;
    let mut received_at_injection = None;
    // A replacement route may need the bounded 500 ms path-probe retry before media starts.
    while start.elapsed() < Duration::from_millis(4500) {
        let elapsed = start.elapsed();
        let ms = elapsed.as_millis();
        if ms >= 700 && !injected {
            if let Some(socket) = &inject_from_b {
                let rejected_before = listeners[0].snapshot().rejected_datagrams;
                let mut sealer =
                    audio_media::crypto::Sealer::new(material(0).forward, [1; 16], 0).unwrap();
                let mut packet = [0; audio_model::packet::MAX_DATAGRAM_LEN];
                // Advance beyond legitimate A's replay window; wrong-tuple packets must never reach it.
                for number in 0..4096 {
                    let n = sealer
                        .seal(
                            0,
                            audio_model::PacketKind::PathChallenge,
                            0,
                            0,
                            &[0; 16],
                            &mut packet,
                        )
                        .unwrap();
                    if number == 4095 {
                        socket.send_to(&packet[..n], listener_address).unwrap();
                    }
                }
                thread::sleep(Duration::from_millis(10));
                assert_eq!(
                    listeners[0].snapshot().rejected_datagrams,
                    rejected_before + 1
                );
                assert!(listeners[0].snapshot().health.received_packets > 10);
                received_at_injection = Some(listeners[0].snapshot().health.received_packets);
            }
            injected = true;
        }
        for (index, at) in [900, 1300, 1500, 2000, 2300, 3100].into_iter().enumerate() {
            if ms >= at && !actions[index] {
                match index {
                    0 => {
                        if let Some(before) = received_at_injection {
                            assert!(listeners[0].snapshot().health.received_packets > before);
                        }
                        listeners[1].set_gain(0.25).unwrap();
                    }
                    1 => listeners[1].set_gain(1.0).unwrap(),
                    2 => listeners[1].set_muted(true).unwrap(),
                    3 => listeners[1].set_muted(false).unwrap(),
                    4 => {
                        listeners[0].stop().unwrap();
                        sources[0].stop().unwrap();
                    }
                    5 => {
                        let source_socket = socket();
                        let source_address = source_socket.local_addr().unwrap();
                        let config = |socket, peer| SessionConfig {
                            socket,
                            peer,
                            session_id: [19; 16],
                            profile: Profile::Ethernet,
                            authorized: Arc::new(AtomicBool::new(true)),
                        };
                        listeners.push(
                            group
                                .join(config(listener_socket.clone(), source_address), material(9))
                                .unwrap(),
                        );
                        let mut backend = ToneBackend::new(frequencies[0], 48_000);
                        sources.push(
                            start_source(
                                &mut backend,
                                CaptureSpec::SystemExcludingOwnProcessTree {
                                    format: AudioFormat {
                                        sample_rate: 48_000,
                                        channels: 2,
                                    },
                                },
                                config(source_socket, listener_address),
                                material(9),
                            )
                            .unwrap(),
                        );
                    }
                    _ => unreachable!(),
                }
                actions[index] = true;
            }
        }
        if elapsed >= due {
            for source in &mut sources {
                if let Some(stream) = source.stream_mut() {
                    stream.pump(480).unwrap();
                }
            }
            let frames = (rate / 100) as usize;
            let stream = group.stream_mut().unwrap();
            stream.pump(frames).unwrap();
            assert!(stream.processed_frames() > last_position);
            last_position = stream.processed_frames();
            let samples = stream.rendered_samples().collect::<Vec<_>>();
            let tail = &samples[samples.len() - frames * 2..];
            if (2300..2450).contains(&ms) || (3100..3250).contains(&ms) {
                let remaining = tail.iter().step_by(2).copied().collect::<Vec<_>>();
                assert!(
                    amplitude(&remaining, frequencies[2], rate) > 0.02,
                    "remaining tone interrupted on join/leave at {ms} ms, rate {rate}"
                );
            }
            let window = if (500..900).contains(&ms) {
                Some(0)
            } else if (1100..1300).contains(&ms) {
                Some(1)
            } else if (1700..2000).contains(&ms) {
                Some(2)
            } else if (2600..3000).contains(&ms) {
                Some(3)
            } else {
                None
            };
            if let Some(window) = window {
                windows[window].extend(tail.iter().step_by(2));
            }
            group.refresh();
            due += Duration::from_millis(10);
        }
        thread::sleep(Duration::from_millis(1));
    }
    let tones: [[f64; 3]; 4] = std::array::from_fn(|window| {
        std::array::from_fn(|index| amplitude(&windows[window], frequencies[index], rate))
    });
    for index in 0..3 {
        assert!(tones[0][index] > 0.04, "rate={rate} tones={tones:?}");
    }
    assert!(
        tones[1][1] < tones[0][1] * 0.5 && tones[1][1] > tones[0][1] * 0.1,
        "{tones:?}"
    );
    assert!(tones[2][1] < 0.002, "{tones:?}");
    for index in [0, 2] {
        assert!(tones[2][index] > tones[0][index] * 0.7, "{tones:?}");
    }
    assert!(tones[3][0] < 0.002, "{tones:?}");
    for index in [1, 2] {
        assert!(tones[3][index] > tones[0][index] * 0.7, "{tones:?}");
    }
    assert_eq!(output_backend.render_opens, 1);
    for listener in &mut listeners[1..3] {
        assert!(listener.snapshot().health.received_packets > 50);
    }
    assert!(listeners[3].snapshot().health.received_packets > 10);
    assert!(
        listeners[1].snapshot().health.received_packets
            < listeners[2].snapshot().health.received_packets
    );
    group.on_backend_event(&Event::Device(DeviceEvent::DefaultChanged {
        direction: Direction::Render,
        role: DefaultRole::Multimedia,
    }));
    let deadline = Instant::now() + Duration::from_secs(1);
    for listener in &mut listeners[1..] {
        loop {
            if let Some(MediaEvent::Stopped { reason }) = listener.poll_event() {
                assert_eq!(reason, StopReason::DeviceInvalidated);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        listener.stop().unwrap();
    }
    for source in &mut sources {
        source.stop().unwrap();
    }
    group.stop().unwrap();
    assert!(group.stream_mut().is_none());
    assert_eq!(output_backend.render_opens, 1);
}

#[test]
fn authorization_revocation_silences_already_mixed_pcm_without_waiting_for_control() {
    let mut backend = ToneBackend::new(0.0, 48_000);
    let mut group = ListenerGroup::new(&mut backend, request()).unwrap();
    let authorized = Arc::new(AtomicBool::new(true));
    let source_socket = socket();
    let listener_socket = socket();
    let source_address = source_socket.local_addr().unwrap();
    let listener_address = listener_socket.local_addr().unwrap();
    let mut session = group
        .join(
            SessionConfig {
                socket: listener_socket,
                peer: source_address,
                session_id: [71; 16],
                profile: Profile::Ethernet,
                authorized: authorized.clone(),
            },
            material(1),
        )
        .unwrap();
    let mut source_backend = ToneBackend::new(600.0, 48_000);
    let mut source = start_source(
        &mut source_backend,
        CaptureSpec::SystemExcludingOwnProcessTree {
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        SessionConfig {
            socket: source_socket,
            peer: listener_address,
            session_id: [71; 16],
            profile: Profile::Ethernet,
            authorized: Arc::new(AtomicBool::new(true)),
        },
        material(1),
    )
    .unwrap();
    let start = Instant::now();
    let mut due = Duration::ZERO;
    let mut audible = false;
    while start.elapsed() < Duration::from_secs(2) && !audible {
        if start.elapsed() >= due {
            source.stream_mut().unwrap().pump(480).unwrap();
            group.stream_mut().unwrap().pump(480).unwrap();
            group.refresh();
            due += Duration::from_millis(10);
            let samples = group
                .stream_mut()
                .unwrap()
                .rendered_samples()
                .collect::<Vec<_>>();
            audible = samples[samples.len().saturating_sub(960)..]
                .iter()
                .any(|sample| sample.abs() > 0.05);
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(
        audible,
        "path validation and jitter startup must produce PCM before revocation"
    );
    // Allow the summing worker to queue real PCM, then revoke without a control refresh.
    thread::sleep(Duration::from_millis(5));
    authorized.store(false, Ordering::Release);
    group.stream_mut().unwrap().pump(480).unwrap();
    let rendered = group
        .stream_mut()
        .unwrap()
        .rendered_samples()
        .collect::<Vec<_>>();
    assert!(rendered[rendered.len() - 960..]
        .iter()
        .all(|sample| *sample == 0.0));
    session.stop().unwrap();
    group.refresh();
    assert!(group.stream_mut().is_none());
    source.stop().unwrap();
    group.stop().unwrap();
}
