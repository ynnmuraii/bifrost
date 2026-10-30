//! In-process public-interface check; deterministic enqueue bursts, real loopback TLS.
//! Scripted virtual-time network/write impairments live in the link unit bench.
use super::*;
use input_model::{InputEvent, KeyState, MouseButton, Position};
use transport::input_metrics::{Counter, Stage};

fn position(x: f64) -> InputEvent {
    InputEvent::PointerAbsolute(Position::new(x, 10.0))
}

fn prepare(lease_id: [u8; 16]) -> Message {
    Message::EnterPrepare {
        lease_id,
        topology_revision: 1,
        position: geometry::Position::new("screen", 0.0, 10.0),
        owner_serial: 1,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn burst_resting_point_click_and_lease_fences_survive_public_tls_pipeline() {
    let root = temp_root("input-pipeline");
    let a_identity = Arc::new(identity_at(&root, "a"));
    let b_identity = Arc::new(identity_at(&root, "b"));
    let (a_id, b_id) = (a_identity.node_id(), b_identity.node_id());
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node("a", a_identity, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b_identity, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    settle(&mut a, Duration::from_millis(50)).await;
    settle(&mut b, Duration::from_millis(50)).await;
    let sender = a.manager.handle(&b_id).await.expect("sender");
    let receiver = b.manager.handle(&a_id).await.expect("receiver");
    let first = [17; 16];
    let second = [23; 16];
    sender.send(prepare(first)).await.expect("first prepare");
    wait_for_event(
        &mut b.events,
        |event| matches!(event, LinkEvent::Message { message: Inbound::Focus(Message::EnterPrepare { lease_id, .. }), .. } if *lease_id == first),
        EVENT_TIMEOUT,
    ).await.expect("first lease barrier");

    // No runtime yield while admitting this burst: queue capacity cannot decide which
    // final position survives. The last sample must survive without another producer tick.
    for index in 0..10_000u64 {
        sender
            .send_timed_input(
                first,
                vec![(position(index as f64), Some(10_000 + index))],
                Some(11_000 + index),
            )
            .await
            .expect("latest sample admission");
    }
    sender
        .send_timed_input(
            first,
            vec![
                (
                    InputEvent::Button {
                        button: MouseButton::Left,
                        state: KeyState::Down,
                    },
                    Some(20_000),
                ),
                (
                    InputEvent::Button {
                        button: MouseButton::Left,
                        state: KeyState::Up,
                    },
                    Some(20_001),
                ),
            ],
            Some(21_001),
        )
        .await
        .expect("click admission");
    sender
        .send_timed_input(
            first,
            vec![(position(20_000.0), Some(20_002))],
            Some(21_002),
        )
        .await
        .expect("final first-lease position");
    sender
        .send(Message::Leave {
            lease_id: first,
            reason: focus_router::LeaveReason::Hotkey,
        })
        .await
        .expect("leave barrier");
    sender
        .send(prepare(second))
        .await
        .expect("replacement lease");
    assert!(
        !sender.send_motion(first, vec![position(-1.0)]),
        "stale producer must not rebind the slot"
    );
    sender
        .send_timed_input(
            second,
            vec![(position(30_000.0), Some(30_000))],
            Some(31_000),
        )
        .await
        .expect("final resting point without later wakeup");

    let mut click_count = 0;
    let mut leave_seen = false;
    let mut second_seen = false;
    let mut cursor = None;
    let mut last_sequence = 0;
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = b.events.recv().await.expect("live pipeline");
            let LinkEvent::Message { message: Inbound::Focus(message), .. } = event else { continue };
            match message {
                Message::InputBatch { lease_id, input_batch_seq, events } => {
                    assert!(input_batch_seq > last_sequence);
                    last_sequence = input_batch_seq;
                    for event in &events {
                        match event {
                            InputEvent::PointerAbsolute(point) => {
                                assert_eq!(lease_id, if second_seen { second } else { first });
                                cursor = Some(point.x);
                            }
                            InputEvent::Button { .. } => {
                                assert_eq!(cursor, Some(9_999.0), "click must use its preceding position, not a later resting point");
                                click_count += 1;
                            }
                            _ => panic!("unexpected synthetic event"),
                        }
                    }
                    receiver.send(Message::InjectAck {
                        lease_id,
                        input_batch_seq,
                        applied_count: events.len() as u32,
                        actual_position: None,
                        status: focus_router::InjectStatus::Ok,
                    }).await.expect("ACK follows in-memory application");
                    if second_seen && cursor == Some(30_000.0) { break; }
                }
                Message::Leave { lease_id, .. } => {
                    assert_eq!(lease_id, first);
                    assert_eq!(cursor, Some(20_000.0), "old resting point precedes its lease fence");
                    assert_eq!(click_count, 2);
                    leave_seen = true;
                }
                Message::EnterPrepare { lease_id, .. } if lease_id == second => {
                    assert!(leave_seen, "replacement cannot overtake old lease termination");
                    second_seen = true;
                }
                _ => {}
            }
        }
    }).await.expect("final position and all discrete edges delivered");
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let snapshot = sender.input_metrics();
            if snapshot.stage(Stage::InjectAckRtt).1.count > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input ACK timing, not Ping RTT");
    let snapshot = sender.input_metrics();
    assert!(snapshot.counter(Counter::MotionOverwritten).1 >= 9_999);
    let age = snapshot.stage(Stage::CaptureToWrite).1;
    assert!(age.count > 0);
    assert!(age.p95_us.is_some_and(|age| age >= 1_000));
    assert!(age.p99_us.is_some());
    println!("input_pipeline_loopback capture_to_write_p95_us={:?} capture_to_write_p99_us={:?} stalls_count={} stalls_total_us={} final_position_ok=true click_position_failures=0 discrete_order_failures=0 cross_lease_leaks=0", age.p95_us, age.p99_us, age.over_50ms, age.stall_total_us);
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "current_thread")]
async fn aborting_coordinator_drops_its_reader_and_writer_before_liveness_timeout() {
    let root = temp_root("input-pipeline-abort");
    let a_identity = Arc::new(identity_at(&root, "a"));
    let b_identity = Arc::new(identity_at(&root, "b"));
    let (a_id, b_id) = (a_identity.node_id(), b_identity.node_id());
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node("a", a_identity, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b_identity, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    settle(&mut a, Duration::from_millis(50)).await;
    settle(&mut b, Duration::from_millis(50)).await;
    let old_handle = a.manager.handle(&b_id).await.expect("live handle");
    a.kill().await;
    let down = wait_for_event(
        &mut b.events,
        |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == a_id),
        Duration::from_secs(1),
    )
    .await;
    assert!(
        matches!(down, Some(LinkEvent::Down { reason: LinkDownReason::Io(_), .. })),
        "aborting the coordinator must close its TLS halves, not orphan them until the 3s Ping timeout"
    );
    assert!(
        !old_handle.send_motion([17; 16], vec![position(1.0)]),
        "a retained handle must reject input after its coordinator was aborted"
    );
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}
