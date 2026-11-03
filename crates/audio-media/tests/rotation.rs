//! Real UDP engines and owned Dry streams; no OS device or installed application is touched.
#![cfg(debug_assertions)]
use audio_backend::{
    dry::{DryBackend, DryStream, ScriptEvent},
    AudioFormat, Capabilities, CaptureSpec, Device, DeviceId, DeviceSelection, Direction,
    RenderRequest,
};
use audio_media::{
    crypto::DirectionMaterial,
    runtime::{
        start_listener, start_source, MediaEvent, MediaSession, SessionConfig, SessionMaterial,
    },
    socket::MediaSocket,
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
fn material(seed: u8) -> SessionMaterial {
    SessionMaterial {
        forward: DirectionMaterial::new(Zeroizing::new([seed; 32]), Zeroizing::new([seed + 1; 4])),
        reverse: DirectionMaterial::new(
            Zeroizing::new([seed + 2; 32]),
            Zeroizing::new([seed + 3; 4]),
        ),
    }
}
fn backend() -> (DryBackend, DeviceId) {
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        ..Capabilities::default()
    });
    let id = DeviceId::new("OWNED_RENDER").unwrap();
    backend
        .apply(ScriptEvent::UpsertDevice(Device {
            id: id.clone(),
            label: "Owned render".into(),
            direction: Direction::Render,
            format: format(),
        }))
        .unwrap();
    (backend, id)
}
fn format() -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}
fn pump(source: &mut MediaSession<DryStream>, listener: &mut MediaSession<DryStream>) {
    if let Some(stream) = source.stream_mut() {
        stream.pump(480).unwrap();
    }
    if let Some(stream) = listener.stream_mut() {
        stream.pump(480).unwrap();
    }
    thread::sleep(Duration::from_millis(10));
}
fn await_event(
    source: &mut MediaSession<DryStream>,
    listener: &mut MediaSession<DryStream>,
    on_source: bool,
    expected: MediaEvent,
) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        pump(source, listener);
        let session = if on_source {
            &mut *source
        } else {
            &mut *listener
        };
        while let Some(event) = session.poll_event() {
            if event == expected {
                return;
            }
            assert!(
                !matches!(
                    event,
                    MediaEvent::Stopped { .. } | MediaEvent::RotationRequired
                ),
                "unexpected {event:?}"
            );
        }
        assert!(Instant::now() < deadline, "missing {expected:?}");
    }
}
#[test]
fn handoff_keeps_native_stream_silence_and_override_and_off_fences_pending_keys() {
    let (mut source_backend, source_id) = backend();
    let (mut listener_backend, listener_id) = backend();
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let source_peer = b.local_addr().unwrap();
    let listener_peer = a.local_addr().unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let config = |socket, peer| SessionConfig {
        socket: MediaSocket::new(socket).unwrap(),
        peer,
        session_id: [55; 16],
        profile: Profile::Ethernet,
        authorized: allowed.clone(),
    };
    let mut listener = start_listener(
        &mut listener_backend,
        RenderRequest {
            device: DeviceSelection::Explicit(listener_id),
            requested: format(),
        },
        config(b, listener_peer),
        material(10),
    )
    .unwrap();
    let mut source = start_source(
        &mut source_backend,
        CaptureSpec::Monitor {
            sink: DeviceSelection::Explicit(source_id.clone()),
            format: format(),
        },
        config(a, source_peer),
        material(10),
    )
    .unwrap();
    await_event(&mut source, &mut listener, true, MediaEvent::Started);
    await_event(&mut source, &mut listener, false, MediaEvent::Started);
    let local_silence = Arc::new(AtomicBool::new(true));
    let permit = local_silence.clone();
    source
        .begin_source_silence(Arc::new(move || permit.load(Ordering::Acquire)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = source.poll_source_silence() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert_eq!(
        source_backend.render_mute_state(&source_id).unwrap(),
        (true, true, 1)
    );
    let source_stream = source.stream_mut().unwrap() as *mut DryStream;
    let listener_stream = listener.stream_mut().unwrap() as *mut DryStream;
    source.advance_test_clock(1_740_000);
    listener.advance_test_clock(1_740_000);
    await_event(
        &mut source,
        &mut listener,
        true,
        MediaEvent::RotationDue { epoch: 1 },
    );
    await_event(
        &mut source,
        &mut listener,
        false,
        MediaEvent::RotationDue { epoch: 1 },
    );
    listener.prepare_rotation(2, material(20)).unwrap();
    source.prepare_rotation(2, material(20)).unwrap();
    await_event(
        &mut source,
        &mut listener,
        false,
        MediaEvent::RotationPrepared { epoch: 2 },
    );
    await_event(
        &mut source,
        &mut listener,
        true,
        MediaEvent::RotationPrepared { epoch: 2 },
    );
    listener.activate_rotation(2).unwrap();
    await_event(
        &mut source,
        &mut listener,
        false,
        MediaEvent::RotationActivated { epoch: 2 },
    );
    source.activate_rotation(2).unwrap();
    await_event(
        &mut source,
        &mut listener,
        true,
        MediaEvent::RotationActivated { epoch: 2 },
    );
    let before = listener.snapshot().health.received_packets;
    source.advance_test_clock(1_800_000);
    listener.advance_test_clock(1_800_000);
    for _ in 0..25 {
        pump(&mut source, &mut listener);
    }
    assert!(listener.snapshot().health.received_packets > before + 10);
    assert_eq!(
        source.stream_mut().unwrap() as *mut DryStream,
        source_stream
    );
    assert_eq!(
        listener.stream_mut().unwrap() as *mut DryStream,
        listener_stream
    );
    assert_eq!(
        source_backend.render_mute_state(&source_id).unwrap(),
        (true, true, 1)
    );
    // Emergency/local return wins without stopping transmission; rotation must not reacquire silence.
    local_silence.store(false, Ordering::Release);
    pump(&mut source, &mut listener);
    assert!(!source_backend.render_mute_state(&source_id).unwrap().0);
    listener.prepare_rotation(3, material(30)).unwrap();
    source.prepare_rotation(3, material(30)).unwrap();
    await_event(
        &mut source,
        &mut listener,
        false,
        MediaEvent::RotationPrepared { epoch: 3 },
    );
    await_event(
        &mut source,
        &mut listener,
        true,
        MediaEvent::RotationPrepared { epoch: 3 },
    );
    allowed.store(false, Ordering::Release);
    assert!(source.activate_rotation(3).is_err());
    assert!(listener.activate_rotation(3).is_err());
    source.stop().unwrap();
    listener.stop().unwrap();
    assert!(!source_backend.render_mute_state(&source_id).unwrap().0);
}
