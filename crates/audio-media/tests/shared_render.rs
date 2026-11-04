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
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

// This fixture stops and catches up its Dry device explicitly. It has no
// independently advancing native device clock to extrapolate between pumps.
struct ToneStream {
    inner: DryStream,
    manual_render_clock: bool,
}
impl std::ops::Deref for ToneStream {
    type Target = DryStream;
    fn deref(&self) -> &DryStream {
        &self.inner
    }
}
impl std::ops::DerefMut for ToneStream {
    fn deref_mut(&mut self) -> &mut DryStream {
        &mut self.inner
    }
}
impl Stream for ToneStream {
    fn format(&self) -> AudioFormat {
        self.inner.format()
    }
    fn device(&self) -> Option<&DeviceId> {
        self.inner.device()
    }
    fn feedback_guarantee(&self) -> FeedbackGuarantee {
        self.inner.feedback_guarantee()
    }
    fn snapshot(&self) -> StreamSnapshot {
        let mut snapshot = self.inner.snapshot();
        if self.manual_render_clock {
            snapshot.device_position = None;
            snapshot.clock_frequency = None;
            snapshot.host_time_100ns = None;
        }
        snapshot
    }
    fn suppress_local_playback(
        &mut self,
        admitted: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), BackendError> {
        self.inner.suppress_local_playback(admitted)
    }
    fn stop(&mut self) -> Result<(), BackendError> {
        self.inner.stop()
    }
}

