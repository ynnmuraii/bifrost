//! Explicit native diagnostic. --timing initializes without playback; --stress-period is audible.
#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use audio_backend::{Backend, Stream};
    use platform_audio_win::stream::*;
    use std::{thread, time::Duration};
    if std::env::args().nth(1).as_deref() == Some("--timing") {
        return timing();
    }
    if std::env::args().any(|arg| arg == "--stress-period") {
        return stress_period();
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
    use platform_audio_win::render_config::{
        initialize_legacy_render, initialize_shared_render, SAFE_PERIOD_FRAMES,
    };
    let args: Vec<_> = std::env::args().collect();
    let candidate = args.iter().position(|arg| arg == "--period");
    let floor = match candidate
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
    {
        None if candidate.is_none() => SAFE_PERIOD_FRAMES,
        Some("min") => 0,
        Some("5ms") => SAFE_PERIOD_FRAMES,
        Some("10ms") => 480,
        _ => return Err("--period requires min, 5ms or 10ms".into()),
    };
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
    {
        let (raw, config) = initialize_shared_render(&device, floor)?;
        println!(
            "TIMING negotiated requested_floor_frames={floor} config={config:?} stream_latency_hns={}",
            unsafe { raw.GetStreamLatency()? },
        );
    }
    {
        let (raw, config) = initialize_legacy_render(&device, None)?;
        println!(
            "TIMING legacy config={config:?} stream_latency_hns={}",
            unsafe { raw.GetStreamLatency()? },
        );
    }
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

#[cfg(windows)]
fn stress_period() -> Result<(), Box<dyn std::error::Error>> {
    use platform_audio_win::{render_config::SAFE_PERIOD_FRAMES, stream::*};
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let args: Vec<_> = std::env::args().collect();
    let candidate = args
        .iter()
        .position(|arg| arg == "--stress-period")
        .ok_or("missing --stress-period")?;
    let floor = match args.get(candidate + 1).map(String::as_str) {
        Some("min") => 0,
        Some("5ms") => SAFE_PERIOD_FRAMES,
        Some("10ms") => 480,
        _ => return Err("--stress-period requires min, 5ms or 10ms".into()),
    };
    let duration_arg = args.iter().position(|arg| arg == "--seconds");
    let seconds = match duration_arg {
        Some(index) => args
            .get(index + 1)
            .ok_or("--seconds requires a value")?
            .parse::<u64>()?,
        None => 60,
    };
    if !(1..=600).contains(&seconds) {
        return Err("--seconds must be in 1..=600".into());
    }
    // A bounded producer ring is continuously replenished, not a finite prerecorded tone.
    let (mut tx, rx) = rtrb::RingBuffer::new(SAMPLE_RATE as usize / 2);
    let mut phase = 0_u32;
    let gain = 10_f32.powf(-18.0 / 20.0);
    let mut replenish = || {
        for _ in 0..tx.slots() {
            let sample = (std::f32::consts::TAU * phase as f32 / 48.0).sin() * gain;
            if tx.push([sample; 2]).is_err() {
                break;
            }
            phase = (phase + 1) % 48;
        }
    };
    replenish();
    println!("Audible 1 kHz candidate stress at -18 dBFS: requested_floor_frames={floor}, warmup=2s, measured={seconds}s; no endpoint settings changed.");
    let mut render = start_render_for_probe(EndpointSelection::FollowDefaultMultimedia, rx, floor)?;
    let config = render
        .snapshot()
        .render_configuration
        .ok_or("missing render configuration")?;
    let starvation_observable = config.queue_limit_frames >= config.period_frames.saturating_mul(2);
    println!("STRESS negotiated {config:?} engine_starvation_heuristic_available={starvation_observable}");
    let warmup = Instant::now();
    while warmup.elapsed() < Duration::from_secs(2) {
        replenish();
        thread::sleep(Duration::from_millis(1));
    }
    let baseline = render.snapshot();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(seconds) {
        replenish();
        thread::sleep(Duration::from_millis(1));
    }
    let measured_seconds = started.elapsed().as_secs_f64();
    let final_snapshot = render.snapshot();
    render.stop();
    let underruns = final_snapshot
        .underrun_frames
        .saturating_sub(baseline.underrun_frames);
    let padding_zero = final_snapshot
        .render_padding_zero_events
        .saturating_sub(baseline.render_padding_zero_events);
    let starvation = final_snapshot
        .engine_starvation_events
        .saturating_sub(baseline.engine_starvation_events);
    println!("STRESS warmup={baseline:?}");
    println!("STRESS final={final_snapshot:?}");
    println!("STRESS measured_seconds={measured_seconds:.3} transferred_frames={} source_underrun_frames={underruns} raw_padding_zero_events={padding_zero} engine_starvation_events={starvation} stopped_status={:?}", final_snapshot.transferred_frames.saturating_sub(baseline.transferred_frames), render.snapshot().status);
    if underruns != 0 || starvation != 0 || final_snapshot.status != StreamStatus::Running {
        return Err("candidate stress failed: post-warmup source underrun, two-period queue starvation or stopped stream".into());
    }
    println!("PASS candidate stress: zero post-warmup source underruns and observed two-period engine starvation events; raw padding-zero is diagnostic only");
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("hw_probe requires Windows");
}
