//! The actual child IPC/media workers rotate beyond the old initial-key tombstone bound.
use super::*;
fn rotation(epoch: u32, activate: bool) -> w::RotateSession {
    w::RotateSession {
        session_id: vec![13; 16],
        epoch,
        activate,
        forward: (!activate).then(|| w::DirectionMaterial {
            key: vec![41; 32],
            nonce_prefix: vec![51; 4],
        }),
        reverse: (!activate).then(|| w::DirectionMaterial {
            key: vec![42; 32],
            nonce_prefix: vec![52; 4],
        }),
    }
}
async fn observed(core: &mut AudioLink<transport::ClientStream>, epoch: u32, kind: u32) {
    loop {
        match next(core).await {
            Msg::RotationEvent(event) if event.epoch == epoch && event.kind == kind => return,
            Msg::Error(error) => panic!("rotation rejected: {}", error.code),
            Msg::SessionEvent(event) if event.kind == 5 => {
                panic!("rotation stopped: {}", event.reason)
            }
            _ => {}
        }
    }
}
#[tokio::test]
#[cfg(debug_assertions)]
async fn more_than_256_handoffs_keep_native_media_and_initial_key_reuse_guard() {
    let mut source = child().await;
    let mut listener = child().await;
    let (a, b) = ports();
    listener
        .core
        .send(2, Msg::StartSession(start(2, b, a)))
        .await
        .unwrap();
    session_event(&mut listener.core, 1, b).await;
    source
        .core
        .send(2, Msg::StartSession(start(1, a, b)))
        .await
        .unwrap();
    session_event(&mut source.core, 1, a).await;
    session_event(&mut source.core, 6, 0).await;
    session_event(&mut listener.core, 6, 0).await;
    for epoch in 2..=260 {
        for fixture in [&mut source, &mut listener] {
            fixture
                .core
                .send(
                    0,
                    Msg::Ping(w::Ping {
                        serial: u64::from(epoch),
                    }),
                )
                .await
                .unwrap();
            renew(&mut fixture.core, 13, 1000).await;
            fixture
                .core
                .send(
                    u64::from(epoch) * 4,
                    Msg::SetTestClock(w::SetTestClock {
                        offset_ms: u64::from(epoch) * 3000,
                    }),
                )
                .await
                .unwrap();
        }
        listener
            .core
            .send(
                u64::from(epoch) * 4 + 1,
                Msg::RotateSession(rotation(epoch, false)),
            )
            .await
            .unwrap();
        observed(&mut listener.core, epoch, 2).await;
        source
            .core
            .send(
                u64::from(epoch) * 4 + 1,
                Msg::RotateSession(rotation(epoch, false)),
            )
            .await
            .unwrap();
        observed(&mut source.core, epoch, 2).await;
        listener
            .core
            .send(
                u64::from(epoch) * 4 + 2,
                Msg::RotateSession(rotation(epoch, true)),
            )
            .await
            .unwrap();
        observed(&mut listener.core, epoch, 3).await;
        source
            .core
            .send(
                u64::from(epoch) * 4 + 2,
                Msg::RotateSession(rotation(epoch, true)),
            )
            .await
            .unwrap();
        observed(&mut source.core, epoch, 3).await;
    }
    let (active, received) = drive(&mut source, &mut listener, Duration::from_millis(1200)).await;
    assert!(active && received);
    assert!(listener.observed.rms() > 0.1);
    source
        .core
        .send(
            2000,
            Msg::StopSession(w::StopSession {
                session_id: vec![13; 16],
            }),
        )
        .await
        .unwrap();
    session_event(&mut source.core, 5, 0).await;
    // A heartbeat renewal may arrive after the stopped session was retired.
    renew(&mut source.core, 13, 1000).await;
    let mut reused = start(1, a, b);
    reused.session_id = vec![14; 16];
    for (request_id, session_id, expected_code, attempt) in
        [(2001, 14, 5, reused), (2002, 13, 4, start(1, a, b))]
    {
        source
            .core
            .send(request_id, Msg::StartSession(attempt))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let mut received = source.core.recv().await.unwrap();
                let message = received.frame.msg.take().unwrap();
                if received.frame.request_id == request_id {
                    let Msg::Error(error) = message else {
                        panic!("guarded StartSession {request_id} answered with a non-error frame");
                    };
                    assert_eq!(error.session_id.as_slice(), &[session_id; 16]);
                    assert_eq!(error.code, expected_code);
                    break;
                }
                if let Msg::Error(error) = message {
                    assert!(
                        received.frame.request_id == 100
                            && error.session_id.as_slice() == [13; 16]
                            && error.code == 6,
                        "unexpected uncorrelated error: request={} session={:?} code={}",
                        received.frame.request_id,
                        error.session_id,
                        error.code,
                    );
                }
            }
        })
        .await
        .expect("the guarded start answers its own request before the absolute deadline");
    }
    source
        .core
        .send(2003, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    listener
        .core
        .send(2003, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    assert_eq!(
        listener.handle.await.unwrap().unwrap(),
        ExitReason::Shutdown
    );
}
