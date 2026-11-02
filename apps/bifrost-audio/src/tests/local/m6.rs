use super::scripted::{self, Script};
use super::*;
use audio_backend::{MediaCommands, MediaInfo, MediaSessionToken, PlaybackState};
use std::sync::mpsc::Sender;

// Drive both real IPC peers, independently of the Source's authorization lease.
async fn next_pair(
    source: &mut Fixture,
    listener: &mut Fixture,
    complete: Option<&Sender<Script>>,
) -> (bool, contract::Received) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                frame = source.core.recv() => return (true, frame.unwrap()),
                frame = listener.core.recv() => return (false, frame.unwrap()),
                _ = tick.tick() => {
                    for fixture in [&mut *source, &mut *listener] {
                        fixture.core.send(0, Msg::Ping(w::Ping { serial: 1 })).await.unwrap();
                        for id in fixture.renew_ids.clone() {
                            renew(&mut fixture.core, id, 1000).await;
                        }
                    }
                    if let Some(script) = complete {
                        script.send(Script::CompleteMedia).unwrap();
                    }
                }
            }
        }
    })
    .await
    .unwrap()
}

async fn quiet(source: &mut Fixture, listener: &mut Fixture, duration: Duration, consented: bool) {
    assert!(tokio::time::timeout(duration, async {
        loop {
            let (_, mut received) = next_pair(source, listener, None).await;
            match received.frame.msg.take().unwrap() {
                Msg::MediaSessions(snapshot) => {
                    if !consented {
                        assert_empty(&snapshot);
                    }
                }
                Msg::MediaCommandResult(_) => panic!("unexpected command completion"),
                Msg::Error(error) => panic!("unexpected wire error: {}", error.code),
                _ => {}
            }
        }
    })
    .await
    .is_err());
}

