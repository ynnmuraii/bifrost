use super::*;
use input_model::{HidKey, KeyState, MouseButton};
use std::future::Future;

const LEASE: [u8; 16] = [7; 16];

fn motion(x: f64) -> InputEvent {
    InputEvent::PointerAbsolute(input_model::Position::new(x, 1.0))
}
fn button(state: KeyState) -> InputEvent {
    InputEvent::Button {
        button: MouseButton::Left,
        state,
    }
}
fn sample(event: InputEvent) -> InputSample {
    InputSample {
        event,
        capture_host_us: Some(10),
        capture_to_enqueue_us: Some(20),
        enqueued_at: tokio::time::Instant::now(),
    }
}
fn prepare(lease_id: [u8; 16]) -> Outbound {
    Outbound::Guaranteed(fr::Message::EnterPrepare {
        lease_id,
        topology_revision: 1,
        position: geometry::Position::new("display", 0.0, 0.0),
        owner_serial: 1,
    })
}
fn queue(capacity: usize) -> InputQueue {
    let queue = InputQueue::new(capacity);
    queue.ready(false);
    queue
}
fn writer_state() -> WriterState {
    WriterState {
        tx_seq: 1,
        connection_id: [1; 16],
        batch_display: "display".into(),
        lease_diagnostics: Arc::new(LeaseTransportCounters::default()),
        rtt: Arc::new(Mutex::new(RttMeter::default())),
    }
}

fn topology(revision: u64) -> Outbound {
    Outbound::Layout(convert::LayoutMessage::Topology(wire::Topology {
        revision,
        ..Default::default()
    }))
}

#[test]
fn admitted_topology_precedes_later_enter_fence_with_pending_motion() {
    let queue = queue(4);
    queue.input(LEASE, vec![sample(motion(1.0))]).unwrap();
    queue.push(topology(1)).unwrap();
    queue.push(prepare(LEASE)).unwrap();
    assert!(matches!(queue.pop(), Some(Intent::Input { .. })));
    assert!(matches!(queue.pop(),
        Some(Intent::Outbound(Outbound::Layout(convert::LayoutMessage::Topology(value)))) if value.revision == 1));
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare {
                topology_revision: 1,
                ..
            }
        )))
    ));
    assert!(queue.pop().is_none());
}

#[test]
fn later_topology_cannot_overtake_earlier_fence_at_fairness_limit() {
    let queue = queue(16);
    for _ in 0..8 {
        queue
            .input(LEASE, vec![sample(button(KeyState::Down))])
            .unwrap();
    }
    queue.push(prepare(LEASE)).unwrap();
    queue.push(topology(2)).unwrap();
    for _ in 0..8 {
        assert!(matches!(queue.pop(), Some(Intent::Input { .. })));
    }
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare { .. }
        )))
    ));
    assert!(matches!(queue.pop(),
        Some(Intent::Outbound(Outbound::Layout(convert::LayoutMessage::Topology(value)))) if value.revision == 2));
}

#[test]
fn continuous_latest_motion_serves_auxiliary_within_eight_input_frames() {
    let queue = queue(1);
    queue.push(topology(1)).unwrap();
    for index in 0..8 {
        queue
            .input(LEASE, vec![sample(motion(f64::from(index)))])
            .unwrap();
        assert!(matches!(queue.pop(), Some(Intent::Input { .. })));
    }
    queue.input(LEASE, vec![sample(motion(9.0))]).unwrap();
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Layout(
            convert::LayoutMessage::Topology(_)
        )))
    ));
    let Some(Intent::Input { samples, .. }) = queue.pop() else {
        panic!("latest motion was lost at the auxiliary fairness boundary");
    };
    assert_eq!(samples[0].event, motion(9.0));
}

#[tokio::test(start_paused = true)]
async fn writer_keeps_the_original_terminal_overflow_reason() {
    let queue = Arc::new(queue(1));
    let (terminal, _) = tokio::sync::watch::channel(None);
    let expected = LinkDownReason::Protocol("critical input queue overflow".into());
    set_terminal_once(&terminal, || expected.clone());
    set_terminal_once(&terminal, || {
        panic!("a second reason must not even be constructed")
    });
    writer_loop(
        ScriptedWriter::new(1),
        queue,
        terminal.clone(),
        writer_state(),
        Arc::new(Mutex::new(InputMetrics::default())),
        Arc::new(Mutex::new(AckTracker::default())),
        wire::Hello::default(),
    )
    .await;
    assert_eq!(*terminal.borrow(), Some(expected));
}

