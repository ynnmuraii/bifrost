//! Explicit native diagnostic; --timing initializes streams without starting or playing them.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use audio_backend::{Backend, Stream};
    use platform_audio_win::stream::*;
    use std::{thread, time::Duration};
    if std::env::args().nth(1).as_deref() == Some("--timing") {
        return timing();
    }
    let mut backend = platform_audio_win::WindowsBackend::new()?;
    let fixed = audio_backend::AudioFormat {
        sample_rate: SAMPLE_RATE,
        channels: 2,
    };
    let (mut contract_tx, mut contract_rx) = rtrb::RingBuffer::new(48_000 * 6);
    let mut contract_capture = backend.start_capture(
        audio_backend::CaptureSpec::SystemExcludingOwnProcessTree { format: fixed },
        Box::new(move |block| {
            for (index, samples) in block.samples.as_chunks::<2>().0.iter().enumerate() {
                let _ = contract_tx.push(CapturedFrame {
                    samples: [samples[0], samples[1]],
                    device_position: block.device_position.unwrap_or(0) + index as u64,
                    qpc_100ns: block.timestamp_ns / 100
                        + index as u64 * 10_000_000 / u64::from(SAMPLE_RATE),
                    timestamp_valid: block.timestamp_valid,
                    discontinuity: index == 0 && block.discontinuity,
                });
            }
        }),
    )?;
    let mut contract_render = backend.start_render(
        audio_backend::RenderRequest {
            device: audio_backend::DeviceSelection::FollowDefault(
                audio_backend::DefaultRole::Multimedia,
            ),
            requested: fixed,
        },
        Box::new(|samples| samples.fill(0.0)),
    )?;
    println!("Audible 1 kHz probe: in-memory PCM only; no endpoint settings changed.");
    let (exclude_tx, mut exclude_rx) = rtrb::RingBuffer::new(48_000 * 6);
    let (include_tx, mut include_rx) = rtrb::RingBuffer::new(48_000 * 6);
    let mut exclude = start_capture_excluding(std::process::id(), exclude_tx)?;
    let mut include = start_capture_including_for_probe(std::process::id(), include_tx)?;
    thread::sleep(Duration::from_millis(300));
    let (mut tx, rx) = rtrb::RingBuffer::new(48_000 * 4);
    for i in 0..48_000 * 3 {
        let value = ((2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin()
            * 10_f64.powf(-12.0 / 20.0)) as f32;
        tx.push([value; 2]).unwrap();
    }
    let mut render = start_render(EndpointSelection::FollowDefaultMultimedia, rx)?;
    thread::sleep(Duration::from_millis(3200));
    render.stop();
    thread::sleep(Duration::from_millis(200));
    exclude.stop();
    include.stop();
    contract_capture.stop()?;
    contract_render.stop()?;
    let contract_stats = contract_capture.native_snapshot();
    let contract_clock = Stream::snapshot(&contract_render);
    fn measure(rx: &mut rtrb::Consumer<CapturedFrame>) -> (usize, f64, f64, usize) {
        let mut samples = Vec::new();
        let mut gaps = 0;
        while let Ok(frame) = rx.pop() {
            samples.push(frame.samples[0]);
            gaps += usize::from(frame.discontinuity);
        }
        let total = samples.len();
        let a = 36_000.min(total);
        let b = (a + 96_000).min(total);
        let samples = &samples[a..b];
        if samples.is_empty() {
            return (total, 0.0, 0.0, gaps);
        }
        let rms = (samples.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>()
            / samples.len() as f64)
            .sqrt();
        let coefficient = 2.0 * (2.0 * std::f64::consts::PI * 1000.0 / 48_000.0).cos();
        let (mut s1, mut s2) = (0.0, 0.0);
        for &x in samples {
            let s = f64::from(x) + coefficient * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = 2.0 * (s1 * s1 + s2 * s2 - coefficient * s1 * s2).max(0.0)
            / (samples.len() as f64).powi(2);
        (total, rms, power, gaps)
    }
    let contract = measure(&mut contract_rx);
    println!(
        "SHARED EXCLUDE frames={} rms={:.9} tone_power={:.9} timestamped_clock={:?}",
        contract.0, contract.1, contract.2, contract_clock
    );
    let ex = measure(&mut exclude_rx);
    let inc = measure(&mut include_rx);
    let render_stats = render.snapshot();
    println!(
        "EXCLUDE frames={} rms={:.9} tone_power={:.9} discontinuities={} dropped={}",
        ex.0,
        ex.1,
        ex.2,
        ex.3,
        exclude.snapshot().dropped_capture_frames
    );
    println!(
        "INCLUDE frames={} rms={:.9} tone_power={:.9} discontinuities={} dropped={}",
        inc.0,
        inc.1,
        inc.2,
        inc.3,
        include.snapshot().dropped_capture_frames
    );
    println!("RENDER frames={} underrun_frames={} clock_position={} clock_frequency={} qpc_100ns={} status={:?}", render_stats.transferred_frames, render_stats.underrun_frames, render_stats.device_position, render_stats.clock_frequency, render_stats.qpc_100ns, render_stats.status);
    if ex.0 >= 132_000
        && inc.0 >= 132_000
        && ex.2 < 0.000001
        && inc.2 > 0.01
        && exclude.snapshot().dropped_capture_frames == 0
        && include.snapshot().dropped_capture_frames == 0
        && render_stats.status == StreamStatus::Stopped
        && contract.0 >= 132_000
        && contract.2 < 0.000001
        && contract.0 as u64 == contract_stats.transferred_frames
        && contract_clock.clock_frequency.is_some()
        && contract_clock.host_time_100ns.is_some()
    {
        println!("PASS capture-exclude + positive INCLUDE control + event-driven render");
        Ok(())
    } else {
        Err("FAIL capture/render thresholds".into())
    }
}

#[cfg(windows)]
fn timing() -> Result<(), Box<dyn std::error::Error>> {
    use wasapi::{
        AudioClient, DeviceEnumerator, Direction, Role, SampleType, StreamMode, WaveFormat,
    };
    wasapi::initialize_mta().ok()?;
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            wasapi::deinitialize();
        }
    }
    let _com = Com;
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let device = DeviceEnumerator::new()?
        .get_default_device_for_role(&Direction::Render, &Role::Multimedia)?;
    let mut capture = AudioClient::new_application_loopback_client(std::process::id(), false)?;
    capture.initialize_client(
        &format,
        &Direction::Capture,
        &StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 0,
        },
    )?;
    let raw = capture.as_iaudioclient();
    println!(
        "TIMING process_capture buffer_frames={} period={:?} stream_latency_hns={:?}",
        unsafe { raw.GetBufferSize()? },
        capture.get_device_period(),
        unsafe { raw.GetStreamLatency() }
    );
    for duration in [0, 100_000] {
        let mut client = device.get_iaudioclient()?;
        let period = client.get_device_period()?;
        client.initialize_client(
            &format,
            &Direction::Render,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: duration,
            },
        )?;
        let raw = client.as_iaudioclient();
        println!("TIMING render requested_duration_hns={duration} buffer_frames={} default_period_hns={} minimum_period_hns={} stream_latency_hns={}", unsafe { raw.GetBufferSize()? }, period.0, period.1, unsafe { raw.GetStreamLatency()? });
    }
    println!("PASS timing-only: no stream started, no endpoint settings changed");
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("hw_probe requires Windows");
}
