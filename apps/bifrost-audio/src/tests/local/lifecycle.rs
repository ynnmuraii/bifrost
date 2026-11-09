use super::*;

static OUTPUT_OWNER: Mutex<Option<DryStream>> = Mutex::new(None);

struct OutputOwner;
impl Drop for OutputOwner {
    fn drop(&mut self) {
        lock(&OUTPUT_OWNER).take();
    }
}

fn occupied_output_backend() -> Result<DryBackend, BackendError> {
    let mut backend = dry_backend()?;
    let mut owner = backend.start_capture(
        CaptureSpec::Monitor {
            sink: DeviceSelection::Explicit(DeviceId::new("DRY_OUTPUT")?),
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        },
        Box::new(|_| {}),
    )?;
    owner.suppress_local_playback(Arc::new(|| true))?;
    *lock(&OUTPUT_OWNER) = Some(owner);
    Ok(backend)
}

#[tokio::test]
async fn worker_emits_session_error_and_keeps_mic_owner() {
    let _owner = OutputOwner;
    let mut listener = child().await;
    let mut source = child_with_factory(Some(occupied_output_backend)).await;
    let (a, b) = ports();
    source
        .core
        .send(19, Msg::ProbeBackend(w::ProbeBackend {}))
        .await
        .unwrap();
    loop {
        let frame = scripted::next_frame(&mut source).await.frame;
        match frame.msg.unwrap() {
            Msg::Ready(ready) if frame.request_id == 19 => {
                assert!(ready.probed);
                break;
            }
            Msg::Error(error) => panic!(
                "probe setup rejected: id={} code={}",
                frame.request_id, error.code
            ),
            _ => {}
        }
    }

    source
        .core
        .send(
            20,
            Msg::AcquireMicMute(w::AcquireMicMute {
                selection: Some(w::DeviceSelection {
                    selector: Some(w::device_selection::Selector::DeviceId(
                        "DRY_CAPTURE".into(),
                    )),
                }),
            }),
        )
        .await
        .unwrap();
    let initial_mic = loop {
        let frame = scripted::next_frame(&mut source).await.frame;
        match frame.msg.unwrap() {
            Msg::MuteSnapshot(snapshot) if frame.request_id == 20 => break snapshot,
            Msg::Error(error) => panic!("mic setup rejected: {}", error.code),
            _ => {}
        }
    };
    assert!(initial_mic.guard_active);

    listener
        .core
        .send(2, Msg::StartSession(start(2, b, a)))
        .await
        .unwrap();
    source
        .core
        .send(2, Msg::StartSession(start(1, a, b)))
        .await
        .unwrap();
    source.renew_ids.push(13);
    listener.renew_ids.push(13);
    let deadline = tokio::time::sleep(Duration::from_secs(3));
    tokio::pin!(deadline);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut error = None;
    let mut stopped = false;
    let mut serial = 1;
    while error.is_none() || !stopped {
        tokio::select! {
            _ = &mut deadline => panic!("missing async local-silence failure / native Stopped"),
            _ = tick.tick() => {
                for fixture in [&mut source, &mut listener] {
                    fixture.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                }
                if !stopped && error.is_none() { renew(&mut source.core, 13, 1000).await; }
                renew(&mut listener.core, 13, 1000).await;
                serial += 1;
            }
            frame = source.core.recv() => {
                let frame = frame.unwrap().frame;
                match frame.msg.unwrap() {
                    Msg::Error(value) if frame.request_id == 0 => {
                        assert_eq!(frame.request_id, 0, "actual worker asynchronous response ID");
                        assert_eq!(value.session_id, vec![13; 16]);
                        assert_eq!(value.code, 8, "Dry output is already owned; native suppression fails Busy");
                        assert!(error.replace((frame.request_id, value)).is_none());
                    }
                    Msg::SessionEvent(event) if event.kind == 5 => {
                        assert!(error.is_some(), "Stopped must not hide the reason");
                        assert_eq!(event.session_id, vec![13; 16]);
                        assert_eq!(event.reason, 4);
                        stopped = true;
                    }
                    _ => {}
                }
            }
            frame = listener.core.recv() => {
                assert!(!matches!(frame.unwrap().frame.msg, Some(Msg::Error(_))));
            }
        }
    }
    source.renew_ids.clear();
    source
        .core
        .send(
            21,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: initial_mic.generation,
                muted: true,
            }),
        )
        .await
        .unwrap();
    loop {
        let frame = scripted::next_frame(&mut source).await.frame;
        match frame.msg.unwrap() {
            Msg::MuteSnapshot(snapshot) if frame.request_id == 21 => {
                assert_eq!(snapshot.generation, initial_mic.generation);
                assert!(
                    snapshot.guard_active && snapshot.muted,
                    "session-local failure must retain independent mic ownership"
                );
                break;
            }
            Msg::Error(value) => panic!("post-stop mic control failed: {}", value.code),
            _ => {}
        }
    }
    for fixture in [&mut source, &mut listener] {
        fixture
            .core
            .send(30, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
    }
    assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    assert_eq!(
        listener.handle.await.unwrap().unwrap(),
        ExitReason::Shutdown
    );
}