#[test]
fn synchronous_metric_sink_can_reenter_admission_without_the_metrics_lock() {
    struct Probe {
        metrics: Arc<Mutex<InputMetrics>>,
        events: Arc<AtomicU64>,
    }
    impl tracing::Subscriber for Probe {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            assert!(
                self.metrics.try_lock().is_ok(),
                "logging held the shared metrics lock"
            );
            self.events.fetch_add(1, Ordering::Relaxed);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let metrics = Arc::new(Mutex::new(InputMetrics::default()));
    let events = Arc::new(AtomicU64::new(0));
    metrics.lock().unwrap().record(Stage::TlsWrite, 100);
    metrics.lock().unwrap().increment(Counter::Sent, 1);
    let now = tokio::time::Instant::now();
    tracing::subscriber::with_default(
        Probe {
            metrics: Arc::clone(&metrics),
            events: Arc::clone(&events),
        },
        || {
            publish_metrics(&metrics, now + Duration::from_secs(10), false);
            assert_eq!(events.load(Ordering::Relaxed), 2);
            publish_metrics(&metrics, now + Duration::from_secs(20), false);
            assert_eq!(
                events.load(Ordering::Relaxed),
                2,
                "empty idle window was logged"
            );
        },
    );
    assert_eq!(
        metrics
            .lock()
            .unwrap()
            .snapshot()
            .stage(Stage::TlsWrite)
            .0
            .count,
        0
    );
}

#[test]
fn full_fifo_keeps_final_motion_without_another_producer_wakeup() {
    let queue = queue(1);
    queue.push(prepare(LEASE)).unwrap();
    let mut replaced = 0;
    for x in 0..10_000 {
        replaced += queue
            .input(LEASE, vec![sample(motion(f64::from(x)))])
            .unwrap();
    }
    assert_eq!(queue.depth(), 1);
    assert_eq!(replaced, 9_999);
    assert!(matches!(queue.pop(), Some(Intent::Outbound(_))));
    let Some(Intent::Input { samples, .. }) = queue.pop() else {
        panic!("resting point missing");
    };
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].event, motion(9_999.0));
    assert_eq!(samples[0].capture_host_us, Some(10));
    assert!(queue.pop().is_none());
}

#[test]
fn mixed_batch_freezes_positions_before_every_discrete_and_splits_at_128() {
    let queue = queue(4);
    queue.push(prepare(LEASE)).unwrap();
    queue.pop();
    let mut events = vec![
        sample(motion(1.0)),
        sample(button(KeyState::Down)),
        sample(motion(2.0)),
    ];
    events.extend((0..126).map(|_| sample(InputEvent::Scroll { h120: 0, v120: 120 })));
    events.push(sample(InputEvent::Key {
        key: HidKey::keyboard(4),
        state: KeyState::Up,
    }));
    events.push(sample(motion(3.0)));
    queue.input(LEASE, events).unwrap();
    let Some(Intent::Input { samples: first, .. }) = queue.pop() else {
        panic!("first frame missing");
    };
    assert_eq!(first.len(), 128);
    assert_eq!(first[0].event, motion(1.0));
    assert_eq!(first[1].event, button(KeyState::Down));
    assert_eq!(first[2].event, motion(2.0));
    let Some(Intent::Input {
        samples: second, ..
    }) = queue.pop()
    else {
        panic!("second frame missing");
    };
    assert_eq!(second.len(), 2);
    assert!(matches!(second[0].event, InputEvent::Scroll { .. }));
    assert!(matches!(second[1].event, InputEvent::Key { .. }));
    let Some(Intent::Input {
        samples: final_motion,
        ..
    }) = queue.pop()
    else {
        panic!("final point missing");
    };
    assert_eq!(final_motion[0].event, motion(3.0));
}

#[test]
fn discrete_admission_is_atomic_on_split_overflow() {
    let queue = queue(1);
    queue.input(LEASE, vec![sample(motion(5.0))]).unwrap();
    assert!(queue
        .input(
            LEASE,
            (0..129).map(|_| sample(button(KeyState::Up))).collect()
        )
        .is_err());
    assert_eq!(queue.depth(), 0);
    let Some(Intent::Input { samples, .. }) = queue.pop() else {
        panic!("overflow modified slot");
    };
    assert_eq!(samples[0].event, motion(5.0));
}

