#![cfg(target_os = "linux")]
#[cfg(not(feature = "testing"))]
#[test]
fn live_tests_require_explicit_testing_feature() {
    eprintln!("skipping live Pulse tests: enable testing feature and BIFROST_PULSE_TESTS=1");
}

#[cfg(feature = "testing")]
mod live {
    use audio_backend::*;
    use audio_media::{
        crypto::DirectionMaterial,
        runtime::{self, MediaEvent, SessionConfig, SessionMaterial},
    };
    use audio_model::jitter::Profile;
    use platform_audio_linux::{
        testing::{remaining_test_modules, OwnedNullSink},
        Limitation, LinuxBackend,
    };
    use rtrb::{Consumer, RingBuffer};
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
    fn format() -> AudioFormat {
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
        }
    }
    fn explicit(id: &DeviceId) -> DeviceSelection {
        DeviceSelection::Explicit(id.clone())
    }
    fn monitor(
        backend: &mut LinuxBackend,
        sink: &DeviceId,
    ) -> (platform_audio_linux::LinuxStream, Consumer<f32>) {
        let (mut producer, consumer) = RingBuffer::new(192_000);
        let stream = backend
            .start_capture(
                CaptureSpec::Monitor {
                    sink: explicit(sink),
                    format: format(),
                },
                Box::new(move |block| {
                    assert!(block.frames <= 960 && block.timestamp_valid);
                    if producer.slots() >= block.samples.len() {
                        for &sample in block.samples {
                            let _ = producer.push(sample);
                        }
                    }
                }),
            )
            .unwrap();
        (stream, consumer)
    }
    fn collect(
        consumer: &mut Consumer<f32>,
        duration: Duration,
        mut tick: impl FnMut(),
    ) -> Vec<f32> {
        let start = Instant::now();
        let mut pcm = Vec::new();
        while start.elapsed() < duration {
            while let Ok(sample) = consumer.pop() {
                pcm.push(sample);
            }
            tick();
            thread::sleep(Duration::from_millis(10));
        }
        pcm
    }
    fn goertzel(samples: &[f32], frequency: f64) -> f64 {
        let coefficient = 2.0 * (std::f64::consts::TAU * frequency / 48_000.0).cos();
        let (mut first, mut second) = (0.0, 0.0);
        let mut count = 0;
        for frame in samples.as_chunks::<2>().0 {
            assert!(frame.iter().all(|sample| sample.is_finite()));
            let current = f64::from(frame[0]) + coefficient * first - second;
            second = first;
            first = current;
            count += 1;
        }
        assert!(count > 4800, "not enough captured samples: {count}");
        2.0 * (first * first + second * second - coefficient * first * second)
            .max(0.0)
            .sqrt()
            / count as f64
    }
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Pulse event/state"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn material() -> SessionMaterial {
        SessionMaterial {
            forward: DirectionMaterial::new(Zeroizing::new([42; 32]), Zeroizing::new([43; 4])),
            reverse: DirectionMaterial::new(Zeroizing::new([44; 32]), Zeroizing::new([45; 4])),
        }
    }
    #[test]
    fn owned_monitor_render_udp_path_device_events_and_cleanup() {
        if std::env::var("BIFROST_PULSE_TESTS").as_deref() != Ok("1") {
            eprintln!("skipping live Pulse tests: BIFROST_PULSE_TESTS is not 1");
            return;
        }
        assert!(
            remaining_test_modules().unwrap().is_empty(),
            "pre-existing bifrost_test modules require owner inspection"
        );
        let mut backend = LinuxBackend::connect().unwrap();
        let defaults = backend.defaults().unwrap();
        let mut a = OwnedNullSink::new().unwrap();
        let mut b = OwnedNullSink::new().unwrap();
        wait_until(|| {
            let devices = backend.devices().unwrap();
            devices.iter().any(|device| device.id == *a.sink())
                && devices.iter().any(|device| device.id == *b.sink())
        });
        assert_eq!(
            backend.defaults().unwrap(),
            defaults,
            "null sinks must not change native defaults"
        );
        let events = std::iter::from_fn(|| backend.poll_event()).collect::<Vec<_>>();
        assert!(events
            .iter()
            .any(|event| matches!(event,Event::Device(DeviceEvent::Added(id)) if id==a.sink())));
        assert!(backend.capabilities().system_capture);
        assert_eq!(backend.monitor_availability(), Availability::Available);
        assert!(backend
            .limitations()
            .contains(&Limitation::EndpointMuteOwnershipUnavailable));
        assert_eq!(
            backend.probe_capabilities().process_exclude,
            Availability::Unavailable(BackendError::Unsupported)
        );
        assert_eq!(
            backend.media_sessions().is_ok(),
            backend.capabilities().media
        );
        let source = explicit(&a.monitor().unwrap());
        let before = backend.capture_mute(&source).unwrap();
        assert_eq!(
            backend.acquire_mute(&source).err(),
            Some(BackendError::Unsupported)
        );
        assert_eq!(backend.capture_mute(&source).unwrap(), before);
        eprintln!("mute guard round-trip skipped: EndpointMuteOwnershipUnavailable (accepted contract limitation); observed mute unchanged");
        let mut phase = 0u64;
        let rendered = Arc::new(AtomicU64::new(0));
        let callback_rendered = rendered.clone();
        let mut tone = backend
            .start_render(
                RenderRequest {
                    device: explicit(a.sink()),
                    requested: format(),
                },
                Box::new(move |output| {
                    for frame in output.as_chunks_mut::<2>().0 {
                        let sample = (phase as f64 * std::f64::consts::TAU * 1000.0 / 48_000.0)
                            .sin() as f32
                            * 0.2;
                        frame.fill(sample);
                        phase += 1;
                    }
                    callback_rendered.store(phase, Ordering::Release);
                }),
            )
            .unwrap();
        let (mut direct, mut direct_pcm) = monitor(&mut backend, a.sink());
        assert!(
            !a.muted().unwrap(),
            "owned test sink must be initialized unmuted"
        );
        let samples = collect(&mut direct_pcm, Duration::from_millis(1400), || {});
        let amplitude = goertzel(&samples, 1000.0);
        assert!(rendered.load(Ordering::Acquire) > 4800);
        eprintln!(
            "owned null sink direct 1 kHz amplitude={amplitude:.4}, frames={}",
            samples.len() / 2
        );
        assert!((0.10..0.30).contains(&amplitude));
        assert_eq!(direct.snapshot().status, StreamStatus::Running);
        assert_eq!(direct.snapshot().clock_frequency, Some(48_000));
        direct.stop().unwrap();
        let (mut probe, mut remote_pcm) = monitor(&mut backend, b.sink());
        let source_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let listener_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let source_address = source_socket.local_addr().unwrap();
        let listener_address = listener_socket.local_addr().unwrap();
        let mut source = runtime::start_source(
            &mut backend,
            CaptureSpec::Monitor {
                sink: explicit(a.sink()),
                format: format(),
            },
            SessionConfig {
                authorized: Arc::new(AtomicBool::new(true)),
                socket: source_socket,
                peer: listener_address,
                session_id: [7; 16],
                profile: Profile::Ethernet,
            },
            material(),
        )
        .unwrap();
        let mut listener = runtime::start_listener(
            &mut backend,
            RenderRequest {
                device: explicit(b.sink()),
                requested: format(),
            },
            SessionConfig {
                authorized: Arc::new(AtomicBool::new(true)),
                socket: listener_socket,
                peer: source_address,
                session_id: [7; 16],
                profile: Profile::Ethernet,
            },
            material(),
        )
        .unwrap();
        let mut started = 0;
        let samples = collect(&mut remote_pcm, Duration::from_secs(3), || {
            for event in std::iter::from_fn(|| source.poll_event())
                .chain(std::iter::from_fn(|| listener.poll_event()))
            {
                match event {
                    MediaEvent::Started => started += 1,
                    MediaEvent::Stopped { reason } => panic!("engine stopped: {reason:?}"),
                    _ => {}
                }
            }
        });
        let remote_amplitude = goertzel(&samples[4800.min(samples.len())..], 1000.0);
        eprintln!("owned null sinks Linux→UDP→Linux 1 kHz amplitude={remote_amplitude:.4}; listener health={:?}",listener.snapshot().health);
        assert_eq!(started, 2);
        assert!(remote_amplitude > 0.06 && remote_amplitude < 0.30);
        assert!(source.snapshot().health.received_packets > 50);
        listener.stop().unwrap();
        source.stop().unwrap();
        tone.stop().unwrap();
        // No playback survives unloading; monitor capture is pinned no_move and must invalidate.
        let removed = b.sink().clone();
        b.unload().unwrap();
        let mut removed_event = false;
        wait_until(|| {
            while let Some(event) = backend.poll_event() {
                if matches!(&event,Event::Device(DeviceEvent::Removed(id)|DeviceEvent::Invalidated(id)) if *id==removed)
                {
                    removed_event = true;
                }
            }
            removed_event && probe.snapshot().status == StreamStatus::DeviceInvalidated
        });
        let _ = probe.stop();
        a.unload().unwrap();
        wait_until(|| backend.defaults().unwrap() == defaults);
        drop(source);
        drop(listener);
        drop(probe);
        drop(direct);
        drop(tone);
        drop(a);
        drop(b);
        drop(backend);
        assert!(remaining_test_modules().unwrap().is_empty());
        eprintln!("cleanup verified by list_modules: no bifrost_test modules remain");
    }
}
