//! Explicitly audible, isolated Windows worker/IPC/encrypted-UDP smoke.
//! Run: cargo run -p bifrost-audio --example windows_e2e -- --audible
//! The coordinator plays the helper tone OUTSIDE its child's excluded process tree.
//! The child owns both real workers, so listener output cannot feed its source.
#[cfg(windows)]
mod windows {
    use audio_model::packet::{PacketKind, MAX_DATAGRAM_LEN};
    use ipc::{
        audio_worker::{AudioLink, LaunchToken},
        peer::SameUser,
        transport::{self, Endpoint},
    };
    use platform_audio_win::stream::*;
    use protocol::audio_worker::{self as w, audio_frame::Msg};
    use ring::rand::{SecureRandom, SystemRandom};
    use std::{
        collections::VecDeque,
        error::Error,
        io::{BufRead, BufReader, Write},
        net::UdpSocket,
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        thread,
        time::{Duration, Instant},
    };

    type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
    const DELAY: Duration = Duration::from_millis(500);
    const SESSION: [u8; 16] = [13; 16];

    fn qpc_100ns() -> Result<u64> {
        let (mut ticks, mut frequency) = (0, 0);
        // Same clock and units as WASAPI CapturedFrame::qpc_100ns, across processes.
        unsafe {
            if windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut ticks) == 0
                || windows_sys::Win32::System::Performance::QueryPerformanceFrequency(
                    &mut frequency,
                ) == 0
            {
                return Err("QPC unavailable".into());
            }
        }
        Ok((ticks as u128 * 10_000_000 / frequency as u128) as u64)
    }

    struct HelperChild(Child);
    impl Drop for HelperChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    pub fn main() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        let mut seconds = 6u64;
        let (mut audible, mut child_mode) = (false, false);
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--audible" => audible = true,
                "--harness" => child_mode = true,
                "--seconds" => {
                    seconds = args.next().ok_or("--seconds requires N")?.parse()?;
                    if seconds < 6 {
                        return Err("--seconds must be at least 6 (use 60 for a long run)".into());
                    }
                }
                _ => return Err("Usage: --audible [--seconds N]".into()),
            }
        }
        let duration = Duration::from_secs(seconds);
        let deadline = duration
            .checked_add(Duration::from_secs(12))
            .ok_or("--seconds exceeds the diagnostic deadline range")?;
        if !audible {
            return Err("Explicit consent required: --audible [--seconds N] (default 6)".into());
        }
        if child_mode {
            return tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(harness(deadline));
        }
        println!("Audible Windows E2E: no endpoint, volume, mute, input or installed configuration changes.");
        let mut child = HelperChild(
            Command::new(std::env::current_exe()?)
                .args(["--audible", "--harness", "--seconds", &seconds.to_string()])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        );
        let mut output = BufReader::new(child.0.stdout.take().ok_or("missing child stdout")?);
        loop {
            let mut line = String::new();
            if output.read_line(&mut line)? == 0 {
                return Err("harness exited before readiness".into());
            }
            print!("{line}");
            if line.trim() == "READY helper" {
                break;
            }
        }
        // Drain throughout playback: a long run must not block the child's IPC loop
        // on a full stdout pipe.
        let output = thread::spawn(move || -> std::io::Result<()> {
            for line in output.lines() {
                println!("{}", line?);
            }
            Ok(())
        });
        // Parent is not in the harness child's process tree. Spawning a tone child from
        // the harness itself would silently exclude the intended source signal too.
        // One second of bounded headroom, continuously replenished every 2ms.
        let (mut tx, rx) = rtrb::RingBuffer::new(48_000);
        let mut sample = 0u64;
        let mut fill = |tx: &mut rtrb::Producer<[f32; 2]>| {
            while !tx.is_full() {
                let phase = (sample % 48) as f64 / 48.0;
                let x = (std::f64::consts::TAU * phase).sin() as f32 * 0.1;
                if tx.push([x; 2]).is_err() {
                    break;
                }
                sample += 1;
            }
        };
        fill(&mut tx);
        let mut tone = start_render(EndpointSelection::FollowDefaultMultimedia, rx)?;
        let begun = Instant::now();
        while begun.elapsed() < duration {
            fill(&mut tx);
            thread::sleep(Duration::from_millis(2));
        }
        tone.stop();
        let stopped = qpc_100ns()?;
        let helper_health = tone.snapshot();
        println!("HELPER stopped qpc_100ns={stopped} seconds={seconds} render={helper_health:?}");
        writeln!(
            child.0.stdin.as_mut().ok_or("missing child stdin")?,
            "{stopped}"
        )?;
        output
            .join()
            .map_err(|_| "harness stdout reader panicked")??;
        if helper_health.underrun_frames != 0 {
            return Err("helper streaming ring underrun invalidates tone continuity".into());
        }
        if !child.0.wait()?.success() {
            return Err("Windows E2E child failed".into());
        }
        Ok(())
    }

    struct Worker {
        core: AudioLink<transport::ClientStream>,
        task: tokio::task::JoinHandle<
            std::result::Result<bifrost_audio::ExitReason, bifrost_audio::RoleError>,
        >,
        observed_capture: bool,
        sent: u64,
        received: u64,
        counters: w::AudioHealth,
    }
    impl Drop for Worker {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn worker(label: &str) -> Result<Worker> {
        let endpoint = Endpoint::pipe(format!(
            r"\\.\pipe\bifrost-audio-e2e-{}-{label}",
            std::process::id()
        ));
        let mut acceptor = transport::listen(&endpoint)?;
        let mut token = [0; 32];
        SystemRandom::new()
            .fill(&mut token)
            .map_err(|_| "random token failed")?;
        let task = tokio::spawn(async move {
            let io = transport::connect(&endpoint).await?;
            bifrost_audio::run_connection(io, LaunchToken::new(token.into())).await
        });
        let io = acceptor.accept(&SameUser::current()).await?;
        let mut core = AudioLink::core(io, 9)?;
        core.send(
            1,
            Msg::Attach(w::Attach {
                launch_token: token.to_vec(),
                backend: 1,
                core_silence_ms: 1000,
            }),
        )
        .await?;
        let mut result = Worker {
            core,
            task,
            observed_capture: false,
            sent: 0,
            received: 0,
            counters: w::AudioHealth::default(),
        };
        match tokio::time::timeout(Duration::from_secs(5), result.core.recv())
            .await??
            .frame
            .msg
        {
            Some(Msg::Ready(ready)) if ready.backend == 4 => Ok(result),
            _ => Err("Windows worker attach failed".into()),
        }
    }

    fn start(role: i32, local: u16, relay: u16, forward: &[u8; 32], reverse: &[u8; 32]) -> Msg {
        Msg::StartSession(w::StartSession {
            session_id: SESSION.to_vec(),
            role,
            profile: 1,
            capture: (role == 1).then_some(w::CaptureSpec {
                kind: 1,
                device: None,
            }),
            render: (role == 2).then_some(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(1)),
            }),
            local_bind_ip: "127.0.0.1".into(),
            local_udp_port: u32::from(local),
            peer_ip: "127.0.0.1".into(),
            peer_udp_port: u32::from(relay),
            forward: Some(w::DirectionMaterial {
                key: forward.to_vec(),
                nonce_prefix: vec![31; 4],
            }),
            reverse: Some(w::DirectionMaterial {
                key: reverse.to_vec(),
                nonce_prefix: vec![32; 4],
            }),
            local_scope_id: 0,
            peer_scope_id: 0,
            initial_authorization_ms: 1000,
            source_local_sound_released: false,
        })
    }

    fn observe(worker: &mut Worker, label: &str, msg: Option<Msg>) -> Result<()> {
        match msg {
            Some(Msg::Health(health)) => {
                for session in health.sessions {
                    worker.observed_capture |= session.capture_active == Some(true);
                    worker.sent = worker.sent.max(session.sent_media_packets);
                    if let Some(drift) = session.drift {
                        println!("DRIFT {label} ratio_ppm={} filtered_fill_error_frames={:.3} coarse_corrections={} clock_source={}", drift.ratio_ppm, drift.filtered_fill_error_frames, drift.coarse_corrections, session.clock_source);
                    } else {
                        println!(
                            "DRIFT {label} receiver_PI=absent clock_source={}",
                            session.clock_source
                        );
                    }
                    if let Some(stage) = session.latency {
                        println!(
                            "STAGES {label} packetization_us={} capture_queue_us={} codec_lookahead_us={} jitter_target_us={} jitter_buffer_us={} decoded_queue_us={} render_queue_us={} resampler_delay_us={} native_render_buffer_us={:?} native_discontinuity_flags={}; jitter occupancy overlaps target",
                            stage.packetization_us, stage.capture_queue_us,
                            stage.codec_lookahead_us, stage.jitter_target_us,
                            stage.jitter_buffer_us, stage.decoded_queue_us,
                            stage.render_queue_us, stage.resampler_delay_us,
                            stage.native_render_buffer_us, session.native_discontinuity_flags,
                        );
                    }
                    if let Some(counters) = session.counters {
                        println!("HEALTH {label} sent={} received={} delta_received={} loss={} delta_loss={} underruns={} delta_underruns={} late={} concealed={} discontinuities={}",
                            session.sent_media_packets, counters.received_packets,
                            counters.received_packets.saturating_sub(worker.received),
                            counters.lost_frames, counters.lost_frames.saturating_sub(worker.counters.lost_frames),
                            counters.underruns, counters.underruns.saturating_sub(worker.counters.underruns),
                            counters.late_frames, counters.concealed_frames, counters.discontinuities);
                        worker.received = counters.received_packets;
                        worker.counters = counters;
                    }
                }
            }
            Some(Msg::Error(error)) => {
                return Err(format!("{label} worker error code={}", error.code).into())
            }
            Some(Msg::SessionEvent(event)) if event.reason != 0 => {
                return Err(format!("{label} unexpected stop reason={}", event.reason).into());
            }
            _ => {}
        }
        Ok(())
    }
    // Retain five seconds of onset and five of tail, independent of run length.
    // Clock statistics include every delivered frame, including discarded PCM.
    #[derive(Default)]
    struct CaptureHistory {
        beginning: Vec<CapturedFrame>,
        tail: VecDeque<CapturedFrame>,
        delivered: u64,
        first_clock: Option<(u64, u64, u64)>,
        last_clock: Option<(u64, u64, u64)>,
    }
    impl CaptureHistory {
        fn push(&mut self, frame: CapturedFrame) {
            if frame.timestamp_valid {
                let clock = (self.delivered, frame.qpc_100ns, frame.device_position);
                self.first_clock.get_or_insert(clock);
                self.last_clock = Some(clock);
            }
            self.delivered += 1;
            if self.beginning.len() < 48_000 * 5 {
                self.beginning.push(frame);
            } else {
                if self.tail.len() == 48_000 * 5 {
                    self.tail.pop_front();
                }
                self.tail.push_back(frame);
            }
        }

        fn finish(self, label: &str) -> Result<Vec<CapturedFrame>> {
            let first = self.first_clock.ok_or("no valid capture clock")?;
            let last = self.last_clock.ok_or("no valid capture clock")?;
            let span = last
                .1
                .checked_sub(first.1)
                .filter(|n| *n != 0)
                .ok_or("empty or regressed capture clock span")?;
            let ppm = ((last.0 - first.0) as f64 / (span as f64 * 48_000.0 / 10_000_000.0) - 1.0)
                * 1_000_000.0;
            println!("CLOCK {label} delivered_sample_vs_QPC_drift_ppm={ppm:.3} duration_ms={:.1} native_position_first={} native_position_last={} delivered_frames={}; independent capture estimate, NOT the worker receiver PI ratio", span as f64 / 10_000.0, first.2, last.2, self.delivered);
            let mut frames = self.beginning;
            frames.extend(self.tail);
            Ok(frames)
        }
    }

    #[derive(Default)]
    struct Measurement {
        frames: usize,
        rms: f64,
        tone_power: f64,
    }
    fn measure(frames: &[CapturedFrame], from: u64, to: u64) -> Measurement {
        let mut result = Measurement::default();
        let (mut energy, mut real, mut imaginary) = (0.0, 0.0, 0.0);
        for frame in frames
            .iter()
            .filter(|f| f.timestamp_valid && f.qpc_100ns >= from && f.qpc_100ns < to)
        {
            let x = f64::from(frame.samples[0]);
            let angle = std::f64::consts::TAU * 1000.0 * result.frames as f64 / 48_000.0;
            energy += x * x;
            real += x * angle.cos();
            imaginary += x * angle.sin();
            result.frames += 1;
        }
        if result.frames > 0 {
            result.rms = (energy / result.frames as f64).sqrt();
            result.tone_power =
                2.0 * real.hypot(imaginary).powi(2) / (result.frames as f64).powi(2);
        }
        result
    }
    fn onset(frames: &[CapturedFrame], after: u64) -> Result<u64> {
        for window in frames.as_chunks::<480>().0 {
            let first = window[0];
            if first.timestamp_valid
                && first.qpc_100ns >= after
                && measure(window, 0, u64::MAX).tone_power > 0.00005
            {
                return Ok(first.qpc_100ns);
            }
        }
        Err("no timestamped 1 kHz onset (endpoint mute/loopback suppression may prevent measurement)".into())
    }
    fn report(label: &str, frames: &[CapturedFrame], from: u64, to: u64) -> Measurement {
        let m = measure(frames, from, to);
        println!(
            "PCM {label} frames={} rms={:.7} tone_power={:.9}",
            m.frames, m.rms, m.tone_power
        );
        m
    }

    async fn harness(deadline: Duration) -> Result<()> {
        let mut source = worker("source").await?;
        let mut listener = worker("listener").await?;
        let reserve_source = UdpSocket::bind("127.0.0.1:0")?;
        let reserve_listener = UdpSocket::bind("127.0.0.1:0")?;
        let source_addr = reserve_source.local_addr()?;
        let listener_addr = reserve_listener.local_addr()?;
        let relay = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let relay_port = relay.local_addr()?.port();
        let (mut forward, mut reverse) = ([0; 32], [0; 32]);
        let random = SystemRandom::new();
        random.fill(&mut forward).map_err(|_| "random key failed")?;
        random.fill(&mut reverse).map_err(|_| "random key failed")?;
        drop((reserve_source, reserve_listener));
        listener
            .core
            .send(
                2,
                start(2, listener_addr.port(), relay_port, &forward, &reverse),
            )
            .await?;
        source
            .core
            .send(
                2,
                start(1, source_addr.port(), relay_port, &forward, &reverse),
            )
            .await?;
        let (endpoint_tx, mut endpoint_rx) = rtrb::RingBuffer::new(48_000);
        let (include_tx, mut include_rx) = rtrb::RingBuffer::new(48_000);
        let mut endpoint = start_endpoint_loopback_for_probe(endpoint_tx)?;
        let mut include = start_capture_including_for_probe(std::process::id(), include_tx)?;
        let helper_stopped = Arc::new(AtomicU64::new(0));
        let stopped = helper_stopped.clone();
        thread::spawn(move || {
            let mut line = String::new();
            let value = if std::io::stdin().read_line(&mut line).is_ok() {
                line.trim().parse().unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            stopped.store(value, Ordering::Release);
        });
        let (mut endpoint_pcm, mut include_pcm) =
            (CaptureHistory::default(), CaptureHistory::default());
        let mut pending = VecDeque::<(Instant, Vec<u8>)>::new();
        let mut packet = [0; MAX_DATAGRAM_LEN + 1];
        let mut tick = tokio::time::interval(Duration::from_millis(2));
        let begun = Instant::now();
        let mut serial = 0;
        let mut announced = false;
        let mut signal_after = 0;
        let mut steady_baseline = None;
        let mut steady_last = None;
        let mut steady_samples = 0u64;
        loop {
            tokio::select! {
                frame = source.core.recv() => observe(&mut source, "source", frame?.frame.msg)?,
                frame = listener.core.recv() => {
                    let msg = frame?.frame.msg;
                    let health = matches!(&msg, Some(Msg::Health(_)));
                    observe(&mut listener, "listener", msg)?;
                    // Exclude startup (including the deliberately delayed first packet)
                    // and assess only health received during sustained helper playback.
                    if health && announced && qpc_100ns()? >= signal_after + 20_000_000
                        && helper_stopped.load(Ordering::Acquire) == 0
                    {
                        steady_baseline.get_or_insert(listener.counters.underruns);
                        steady_last = Some(listener.counters.underruns);
                        steady_samples += 1;
                    }
                },
                incoming = relay.recv_from(&mut packet) => {
                    let (n, peer) = incoming?;
                    if peer == source_addr {
                        if audio_model::validate_datagram(&packet[..n]).is_ok_and(|h| h.kind == PacketKind::Opus) {
                            if pending.len() >= 256 { return Err("relay queue exceeded bound".into()); }
                            pending.push_back((Instant::now() + DELAY, packet[..n].to_vec()));
                        } else { relay.send_to(&packet[..n], listener_addr).await?; }
                    } else if peer == listener_addr { relay.send_to(&packet[..n], source_addr).await?; }
                }
                _ = tick.tick() => {
                    while let Some((due, _)) = pending.front() {
                        if *due > Instant::now() { break; }
                        let (_, bytes) = pending.pop_front().ok_or("relay queue empty")?;
                        relay.send_to(&bytes, listener_addr).await?;
                    }
                    while let Ok(frame) = endpoint_rx.pop() { endpoint_pcm.push(frame); }
                    while let Ok(frame) = include_rx.pop() { include_pcm.push(frame); }
                    serial += 1;
                    if serial % 50 == 0 {
                        source.core.send(0, Msg::Ping(w::Ping { serial })).await?;
                        listener.core.send(0, Msg::Ping(w::Ping { serial })).await?;
                        source.core.send(serial + 3, Msg::RenewSession(w::RenewSession {
                            session_id: SESSION.to_vec(), ttl_ms: 1000,
                        })).await?;
                        listener.core.send(serial + 3, Msg::RenewSession(w::RenewSession {
                            session_id: SESSION.to_vec(), ttl_ms: 1000,
                        })).await?;
                    }
                    if !announced && begun.elapsed() > Duration::from_secs(2) {
                        signal_after = qpc_100ns()?;
                        println!("READY helper");
                        std::io::stdout().flush()?;
                        announced = true;
                    }
                    let stop = helper_stopped.load(Ordering::Acquire);
                    if stop == u64::MAX { return Err("helper stop timestamp missing".into()); }
                    if stop != 0 && qpc_100ns()? > stop + 20_000_000 { break; }
                    if begun.elapsed() > deadline { return Err("helper timeout".into()); }
                }
            }
        }
        endpoint.stop();
        include.stop();
        while let Ok(frame) = endpoint_rx.pop() {
            endpoint_pcm.push(frame);
        }
        while let Ok(frame) = include_rx.pop() {
            include_pcm.push(frame);
        }
        let endpoint_pcm = endpoint_pcm.finish("endpoint")?;
        let include_pcm = include_pcm.finish("listener_include")?;
        println!(
            "FINAL_HEALTH source sent={} received={} counters={:?}",
            source.sent, source.received, source.counters
        );
        println!(
            "FINAL_HEALTH listener sent={} received={} counters={:?}",
            listener.sent, listener.received, listener.counters
        );
        let steady_delta = steady_last
            .zip(steady_baseline)
            .map(|(last, first)| last.saturating_sub(first));
        println!("STEADY listener warmup_after_helper_ms=2000 samples={steady_samples} baseline_underruns={steady_baseline:?} last_underruns={steady_last:?} delta_underruns={steady_delta:?}");
        // Stop workers explicitly; dropping also revokes authority on every error path.
        source
            .core
            .send(serial + 4, Msg::Shutdown(w::Shutdown {}))
            .await?;
        listener
            .core
            .send(serial + 4, Msg::Shutdown(w::Shutdown {}))
            .await?;
        for worker in [&mut source, &mut listener] {
            let reason = tokio::time::timeout(Duration::from_secs(3), &mut worker.task).await???;
            if reason != bifrost_audio::ExitReason::Shutdown {
                return Err("worker did not shut down cleanly".into());
            }
        }
        let stop = helper_stopped.load(Ordering::Acquire);
        let direct_onset = onset(&endpoint_pcm, signal_after)?;
        let listener_onset = onset(&include_pcm, signal_after)?;
        let latency = listener_onset
            .checked_sub(direct_onset)
            .ok_or("listener onset precedes helper")? as f64
            / 10_000.0;
        println!("LATENCY common_WASAPI_QPC raw_relay_E2E_ms={latency:.2} injected_delay_ms=500 delay_subtracted_estimate_ms={:.2} (NOT native direct E2E latency; 10ms onset windows)", latency - 500.0);
        let baseline = report(
            "endpoint_before_helper",
            &endpoint_pcm,
            signal_after.saturating_sub(5_000_000),
            signal_after,
        );
        let include_baseline = report(
            "listener_before_helper",
            &include_pcm,
            signal_after.saturating_sub(5_000_000),
            signal_after,
        );
        // At least 100ms after helper Stop returned: no direct helper tone can be
        // attributed to listener output. The 500ms ciphertext delay leaves a long tail.
        let tail = report(
            "ENDPOINT_network_only_tail",
            &endpoint_pcm,
            stop + 1_000_000,
            stop + 3_000_000,
        );
        let control = report(
            "INCLUDE_listener_network_tail",
            &include_pcm,
            stop + 1_000_000,
            stop + 3_000_000,
        );
        let settled = report(
            "listener_settled_no_feedback",
            &include_pcm,
            stop + 12_000_000,
            stop + 17_000_000,
        );
        println!(
            "PROBE endpoint={:?} include={:?}",
            endpoint.snapshot(),
            include.snapshot()
        );
        if steady_samples < 2 || steady_delta != Some(0) {
            return Err(
                "listener steady-state Health must prove zero underrun delta after warmup".into(),
            );
        }
        if baseline.tone_power > 0.00001 || include_baseline.tone_power > 0.00001 {
            return Err("pre-existing 1kHz signal contaminates attribution".into());
        }
        if !source.observed_capture || source.sent < 100 || listener.received < 100 {
            return Err(
                "worker Health does not prove live capture/encrypted media delivery".into(),
            );
        }
        if tail.frames < 8000
            || control.frames < 8000
            || tail.tone_power < 0.00005
            || control.tone_power < 0.00005
        {
            return Err("FAIL: endpoint listener-only tail and process INCLUDE control must both contain 1kHz; muted endpoint/loopback suppression is NOT a pass".into());
        }
        if !(450.0..900.0).contains(&latency) || settled.tone_power > 0.00001 {
            return Err("latency outside bound or listener feedback persists".into());
        }
        if endpoint.snapshot().dropped_capture_frames != 0
            || include.snapshot().dropped_capture_frames != 0
        {
            return Err("probe ring overflow invalidates measurement".into());
        }
        println!("PASS real WindowsBackend workers + same-user named pipes + encrypted UDP + endpoint network-only tone; endpoint settings untouched");
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows::main()
}
#[cfg(not(windows))]
fn main() {
    eprintln!("windows_e2e requires Windows; no audio was played");
    std::process::exit(1);
}