#[test]
fn normal_fences_preserve_old_motion_and_reject_late_producers() {
    let queue = queue(8);
    queue.push(prepare(LEASE)).unwrap();
    queue.input(LEASE, vec![sample(motion(1.0))]).unwrap();
    queue
        .push(Outbound::Guaranteed(fr::Message::Leave {
            lease_id: LEASE,
            reason: fr::LeaveReason::ReturnEdge,
        }))
        .unwrap();
    assert!(queue.input(LEASE, vec![sample(motion(99.0))]).is_err());
    queue.push(prepare([8; 16])).unwrap();
    queue.input([8; 16], vec![sample(motion(2.0))]).unwrap();
    assert!(queue.input(LEASE, vec![sample(motion(99.0))]).is_err());
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare {
                lease_id: LEASE,
                ..
            }
        )))
    ));
    let Some(Intent::Input {
        lease_id, samples, ..
    }) = queue.pop()
    else {
        panic!("old motion missing");
    };
    assert_eq!(lease_id, LEASE);
    assert_eq!(samples[0].event, motion(1.0));
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::Leave { .. }
        )))
    ));
    assert!(
        matches!(queue.pop(), Some(Intent::Outbound(Outbound::Guaranteed(fr::Message::EnterPrepare { lease_id, .. }))) if lease_id == [8;16])
    );
    assert!(matches!(queue.pop(), Some(Intent::Input { lease_id, .. }) if lease_id == [8;16]));
}

#[test]
fn abort_discards_old_input_but_not_fences_or_opposite_direction_control() {
    let queue = queue(8);
    queue.push(prepare(LEASE)).unwrap();
    queue
        .input(
            LEASE,
            vec![
                sample(motion(1.0)),
                sample(button(KeyState::Down)),
                sample(motion(2.0)),
            ],
        )
        .unwrap();
    queue
        .push(Outbound::Guaranteed(fr::Message::LeaseRenew {
            lease_id: [9; 16],
            nonce: [1; 16],
        }))
        .unwrap();
    assert_eq!(queue.abort_lease(LEASE), 2);
    assert!(queue.input(LEASE, vec![sample(motion(99.0))]).is_err());
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::LeaseRenew { .. }
        )))
    ));
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare { .. }
        )))
    ));
    assert!(queue.pop().is_none());
}

#[test]
fn urgent_fairness_is_eight_to_one_and_lease_controls_cannot_pass_entry() {
    let queue = queue(4);
    queue.push(prepare(LEASE)).unwrap();
    queue
        .push(Outbound::Guaranteed(fr::Message::LeaseRenew {
            lease_id: LEASE,
            nonce: [1; 16],
        }))
        .unwrap();
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare { .. }
        )))
    ));
    queue.pop();
    queue
        .input(LEASE, vec![sample(button(KeyState::Down))])
        .unwrap();
    for nonce in 0..16 {
        queue
            .urgent(Intent::Payload(wire::envelope::Payload::Pong(wire::Pong {
                nonce,
            })))
            .unwrap();
    }
    for _ in 0..8 {
        assert!(matches!(queue.pop(), Some(Intent::Payload(_))));
    }
    assert!(matches!(queue.pop(), Some(Intent::Input { .. })));
    for _ in 0..8 {
        assert!(matches!(queue.pop(), Some(Intent::Payload(_))));
    }
    for _ in 0..1_000 {
        queue
            .urgent(Intent::Payload(wire::envelope::Payload::Pong(
                Default::default(),
            )))
            .unwrap();
        assert!(queue.pop().is_some());
    }
}

#[test]
fn reserved_queues_are_bounded_independently() {
    let queue = queue(1);
    queue
        .input(LEASE, vec![sample(button(KeyState::Up))])
        .unwrap();
    for _ in 0..32 {
        queue
            .urgent(Intent::Payload(wire::envelope::Payload::Pong(
                Default::default(),
            )))
            .unwrap();
    }
    assert!(queue
        .urgent(Intent::Payload(wire::envelope::Payload::Pong(
            Default::default()
        )))
        .is_err());
    for _ in 0..16 {
        queue.push(Outbound::CapabilitiesChanged(vec![])).unwrap();
    }
    assert!(queue.push(Outbound::CapabilitiesChanged(vec![])).is_err());
    assert_eq!(queue.depth(), 1);
}