struct ToneBackend {
    dry: DryBackend,
    frequency: f64,
    render_opens: usize,
    manual_render_clock: bool,
    silent_until: u64,
    onset: Option<(Instant, Arc<AtomicU64>)>,
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
            manual_render_clock: false,
            silent_until: 0,
            onset: None,
        }
    }
}
impl Backend for ToneBackend {
    type Stream = ToneStream;
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
    ) -> Result<ToneStream, BackendError> {
        let frequency = self.frequency;
        let silent_until = self.silent_until;
        let onset = self.onset.clone();
        let mut scratch = [0.0; MAX_CAPTURE_BLOCK_FRAMES * 2];
        let mut position = 0u64;
        let stream = self.dry.start_capture(
            request,
            Box::new(move |block| {
                if position <= silent_until && position + block.frames as u64 > silent_until {
                    if let Some((clock, observed)) = &onset {
                        observed.store(clock.elapsed().as_nanos() as u64 + 1, Ordering::Release);
                    }
                }
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
                    let sample = if position + index as u64 >= silent_until {
                        sample
                    } else {
                        0.0
                    };
                    *frame = [sample; 2];
                }
                position += block.frames as u64;
                callback(CaptureBlock {
                    samples: &scratch[..block.frames * 2],
                    ..block
                });
            }),
        )?;
        Ok(ToneStream {
            inner: stream,
            manual_render_clock: false,
        })
    }
    fn start_render(
        &mut self,
        request: RenderRequest,
        callback: RenderCallback,
    ) -> Result<ToneStream, BackendError> {
        let stream = self.dry.start_render(request, callback)?;
        self.render_opens += 1;
        Ok(ToneStream {
            inner: stream,
            manual_render_clock: self.manual_render_clock,
        })
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
    // The attack must not stall this fixture's simulated native device clock.
    let mut attack_sealer =
        audio_media::crypto::Sealer::new(material(0).forward, [1; 16], 0).unwrap();
    let mut attack = [0; audio_model::packet::MAX_DATAGRAM_LEN];
    let mut attack_len = 0;
    for _ in 0..4096 {
        attack_len = attack_sealer
            .seal(
                0,
                audio_model::PacketKind::PathChallenge,
                0,
                0,
                &[0; 16],
                &mut attack,
            )
            .unwrap();
    }
    let frequencies = [600.0, 1100.0, 1700.0];
    let mut output_backend = ToneBackend::new(0.0, rate);
    output_backend.manual_render_clock = true;
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
    let mut media_start: Option<Instant> = None;
    let mut due = Duration::ZERO;
    let mut last_position = 0;
    let mut injected = false;
    let mut received_at_injection = None;
    let mut rejected_at_injection = None;
    // Compare gains only after all paths and tones exist, never against a retry/PLC transient.
    // Startup latency has its own strict onset and delayed-validation regressions.
    while media_start.is_none_or(|ready| ready.elapsed() < Duration::from_millis(4500)) {
        let elapsed = start.elapsed();
        if media_start.is_none() {
            assert!(
                elapsed < Duration::from_secs(3),
                "all three tones failed to start"
            );
        }
        let ms = media_start.map_or(0, |ready| ready.elapsed().as_millis());
        if ms >= 700 && !injected {
            if let Some(socket) = &inject_from_b {
                rejected_at_injection = Some(listeners[0].snapshot().rejected_datagrams);
                socket
                    .send_to(&attack[..attack_len], listener_address)
                    .unwrap();
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
                            assert_eq!(
                                listeners[0].snapshot().rejected_datagrams,
                                rejected_at_injection.unwrap() + 1
                            );
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
            if media_start.is_none()
                && sources
                    .iter_mut()
                    .all(|source| source.snapshot().sent_media_packets > 0)
                && listeners
                    .iter_mut()
                    .all(|listener| listener.snapshot().health.received_packets > 0)
            {
                let mono = tail.iter().step_by(2).copied().collect::<Vec<_>>();
                if frequencies
                    .iter()
                    .all(|&frequency| amplitude(&mono, frequency, rate) > 0.04)
                {
                    media_start = Some(Instant::now());
                }
            }
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

#[test]
fn wired_runtime_tone_onset_stays_in_budget_after_silent_warmup() {
    const RATE: u32 = 48_000;
    const QUANTUM_FRAMES: usize = 480;
    const CAPTURE_ACQUISITION_US: u32 = 10_000;
    const NATIVE_FIFO_US: u32 = 20_000;
    // Dry pumps complete blocks; readout is quantized to one output callback.
    // Actual host-scheduler overshoot is measured separately, never assumed zero.
    const CALLBACK_QUANTUM_US: u32 = QUANTUM_FRAMES as u32 * 1_000_000 / RATE;
    // Portable measurement floor: Windows' high-resolution wait is still budgeted
    // at 1ms, Linux at 0.1ms; other hosts use the conservative Windows floor.
    const HOST_TIMER_FLOOR_US: u64 = if cfg!(target_os = "linux") { 100 } else { 1000 };
    // The amplitude onset search is quantized to one 48kHz sample.
    const ONSET_EPSILON_US: u64 = 1_000_000_u64.div_ceil(RATE as u64);
    let mut output_backend = ToneBackend::new(0.0, 48_000);
    let mut group = ListenerGroup::new(&mut output_backend, request()).unwrap();
    let listener_socket = socket();
    let source_socket = socket();
    let config = |socket, peer| SessionConfig {
        socket,
        peer,
        session_id: [61; 16],
        profile: Profile::Ethernet,
        authorized: Arc::new(AtomicBool::new(true)),
    };
    let mut listener = group
        .join(
            config(listener_socket.clone(), source_socket.local_addr().unwrap()),
            material(61),
        )
        .unwrap();
    let mut source_backend = ToneBackend::new(1230.0, 48_000);
    source_backend.silent_until = 48_000;
    let timeline = Instant::now();
    let capture_onset = Arc::new(AtomicU64::new(0));
    source_backend.onset = Some((timeline, capture_onset.clone()));
    let mut source = start_source(
        &mut source_backend,
        CaptureSpec::SystemExcludingOwnProcessTree {
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        config(source_socket, listener_socket.local_addr().unwrap()),
        material(61),
    )
    .unwrap();
    let mut rendered = Vec::with_capacity(140 * 480);
    let start = Instant::now();
    let mut warmup_health = None;
    let mut stages = Vec::new();
    let mut output_onset = None;
    let mut callback_lateness = Duration::ZERO;
    for frame in 0..140 {
        let due = start + Duration::from_millis(frame * 10);
        thread::sleep(due.saturating_duration_since(Instant::now()));
        source.stream_mut().unwrap().pump(QUANTUM_FRAMES).unwrap();
        if capture_onset.load(Ordering::Acquire) != 0 && output_onset.is_none() {
            callback_lateness =
                callback_lateness.max(Instant::now().saturating_duration_since(due));
        }
        let output = group.stream_mut().unwrap();
        let render_at = timeline.elapsed().as_secs_f64();
        output.pump(QUANTUM_FRAMES).unwrap();
        let recent: Vec<f32> = output.rendered_samples().collect();
        if output_onset.is_none() {
            if let Some(offset) = recent[recent.len() - QUANTUM_FRAMES * 2..]
                .as_chunks::<2>()
                .0
                .iter()
                .position(|frame| frame[0].abs() > 0.01)
            {
                output_onset = Some(render_at + offset as f64 / f64::from(RATE));
            }
        }
        rendered.extend(
            recent[recent.len() - QUANTUM_FRAMES * 2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|frame| frame[0]),
        );
        group.refresh();
        if frame == 80 {
            warmup_health = Some(listener.snapshot().health);
        }
        if frame >= 90 {
            stages.push((source.snapshot().latency, listener.snapshot().latency));
        }
    }
    let health = listener.snapshot().health;
    let warmup = warmup_health.unwrap();
    source.stop().unwrap();
    listener.stop().unwrap();
    group.stop().unwrap();
    let captured = capture_onset.load(Ordering::Acquire);
    assert!(captured > 0, "capture onset observed");
    let captured_seconds = (captured - 1) as f64 / 1_000_000_000.0;
    // Dry delivers a complete capture block at each pump. Charge its acquisition
    // and an explicit hardware FIFO model; neither is a Windows hardware measurement.
    // Compare callback instants, not logical frames: a delayed pump advances no audio.
    let latency_ms = (output_onset.expect("tone rendered") - captured_seconds) * 1000.0
        + f64::from(CAPTURE_ACQUISITION_US + NATIVE_FIFO_US) / 1000.0;
    let (source_stages, listener_stages) = stages.last().copied().unwrap();
    assert_eq!(source_stages.packetization_us, 10_000);
    assert_eq!(listener_stages.jitter_target_us, 30_000);
    // Encoded occupancy overlaps the jitter target. Decoded/render queue snapshots
    // vary within a callback and are not another fixed pipeline delay. The missing
    // stage in the old 86ms assertion was the complete mixer/readout callback quantum:
    // ready PCM can miss this pump and first become observable at the next one.
    let stage_sum_us = CAPTURE_ACQUISITION_US
        + source_stages.packetization_us
        + source_stages.capture_queue_us
        + source_stages.codec_lookahead_us
        + source_stages.resampler_delay_us
        + listener_stages.jitter_target_us
        + listener_stages.resampler_delay_us
        + NATIVE_FIFO_US;
    let stage_sum_ms = f64::from(stage_sum_us) / 1000.0;
    // Keep the Ethernet planning-stage ceiling. Callback quantization and the
    // measured sleep/pump overshoot belong only to this non-realtime Dry harness.
    assert!(
        (55.0..=86.0).contains(&stage_sum_ms),
        "wired pipeline stages {stage_sum_ms}ms: {source_stages:?}, {listener_stages:?}"
    );
    assert!(
        callback_lateness <= Duration::from_micros(u64::from(CALLBACK_QUANTUM_US)),
        "Dry callback scheduler missed more than one quantum: {callback_lateness:?}"
    );
    let measured_overshoot_us = callback_lateness.as_nanos().div_ceil(1000) as u64;
    let scheduling_us = u64::from(CALLBACK_QUANTUM_US)
        + measured_overshoot_us.max(HOST_TIMER_FLOOR_US)
        + ONSET_EPSILON_US;
    let upper_ms = (u64::from(stage_sum_us) + scheduling_us) as f64 / 1000.0;
    println!("actual Dry/socket/shared-mixer onset={latency_ms:.4}ms; fixed stages={stage_sum_ms:.4}ms including modeled capture10ms/native20ms; callback quantum={}ms, measured host overshoot={callback_lateness:?}, OS floor={HOST_TIMER_FLOOR_US}us, sample epsilon={ONSET_EPSILON_US}us, test bound={upper_ms:.4}ms; stages={:?}", CALLBACK_QUANTUM_US / 1000, stages.last());
    assert!(
        (55.0..=upper_ms).contains(&latency_ms),
        "wired runtime {latency_ms}ms exceeds stages {stage_sum_ms}ms + callback scheduling {}ms",
        scheduling_us as f64 / 1000.0
    );
    assert_eq!(health.lost_frames, warmup.lost_frames, "post-warmup loss");
    assert_eq!(
        health.late_frames, warmup.late_frames,
        "post-warmup late frames"
    );
    assert_eq!(health.underruns, warmup.underruns, "post-warmup underruns");
    assert!(rendered.iter().all(|sample| sample.is_finite()));
}
