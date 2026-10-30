use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy)]
struct Profile {
    pause_pct: u64,
    delay_ms: u64,
    jitter_ms: u64,
    seed: u64,
}

#[derive(Default)]
struct Oracle {
    lease: Option<[u8; 16]>,
    position: Option<InputEvent>,
    clicks: VecDeque<InputEvent>,
    pressed: bool,
    frames: u64,
    acked: u64,
    next_seq: u64,
    last_apply: Option<tokio::time::Instant>,
    gaps: crate::input_metrics::DurationHistogram,
    gap_stalls: u64,
    gap_stall_us: u64,
}

struct ImpairedWriter {
    inner: ScriptedWriter,
    profile: Profile,
    frame_started: bool,
    drained: usize,
    oracle: Arc<Mutex<Oracle>>,
    tracker: Arc<Mutex<AckTracker>>,
    metrics: Arc<Mutex<InputMetrics>>,
    applied: Arc<tokio::sync::Notify>,
    enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl AsyncWrite for ImpairedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if !self.frame_started && self.enabled.load(Ordering::Relaxed) {
            self.frame_started = true;
            self.profile.seed = self
                .profile
                .seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let random = self.profile.seed;
            let width = self.profile.jitter_ms * 2 + 1;
            let delay = self
                .profile
                .delay_ms
                .saturating_add(random % width)
                .saturating_sub(self.profile.jitter_ms);
            // A seeded extra byte-stream pause is NOT packet loss or a TCP retransmission model.
            let pause = if (random >> 32) % 100 < self.profile.pause_pct {
                200
            } else {
                0
            };
            self.inner.wait = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                delay + pause,
            ))));
        }
        Pin::new(&mut self.inner).poll_write(cx, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let frame = {
            let bytes = self.inner.bytes.lock().unwrap();
            decode_message::<wire::Envelope>(Channel::InputControl, &bytes[self.drained..])
                .unwrap()
                .unwrap()
        };
        let drained = self.inner.bytes.lock().unwrap().len();
        self.drained = drained;
        self.frame_started = false;
        let now = tokio::time::Instant::now();
        let mut oracle = self.oracle.lock().unwrap();
        oracle.next_seq += 1;
        assert!(frame.seq == oracle.next_seq, "frame sequence order");
        match &frame.payload {
            Some(wire::envelope::Payload::Hello(_)) => {
                assert!(frame.seq == 1, "Hello must be first")
            }
            Some(wire::envelope::Payload::EnterPrepare(_)) => {
                oracle.lease = Some(frame.lease_id.as_slice().try_into().unwrap())
            }
            Some(wire::envelope::Payload::Leave(_)) => {
                oracle.lease = None;
                assert!(!oracle.pressed);
            }
            Some(wire::envelope::Payload::InputBatch(_)) => {
                let fr::Message::InputBatch {
                    lease_id, events, ..
                } = convert::from_envelope(&frame).unwrap()
                else {
                    unreachable!()
                };
                assert!(oracle.lease == Some(lease_id), "cross-lease input");
                for event in events {
                    match event {
                        InputEvent::PointerAbsolute(_) => oracle.position = Some(event),
                        InputEvent::Button {
                            state: KeyState::Down,
                            ..
                        } => {
                            assert!(!oracle.pressed);
                            let expected = oracle.clicks.pop_front().expect("unexpected click");
                            assert!(oracle.position == Some(expected), "click position mismatch");
                            oracle.pressed = true;
                        }
                        InputEvent::Button {
                            state: KeyState::Up,
                            ..
                        } => {
                            assert!(oracle.pressed);
                            oracle.pressed = false;
                        }
                        _ => panic!("trace contains only synthetic absolute positions and buttons"),
                    }
                }
                if let Some(previous) = oracle.last_apply {
                    let gap = duration_us(now.duration_since(previous));
                    oracle.gaps.record(Some(gap));
                    if gap > 50_000 {
                        oracle.gap_stalls += 1;
                        oracle.gap_stall_us += gap - 50_000;
                    }
                }
                oracle.last_apply = Some(now);
                oracle.frames += 1;
                // The dry sink acknowledges only after every event was actually applied above.
                let timing = self
                    .tracker
                    .lock()
                    .unwrap()
                    .acknowledge(0, lease_id, frame.seq, now)
                    .unwrap();
                oracle.acked += 1;
                let mut metrics = self.metrics.lock().unwrap();
                metrics.increment(Counter::Acked, 1);
                metrics.record(Stage::InjectAckRtt, timing.0);
                if let Some(age) = timing.1 {
                    metrics.record(Stage::CaptureToAck, age);
                }
            }
            _ => {}
        }
        drop(oracle);
        self.applied.notify_one();
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Profiles are byte-stream pauses with deterministic delay/jitter, never a packet-loss claim.
#[tokio::test(start_paused = true)]
async fn seeded_impairment_profiles_preserve_final_point_edges_and_bounded_storage() {
    for pause_pct in [0, 1, 5] {
        for (delay_ms, jitter_ms) in [(0, 0), (10, 5), (30, 10), (75, 25)] {
            for seed in [17, 23, 41] {
                let profile = Profile {
                    pause_pct,
                    delay_ms,
                    jitter_ms,
                    seed,
                };
                let queue = Arc::new(queue(8));
                let (terminal, _) = tokio::sync::watch::channel(None);
                let metrics = Arc::new(Mutex::new(InputMetrics::default()));
                let tracker = Arc::new(Mutex::new(AckTracker::default()));
                let oracle = Arc::new(Mutex::new(Oracle::default()));
                let applied = Arc::new(tokio::sync::Notify::new());
                let enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let writer = ImpairedWriter {
                    inner: ScriptedWriter::new(128),
                    profile,
                    frame_started: false,
                    drained: 0,
                    oracle: Arc::clone(&oracle),
                    tracker: Arc::clone(&tracker),
                    metrics: Arc::clone(&metrics),
                    applied: Arc::clone(&applied),
                    enabled: Arc::clone(&enabled),
                };
                let handle = LinkHandle {
                    peer: NodeId::new([2; 32]),
                    queue: Arc::clone(&queue),
                    terminal: terminal.clone(),
                    local: NodeId::new([1; 32]),
                    dropped_motion: Arc::new(AtomicU64::new(0)),
                    lease_diagnostics: Arc::new(LeaseTransportCounters::default()),
                    metrics: Arc::clone(&metrics),
                    ack_tracker: Arc::clone(&tracker),
                    rtt: Arc::new(Mutex::new(RttMeter::default())),
                    shutdown: Arc::new(tokio::sync::Notify::new()),
                };
                let hello = wire::Envelope {
                    connection_id: vec![1; 16],
                    payload: Some(wire::envelope::Payload::Hello(Default::default())),
                    ..Default::default()
                };
                queue.hello(hello);
                queue.push(prepare(LEASE)).unwrap();
                let task = tokio::spawn(writer_loop(
                    writer,
                    Arc::clone(&queue),
                    terminal.clone(),
                    writer_state(),
                    Arc::clone(&metrics),
                    tracker,
                    wire::Hello::default(),
                ));
                let start = tokio::time::Instant::now();
                let mut trace_seed = seed;
                let mut sample_count = 0;
                let mut last = motion(0.0);
                for tick in 0..2_000 {
                    let rate = if (500..750).contains(&tick) { 8 } else { 1 };
                    for _ in 0..rate {
                        trace_seed = trace_seed
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        last = motion((trace_seed % 10_000) as f64);
                        let now = duration_us(start.elapsed()) + 1;
                        handle
                            .send_timed_input(LEASE, vec![(last, Some(now))], Some(now))
                            .await
                            .unwrap();
                        sample_count += 1;
                    }
                    if tick % 250 == 0 {
                        oracle.lock().unwrap().clicks.push_back(last);
                        let now = duration_us(start.elapsed()) + 1;
                        handle
                            .send_timed_input(
                                LEASE,
                                vec![
                                    (button(KeyState::Down), Some(now)),
                                    (button(KeyState::Up), Some(now)),
                                ],
                                Some(now),
                            )
                            .await
                            .unwrap();
                    }
                    assert!(queue.depth() <= 8);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let notified = applied.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        {
                            let oracle = oracle.lock().unwrap();
                            if oracle.position == Some(last)
                                && oracle.clicks.is_empty()
                                && !oracle.pressed
                            {
                                break;
                            }
                        }
                        notified.await;
                    }
                })
                .await
                .expect("final resting point must drain without new capture");
                enabled.store(false, Ordering::Relaxed);
                terminal.send_replace(Some(LinkDownReason::LocalClosed));
                tokio::time::timeout(GRACEFUL_CLOSE_BUDGET, task)
                    .await
                    .unwrap()
                    .unwrap();
                let snapshot = handle.input_metrics();
                let age = snapshot.stage(Stage::CaptureToWrite).1;
                let wait = snapshot.stage(Stage::EnqueueToWrite).1;
                let write = snapshot.stage(Stage::TlsWrite).1;
                let ack = snapshot.stage(Stage::InjectAckRtt).1;
                let capture_ack = snapshot.stage(Stage::CaptureToAck).1;
                let oracle = oracle.lock().unwrap();
                assert_eq!(oracle.frames, oracle.acked);
                assert_eq!(snapshot.counter(Counter::Failed).1, 0);
                println!("{{\"bench\":\"A-scripted-byte-pauses\",\"pause_pct\":{pause_pct},\"delay_ms\":{delay_ms},\"jitter_ms\":{jitter_ms},\"seed\":{seed},\"sample_count\":{sample_count},\"coalesced_count\":{},\"sent\":{},\"acked\":{},\"failed\":0,\"final_position_ok\":true,\"click_position_failures\":0,\"discrete_order_failures\":0,\"capture_to_write_p95_us\":{},\"capture_to_write_p99_us\":{},\"enqueue_to_write_p99_us\":{},\"write_p99_us\":{},\"ack_rtt_p99_us\":{},\"capture_to_ack_p99_us\":{},\"gap_stalls\":{},\"gap_stall_total_us\":{},\"queue_high_water\":{}}}", snapshot.counter(Counter::Coalesced).1, oracle.frames, oracle.acked, age.p95_us.unwrap(), age.p99_us.unwrap(), wait.p99_us.unwrap(), write.p99_us.unwrap(), ack.p99_us.unwrap(), capture_ack.p99_us.unwrap(), oracle.gap_stalls, oracle.gap_stall_us, snapshot.counter(Counter::QueueHighWater).1);
            }
        }
    }
}