struct ScriptedWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    chunk: usize,
    delay: Duration,
    wait: Option<Pin<Box<tokio::time::Sleep>>>,
    stall: bool,
    zero: bool,
    flush_stall: bool,
}
impl ScriptedWriter {
    fn new(chunk: usize) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
            chunk,
            delay: Duration::ZERO,
            wait: None,
            stall: false,
            zero: false,
            flush_stall: false,
        }
    }
}
impl AsyncWrite for ScriptedWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.stall {
            return Poll::Pending;
        }
        if self.zero {
            return Poll::Ready(Ok(0));
        }
        if let Some(wait) = self.wait.as_mut() {
            if wait.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.wait = None;
        }
        let n = self.chunk.min(buf.len());
        self.bytes.lock().unwrap().extend_from_slice(&buf[..n]);
        if !self.delay.is_zero() {
            self.wait = Some(Box::pin(tokio::time::sleep(self.delay)));
        }
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if self.flush_stall {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
fn test_envelope() -> wire::Envelope {
    wire::Envelope {
        major: limits::PROTOCOL_MAJOR,
        minor: limits::PROTOCOL_MINOR,
        connection_id: vec![1; 16],
        seq: 1,
        payload: Some(wire::envelope::Payload::Ping(wire::Ping { nonce: 1 })),
        ..Default::default()
    }
}

#[tokio::test(start_paused = true)]
async fn partial_writes_keep_exact_frame_bytes_and_zero_is_terminal() {
    let envelope = test_envelope();
    let expected = encode_message(Channel::InputControl, &envelope).unwrap();
    let mut writer = ScriptedWriter::new(3);
    write_frame(&mut writer, &envelope).await.unwrap();
    assert_eq!(*writer.bytes.lock().unwrap(), expected);
    writer.zero = true;
    assert!(
        matches!(write_frame(&mut writer, &envelope).await, Err(TransportError::Io(e)) if e.kind() == std::io::ErrorKind::WriteZero)
    );
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_is_exact_and_flush_stalls_are_bounded() {
    for flush in [false, true] {
        let mut writer = ScriptedWriter::new(usize::MAX);
        writer.stall = !flush;
        writer.flush_stall = flush;
        let start = tokio::time::Instant::now();
        let envelope = test_envelope();
        let future = write_frame(&mut writer, &envelope);
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => panic!("early deadline: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(599)) => {}
        }
        assert!(
            matches!(future.await, Err(TransportError::Io(e)) if e.kind() == std::io::ErrorKind::TimedOut)
        );
        assert_eq!(start.elapsed(), Duration::from_millis(600));
    }
}

#[tokio::test(start_paused = true)]
async fn trickle_progress_resets_idle_but_never_the_total_frame_deadline() {
    let mut writer = ScriptedWriter::new(1);
    writer.delay = Duration::from_millis(599);
    let start = tokio::time::Instant::now();
    let error = write_frame(&mut writer, &test_envelope())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("frame deadline"));
    assert_eq!(start.elapsed(), Duration::from_millis(1000));
    assert_eq!(writer.bytes.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn writer_stamps_sequence_capture_and_registers_ack_before_completion() {
    let queue = queue(4);
    let mut state = writer_state();
    let metrics = Mutex::new(InputMetrics::default());
    let tracker = Mutex::new(AckTracker::default());
    let mut writer = ScriptedWriter::new(2);
    let hello = wire::Envelope {
        connection_id: vec![1; 16],
        payload: Some(wire::envelope::Payload::Hello(Default::default())),
        ..Default::default()
    };
    write_intent(
        &mut writer,
        Intent::Hello(hello),
        &mut state,
        &queue,
        &metrics,
        &tracker,
    )
    .await
    .unwrap();
    queue.push(prepare(LEASE)).unwrap();
    write_intent(
        &mut writer,
        queue.pop().unwrap(),
        &mut state,
        &queue,
        &metrics,
        &tracker,
    )
    .await
    .unwrap();
    queue
        .input(
            LEASE,
            vec![sample(motion(1.0)), sample(button(KeyState::Down))],
        )
        .unwrap();
    write_intent(
        &mut writer,
        queue.pop().unwrap(),
        &mut state,
        &queue,
        &metrics,
        &tracker,
    )
    .await
    .unwrap();
    assert_eq!(tracker.lock().unwrap().len(), 1);
    let bytes = writer.bytes.lock().unwrap().clone();
    let mut reader = bytes.as_slice();
    for seq in 1..=3 {
        let frame = read_frame(&mut reader).await.unwrap();
        assert_eq!(frame.seq, seq);
        if seq == 1 {
            assert!(matches!(
                frame.payload,
                Some(wire::envelope::Payload::Hello(_))
            ));
        }
        if seq == 3 {
            let Some(wire::envelope::Payload::InputBatch(batch)) = frame.payload else {
                panic!("input missing");
            };
            assert!(batch
                .events
                .iter()
                .all(|event| event.capture_monotonic_us == 10));
        }
    }
    assert!(tracker
        .lock()
        .unwrap()
        .acknowledge(0, LEASE, 3, tokio::time::Instant::now())
        .is_some());
}

/// Bench A: virtual-time trace, a scripted byte stream and an in-memory apply oracle.
/// No framed-byte drops, desktop input, network randomness or wall-clock slack.
#[tokio::test(start_paused = true)]
async fn deterministic_in_process_bench_a() {
    let queue = queue(2);
    let mut state = writer_state();
    state.tx_seq = 2;
    let metrics = Mutex::new(InputMetrics::default());
    let tracker = Mutex::new(AckTracker::default());
    let mut writer = ScriptedWriter::new(128);
    let mut seed = 17u64;
    let mut sample_count = 0u64;
    let mut coalesced = 0u64;
    let mut expected_clicks = Vec::new();
    let mut last_position = None;
    let mut click_count = 0;
    let mut applied_frames = 0;
    for tick in 0..2_000 {
        let rate = if (500..750).contains(&tick) { 8 } else { 1 };
        for _ in 0..rate {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let x = (seed % 10_000) as f64;
            coalesced += queue.input(LEASE, vec![sample(motion(x))]).unwrap();
            last_position = Some(motion(x));
            sample_count += 1;
        }
        if tick % 250 == 0 {
            expected_clicks.push(last_position.unwrap());
            queue
                .input(
                    LEASE,
                    vec![sample(button(KeyState::Down)), sample(button(KeyState::Up))],
                )
                .unwrap();
        }
        // A 40ms no-service interval models bounded socket backpressure, not packet loss.
        if tick % 50 >= 40 {
            while let Some(intent) = queue.pop() {
                write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker)
                    .await
                    .unwrap();
                applied_frames += 1;
            }
        }
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    let expected_final = last_position.unwrap();
    while let Some(intent) = queue.pop() {
        write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker)
            .await
            .unwrap();
        applied_frames += 1;
    }
    let bytes = writer.bytes.lock().unwrap().clone();
    let mut reader = bytes.as_slice();
    let mut position = None;
    let mut previous_seq = 1;
    while !reader.is_empty() {
        let frame = read_frame(&mut reader).await.unwrap();
        assert_eq!(frame.seq, previous_seq + 1);
        previous_seq = frame.seq;
        let fr::Message::InputBatch { events, .. } = convert::from_envelope(&frame).unwrap() else {
            panic!("unexpected control");
        };
        for event in events {
            if matches!(event, InputEvent::PointerAbsolute(_)) {
                position = Some(event);
            }
            if event == button(KeyState::Down) {
                assert!(
                    position == Some(expected_clicks[click_count]),
                    "click position mismatch"
                );
                click_count += 1;
            }
        }
        assert!(tracker
            .lock()
            .unwrap()
            .acknowledge(0, LEASE, frame.seq, tokio::time::Instant::now())
            .is_some());
    }
    assert!(position == Some(expected_final), "final position mismatch");
    assert_eq!(click_count, expected_clicks.len());
    assert_eq!(queue.depth(), 0);
    assert!(coalesced > 1_000);
    let snapshot = metrics.lock().unwrap().snapshot();
    let age = snapshot.stage(Stage::CaptureToWrite).1;
    assert!(age.count > 0);
    println!("{{\"bench\":\"A\",\"seed\":17,\"sample_count\":{sample_count},\"coalesced_count\":{coalesced},\"sent\":{applied_frames},\"acked\":{applied_frames},\"final_position_ok\":true,\"click_position_failures\":0,\"discrete_order_failures\":0,\"cross_lease_leaks\":0,\"capture_to_write_p95_us\":{},\"capture_to_write_p99_us\":{},\"stall_count\":{}}}", age.p95_us.unwrap(), age.p99_us.unwrap(), age.over_50ms);
}

#[tokio::test(start_paused = true)]
async fn ack_can_arrive_while_write_is_still_pending() {
    let queue = queue(2);
    queue
        .input(LEASE, vec![sample(button(KeyState::Down))])
        .unwrap();
    let mut state = writer_state();
    let metrics = Mutex::new(InputMetrics::default());
    let tracker = Mutex::new(AckTracker::default());
    let mut writer = ScriptedWriter::new(1);
    writer.stall = true;
    let future = write_intent(
        &mut writer,
        queue.pop().unwrap(),
        &mut state,
        &queue,
        &metrics,
        &tracker,
    );
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("stalled frame completed: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(1)) => {}
    }
    assert_eq!(tracker.lock().unwrap().len(), 1);
    assert!(tracker
        .lock()
        .unwrap()
        .acknowledge(0, LEASE, 1, tokio::time::Instant::now())
        .is_some());
    assert!(future.await.is_err());
    assert!(tracker.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn blocked_writer_does_not_block_reader_or_coordinator_dispatch() {
    let queue = Arc::new(queue(4));
    queue
        .input(LEASE, vec![sample(button(KeyState::Down))])
        .unwrap();
    let (terminal, mut failure) = tokio::sync::watch::channel(None);
    let metrics = Arc::new(Mutex::new(InputMetrics::default()));
    let tracker = Arc::new(Mutex::new(AckTracker::default()));
    let mut writer = ScriptedWriter::new(1);
    writer.stall = true;
    let writer_task = tokio::spawn(writer_loop(
        writer,
        Arc::clone(&queue),
        terminal,
        writer_state(),
        Arc::clone(&metrics),
        Arc::clone(&tracker),
        wire::Hello::default(),
    ));
    tokio::task::yield_now().await;
    let diagnostics = Arc::new(LeaseTransportCounters::default());
    let mut state = LinkState::new(
        NodeId::new([2; 32]),
        [1; 16],
        [3; 16],
        Arc::clone(&diagnostics),
        Arc::new(Mutex::new(RttMeter::default())),
        metrics,
        tracker,
    );
    state.announced = true;
    state.rx_seq.accept(1).unwrap();
    let mut handshake =
        HelloHandshake::dialer(limits::PROTOCOL_MAJOR, limits::PROTOCOL_MINOR, [1; 16]);
    let (mut sender, receiver) = tokio::io::duplex(4096);
    let (reader, _) = tokio::io::split(receiver);
    let (frames_tx, mut frames_rx) = mpsc::channel(8);
    let reader_task = tokio::spawn(read_loop(reader, frames_tx, diagnostics));
    let (reports_tx, mut reports_rx) = mpsc::channel(4);
    for seq in [2, 3] {
        let mut envelope = test_envelope();
        envelope.seq = seq;
        if seq == 3 {
            envelope.lease_id = vec![9; 16];
            envelope.payload = Some(wire::envelope::Payload::LeaseRenew(wire::LeaseRenew {
                nonce: vec![1; 16],
            }));
        }
        write_frame(&mut sender, &envelope).await.unwrap();
        let (frame, _) = frames_rx.recv().await.unwrap().unwrap();
        state
            .on_frame(
                &queue,
                &mut handshake,
                &frame,
                &wire::Hello::default(),
                &reports_tx,
            )
            .await
            .unwrap();
    }
    assert!(matches!(
        reports_rx.recv().await,
        Some(LinkReport::Message {
            message: convert::Inbound::Focus(fr::Message::LeaseRenew { .. }),
            ..
        })
    ));
    assert!(failure.borrow().is_none());
    tokio::time::advance(Duration::from_millis(599)).await;
    assert!(failure.borrow().is_none());
    tokio::time::advance(Duration::from_millis(1)).await;
    failure.changed().await.unwrap();
    assert!(
        matches!(&*failure.borrow(), Some(LinkDownReason::Io(reason)) if reason.contains("idle timeout"))
    );
    writer_task.await.unwrap();
    reader_task.abort();
}

#[tokio::test(start_paused = true)]
async fn independent_router_deadline_returns_control_within_ttl_plus_100() {
    let local = NodeId::new([1; 32]);
    let peer = NodeId::new([2; 32]);
    let capabilities = fr::Capabilities {
        capture: true,
        inject: true,
        absolute_pointer: true,
        emergency_hotkey: true,
    };
    for ttl in [300u64, 600, 1000] {
        let mut router = fr::FocusRouter::new(
            local,
            fr::Timing {
                lease_ttl_ms: ttl,
                ..Default::default()
            },
            fr::SequentialIds::default(),
            capabilities,
        )
        .with_persisted_ownership(1, Some((1, local)));
        router.handle(
            0,
            fr::Input::Hello {
                peer,
                hello: fr::PeerHello {
                    can_own: true,
                    capabilities,
                    owner_serial: 1,
                    owner: Some(local),
                },
            },
        );
        router.handle(
            10,
            fr::Input::Local(fr::LocalInput::SwitchRequest {
                trigger: fr::SwitchTrigger::Edge,
                target: peer,
                entry_position: geometry::Position::new("display", 1.0, 1.0),
                topology_revision: 0,
            }),
        );
        let mut lease = [0; 16];
        lease[..8].copy_from_slice(&1u64.to_be_bytes());
        router.handle(
            11,
            fr::Input::Agent(fr::AgentEvent::CaptureReady { lease_id: lease }),
        );
        router.handle(
            12,
            fr::Input::Net {
                from: peer,
                message: fr::Message::EnterAck {
                    lease_id: lease,
                    accepted: true,
                    reason: None,
                },
            },
        );
        router.handle(
            13,
            fr::Input::Net {
                from: peer,
                message: fr::Message::LeaseChallenge {
                    lease_id: lease,
                    nonce: [8; 16],
                    ttl_ms: ttl as u32,
                },
            },
        );
        let outputs = router.handle(
            14,
            fr::Input::Agent(fr::AgentEvent::CaptureReady { lease_id: lease }),
        );
        assert_eq!(router.focus(), fr::FocusState::Remote);
        let deadline = outputs
            .iter()
            .find_map(|output| match output {
                fr::Output::ArmTimer {
                    timer: fr::Timer::SourceLeaseWatchdog,
                    deadline_ms,
                } => Some(*deadline_ms),
                _ => None,
            })
            .expect("source watchdog must be armed");
        let mut writer = ScriptedWriter::new(1);
        writer.stall = true;
        let envelope = test_envelope();
        let stalled = write_frame(&mut writer, &envelope);
        tokio::pin!(stalled);
        let start = tokio::time::Instant::now();
        // Poll the writer concurrently; its longer timeout cannot postpone the router.
        tokio::select! {
            _ = &mut stalled => {}
            () = tokio::time::sleep(Duration::from_millis(deadline - 14)) => {}
        }
        if start.elapsed() < Duration::from_millis(deadline - 14) {
            tokio::time::advance(Duration::from_millis(deadline - 14) - start.elapsed()).await;
        }
        let outputs = router.handle(
            deadline,
            fr::Input::TimerExpired(fr::Timer::SourceLeaseWatchdog),
        );
        assert!(outputs.iter().any(|output| matches!(output,
            fr::Output::Agent(fr::AgentCommand::EndCapture { lease_id, .. }) if *lease_id == lease)));
        let control_return_us = duration_us(start.elapsed());
        assert!(control_return_us <= (ttl + 100) * 1000);
        assert_eq!(router.focus(), fr::FocusState::Local);
        println!("{{\"bench\":\"A-safety\",\"ttl_ms\":{ttl},\"control_return_us\":{control_return_us},\"release_effect\":true}}");
    }
}

#[path = "bench_a.rs"]
mod bench_a;

#[tokio::test(start_paused = true)]
async fn idle_599_600_601_and_total_999_1000_are_exact_boundaries() {
    for pause_ms in [599, 600, 601] {
        let mut writer = ScriptedWriter::new(usize::MAX);
        writer.wait = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
            pause_ms,
        ))));
        let started = tokio::time::Instant::now();
        let result = write_frame(&mut writer, &test_envelope()).await;
        assert_eq!(result.is_ok(), pause_ms == 599);
        assert_eq!(started.elapsed(), Duration::from_millis(pause_ms.min(600)));
    }
    let mut writer = ScriptedWriter::new(1);
    writer.delay = Duration::from_millis(499);
    let envelope = test_envelope();
    let started = tokio::time::Instant::now();
    let future = write_frame(&mut writer, &envelope);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("total deadline fired before 999ms: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(999)) => {}
    }
    assert!(future
        .await
        .unwrap_err()
        .to_string()
        .contains("frame deadline"));
    assert_eq!(started.elapsed(), Duration::from_millis(1000));
}