async fn snapshot(source: &mut Fixture, listener: &mut Fixture) -> w::MediaSessions {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (from_source, mut received) = next_pair(source, listener, None).await;
            match received.frame.msg.take().unwrap() {
                Msg::MediaSessions(snapshot) => {
                    assert!(from_source);
                    assert_eq!(received.frame.request_id, 0);
                    return snapshot;
                }
                Msg::Error(error) => panic!("unexpected wire error: {}", error.code),
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}

async fn result(
    source: &mut Fixture,
    listener: &mut Fixture,
    request_id: u64,
    expected: i32,
    complete: Option<&Sender<Script>>,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (from_source, mut received) = next_pair(source, listener, complete).await;
            match received.frame.msg.take().unwrap() {
                Msg::MediaCommandResult(result) => {
                    assert!(from_source);
                    assert_eq!(received.frame.request_id, request_id);
                    assert_eq!(result.result, expected);
                    return;
                }
                Msg::MediaSessions(_) => panic!("unexpected media snapshot before completion"),
                Msg::Error(error) => panic!("unexpected wire error: {}", error.code),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

fn media(revision: u64) -> MediaInfo {
    let mut info = MediaInfo::new(
        "org.mpris.MediaPlayer2.fixture".into(),
        "KnownTitle".into(),
        "KnownArtist".into(),
        MediaSessionToken(1),
        revision,
        PlaybackState::Paused,
        MediaCommands {
            play: true,
            ..MediaCommands::default()
        },
    )
    .unwrap();
    info.position_ms = Some(12);
    info.duration_ms = Some(120_000);
    info
}

fn assert_metadata(snapshot: &w::MediaSessions, revision: u64, state: i32) {
    assert_eq!(snapshot.current_token, Some(1));
    assert_eq!(snapshot.sessions.len(), 1);
    let info = &snapshot.sessions[0];
    assert_eq!(info.token, 1);
    assert_eq!(info.revision, revision);
    // Do not use assert_eq!: failure diagnostics must not print private fields.
    assert!(info.app_id == "org.mpris.MediaPlayer2.fixture");
    assert!(info.title == "KnownTitle");
    assert!(info.subtitle == "KnownArtist");
    assert_eq!(info.position_ms, Some(12));
    assert_eq!(info.duration_ms, Some(120_000));
    assert_eq!(info.state, state);
    assert_eq!(info.commands, 1);
}

fn assert_empty(snapshot: &w::MediaSessions) {
    assert!(snapshot.sessions.is_empty());
    assert!(snapshot.current_token.is_none());
}

#[tokio::test]
async fn media_consent_commands_and_source_lease_over_real_audio_link() {
    let (mut source, script) = scripted::child(1).await;
    let mut listener = child().await;
    source
        .core
        .send(2, Msg::ProbeBackend(w::ProbeBackend {}))
        .await
        .unwrap();
    loop {
        let (from_source, mut received) = next_pair(&mut source, &mut listener, None).await;
        if let Some(Msg::Ready(ready)) = received.frame.msg.take() {
            assert!(from_source && ready.probed);
            assert_eq!(received.frame.request_id, 2);
            break;
        }
    }
    script
        .send(Script::Event(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(media(1)),
        }))
        .unwrap();
    script
        .send(Script::Event(ScriptEvent::SetCurrentMediaSession {
            session: Some(MediaSessionToken(1)),
        }))
        .unwrap();
    quiet(
        &mut source,
        &mut listener,
        Duration::from_millis(300),
        false,
    )
    .await;

    source
        .core
        .send(3, Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
        .await
        .unwrap();
    loop {
        let (from_source, mut received) = next_pair(&mut source, &mut listener, None).await;
        match received.frame.msg.take().unwrap() {
            Msg::Error(error) => {
                assert!(from_source);
                assert_eq!(received.frame.request_id, 3);
                assert_eq!(error.code, 6);
                break;
            }
            Msg::MediaSessions(snapshot) => assert_empty(&snapshot),
            _ => {}
        }
    }

    let (a, b) = ports();
    listener
        .core
        .send(4, Msg::StartSession(start(2, b, a)))
        .await
        .unwrap();
    listener.renew_ids.push(13);
    source
        .core
        .send(4, Msg::StartSession(start(1, a, b)))
        .await
        .unwrap();
    source.renew_ids.push(13);
    let (mut source_started, mut listener_started, mut source_path, mut listener_path, mut sending) =
        (false, false, false, false, false);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !(source_started && listener_started && source_path && listener_path && sending) {
            let (from_source, mut received) = next_pair(&mut source, &mut listener, None).await;
            match received.frame.msg.take().unwrap() {
                Msg::SessionEvent(event) => {
                    assert_eq!(event.session_id, vec![13; 16]);
                    assert_eq!(event.reason, 0);
                    match (from_source, event.kind) {
                        (true, 1) => {
                            assert_eq!(event.local_udp_port, a);
                            source_started = true;
                        }
                        (false, 1) => {
                            assert_eq!(event.local_udp_port, b);
                            listener_started = true;
                        }
                        (true, 6) => {
                            assert_eq!(event.local_udp_port, 0);
                            source_path = true;
                        }
                        (false, 6) => {
                            assert_eq!(event.local_udp_port, 0);
                            listener_path = true;
                        }
                        // Transient media degradation is not a lifecycle transition.
                        // Still require both Started/PathValidated events and real sending.
                        (_, 2) => assert_eq!(event.local_udp_port, 0),
                        _ => panic!("unexpected session event: {}", event.kind),
                    }
                }
                Msg::Health(health) if from_source => {
                    assert_eq!(received.frame.request_id, 0);
                    sending |= health.sessions.iter().any(|session| {
                        session.session_id == vec![13; 16]
                            && session.capture_active == Some(true)
                            && session.sending == Some(true)
                            && session.sent_media_packets > 0
                    });
                }
                Msg::MediaSessions(_) => panic!("Source alone disclosed private media"),
                Msg::Error(error) => panic!("session wire error: {}", error.code),
                _ => {}
            }
        }
    })
    .await
    .unwrap();

    source
        .core
        .send(5, Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
        .await
        .unwrap();
    let first = snapshot(&mut source, &mut listener).await;
    assert_metadata(&first, 1, 2);
    assert!(first.revision > 0);
    source
        .core
        .send(
            6,
            Msg::MediaCommand(w::MediaCommand {
                token: 1,
                expected_revision: 1,
                command: 1,
                position_ms: None,
            }),
        )
        .await
        .unwrap();
    result(&mut source, &mut listener, 6, 1, Some(&script)).await;
    let played = snapshot(&mut source, &mut listener).await;
    assert_metadata(&played, 2, 1);
    assert!(played.revision > first.revision);
    source
        .core
        .send(
            7,
            Msg::MediaCommand(w::MediaCommand {
                token: 1,
                expected_revision: 99,
                command: 1,
                position_ms: None,
            }),
        )
        .await
        .unwrap();
    result(&mut source, &mut listener, 7, 2, Some(&script)).await;
    source
        .core
        .send(
            8,
            Msg::MediaCommand(w::MediaCommand {
                token: 1,
                expected_revision: 2,
                command: 4,
                position_ms: None,
            }),
        )
        .await
        .unwrap();
    result(&mut source, &mut listener, 8, 3, Some(&script)).await;
    // Drain single-shot pumps before admitting the deliberately pending command.
    quiet(&mut source, &mut listener, Duration::from_millis(200), true).await;

    // Failed native admission is a correlated failure result, never a fabricated success.
    for (request_id, error) in [
        (20, BackendError::PermissionDenied),
        (21, BackendError::ServerUnavailable),
    ] {
        let (ready, applied) = tokio::sync::oneshot::channel();
        script.send(Script::FailMedia(error, ready)).unwrap();
        tokio::time::timeout(Duration::from_secs(3), applied)
            .await
            .unwrap()
            .unwrap();
        source
            .core
            .send(
                request_id,
                Msg::MediaCommand(w::MediaCommand {
                    token: 1,
                    expected_revision: 2,
                    command: 1,
                    position_ms: None,
                }),
            )
            .await
            .unwrap();
        result(&mut source, &mut listener, request_id, 3, None).await;
    }

    source
        .core
        .send(
            9,
            Msg::MediaCommand(w::MediaCommand {
                token: 1,
                expected_revision: 2,
                command: 1,
                position_ms: None,
            }),
        )
        .await
        .unwrap();
    source
        .core
        .send(10, Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }))
        .await
        .unwrap();
    let mut disabled = None;
    let mut cancelled = false;
    while disabled.is_none() || !cancelled {
        let (from_source, mut received) = next_pair(&mut source, &mut listener, None).await;
        match received.frame.msg.take().unwrap() {
            Msg::MediaSessions(snapshot) => {
                assert!(from_source);
                assert_empty(&snapshot);
                assert!(snapshot.revision > first.revision);
                disabled = Some(snapshot);
            }
            Msg::MediaCommandResult(result) => {
                assert!(from_source);
                assert_eq!(received.frame.request_id, 9);
                assert_eq!(result.result, 5);
                cancelled = true;
            }
            Msg::Error(error) => panic!("unexpected wire error: {}", error.code),
            _ => {}
        }
    }
    let disabled = disabled.expect("metadata withdrawal observed");
    script
        .send(Script::Event(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(media(3)),
        }))
        .unwrap();
    quiet(
        &mut source,
        &mut listener,
        Duration::from_millis(300),
        false,
    )
    .await;

    source
        .core
        .send(11, Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
        .await
        .unwrap();
    let enabled = snapshot(&mut source, &mut listener).await;
    assert_metadata(&enabled, 3, 2);
    assert!(enabled.revision > disabled.revision);
    // Keep IPC and the Listener alive, but never renew the Source again.
    source.renew_ids.clear();
    let cleared = snapshot(&mut source, &mut listener).await;
    assert_empty(&cleared);
    assert!(cleared.revision > enabled.revision);
    script
        .send(Script::Event(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(media(4)),
        }))
        .unwrap();
    quiet(
        &mut source,
        &mut listener,
        Duration::from_millis(300),
        false,
    )
    .await;

    for fixture in [&mut source, &mut listener] {
        fixture
            .core
            .send(12, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
    }
    assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    assert_eq!(
        listener.handle.await.unwrap().unwrap(),
        ExitReason::Shutdown
    );
}