#[tokio::test(start_paused = true)]
async fn all_normal_close_stages_share_the_first_100ms_deadline() {
    let queue = queue(4);
    queue.input(LEASE, vec![sample(motion(1.0))]).unwrap();
    let started = tokio::time::Instant::now();
    let first = queue.begin_close();
    tokio::time::advance(Duration::from_millis(60)).await;
    assert_eq!(queue.begin_close(), first);
    assert_eq!(first.duration_since(started), GRACEFUL_CLOSE_BUDGET);
    assert!(queue.input(LEASE, vec![sample(motion(2.0))]).is_err());
    let Some(Intent::Input { samples, .. }) = queue.pop() else {
        panic!("accepted motion lost on normal close");
    };
    assert_eq!(samples[0].event, motion(1.0));
    let fresh = InputQueue::new(1);
    fresh.begin_close();
    assert!(fresh.input(LEASE, vec![sample(motion(2.0))]).is_err());
}

#[test]
fn stale_abort_invalidates_a_selected_old_frame_without_touching_new_lease() {
    let queue = queue(8);
    queue.push(prepare(LEASE)).unwrap();
    queue.pop();
    queue
        .input(LEASE, vec![sample(button(KeyState::Down))])
        .unwrap();
    let Some(Intent::Input { generation, .. }) = queue.pop() else {
        panic!("selected input missing");
    };
    queue
        .push(Outbound::Guaranteed(fr::Message::Leave {
            lease_id: LEASE,
            reason: fr::LeaveReason::ReturnEdge,
        }))
        .unwrap();
    queue.push(prepare([8; 16])).unwrap();
    queue.input([8; 16], vec![sample(motion(2.0))]).unwrap();
    queue.abort_lease(LEASE);
    assert!(!queue.valid_generation(generation));
    queue.pop();
    queue.pop();
    let Some(Intent::Input {
        generation,
        lease_id,
        samples,
    }) = queue.pop()
    else {
        panic!("new lease was affected");
    };
    assert_eq!(lease_id, [8; 16]);
    assert!(queue.valid_generation(generation));
    assert_eq!(samples[0].event, motion(2.0));
}

#[tokio::test]
async fn invalid_local_unpair_notice_does_not_close_a_healthy_link() {
    let handle = LinkHandle::for_counter_test(NodeId::new([2; 32]));
    handle.queue.ready(true);
    let notice = wire::Unpaired {
        requester_node_id: vec![9; 32],
        ..Default::default()
    };
    assert!(handle.send_unpaired(notice).await.is_err());
    assert!(handle.terminal.borrow().is_none());
    assert!(handle.queue.input(LEASE, vec![sample(motion(1.0))]).is_ok());
}

#[test]
fn replacement_keeps_the_newest_samples_own_capture_metadata() {
    let queue = queue(1);
    let first = sample(motion(1.0));
    let mut newest = sample(motion(2.0));
    newest.capture_host_us = Some(123);
    newest.capture_to_enqueue_us = Some(456);
    assert_eq!(queue.input(LEASE, vec![first, newest]).unwrap(), 1);
    let Some(Intent::Input { samples, .. }) = queue.pop() else {
        panic!("motion missing");
    };
    assert_eq!(samples[0].event, motion(2.0));
    assert_eq!(samples[0].capture_host_us, Some(123));
    assert_eq!(samples[0].capture_to_enqueue_us, Some(456));
}

#[tokio::test(start_paused = true)]
async fn network_ack_absence_does_not_gate_fresh_input_serialization() {
    let queue = queue(64);
    let mut state = writer_state();
    let metrics = Mutex::new(InputMetrics::default());
    let tracker = Mutex::new(AckTracker::default());
    let mut writer = ScriptedWriter::new(4096);
    for index in 0..150 {
        queue
            .input(LEASE, vec![sample(motion(f64::from(index)))])
            .unwrap();
        let intent = queue.pop().expect("network ACK loss stalled fresh input");
        write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker)
            .await
            .unwrap();
    }
    queue
        .input(LEASE, vec![sample(button(KeyState::Down))])
        .unwrap();
    let intent = queue
        .pop()
        .expect("network ACK loss stalled a discrete edge");
    write_intent(&mut writer, intent, &mut state, &queue, &metrics, &tracker)
        .await
        .unwrap();
    assert_eq!(tracker.lock().unwrap().len(), 151);
}

#[test]
fn later_metadata_stays_behind_a_fence_with_a_long_ordered_input_prefix() {
    let queue = queue(16);
    for _ in 0..9 {
        queue
            .input(LEASE, vec![sample(button(KeyState::Down))])
            .unwrap();
    }
    queue.push(prepare(LEASE)).unwrap();
    queue.push(topology(2)).unwrap();
    for _ in 0..9 {
        assert!(matches!(queue.pop(), Some(Intent::Input { .. })));
    }
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Guaranteed(
            fr::Message::EnterPrepare { .. }
        )))
    ));
    assert!(matches!(
        queue.pop(),
        Some(Intent::Outbound(Outbound::Layout(
            convert::LayoutMessage::Topology(_)
        )))
    ));
}
