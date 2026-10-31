use super::{
    scripted::{self, Script},
    *,
};

async fn correlated(fixture: &mut Fixture, id: u64) -> Msg {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let mut received = fixture.core.recv().await.unwrap();
            if received.frame.request_id == id {
                return received.frame.msg.take().unwrap();
            }
        }
    })
    .await
    .unwrap()
}
fn acquire() -> Msg {
    Msg::AcquireMicMute(w::AcquireMicMute {
        selection: Some(w::DeviceSelection {
            selector: Some(w::device_selection::Selector::DeviceId("DRY_MIC".into())),
        }),
    })
}
fn list() -> Msg {
    Msg::ListDevices(w::ListDevices {
        direction: 1,
        page: 0,
        inventory_revision: 0,
    })
}

// The backend handshake, not a sleep, proves generic control cannot drain the queue.
#[tokio::test]
async fn privacy_reductions_bypass_a_full_blocked_audio_link_queue() {
    for mode in 0..3 {
        let (mut fixture, script) = scripted::child(2).await;
        fixture.core.send(2, acquire()).await.unwrap();
        let Msg::MuteSnapshot(owned) = correlated(&mut fixture, 2).await else {
            panic!("acquisition missing")
        };
        if mode == 1 {
            fixture
                .core
                .send(
                    3,
                    Msg::SetMicMute(w::SetMicMute {
                        expected_generation: owned.generation,
                        muted: true,
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                correlated(&mut fixture, 3).await,
                Msg::MuteSnapshot(w::MuteSnapshot { muted: true, .. })
            ));
            fixture
                .core
                .send(
                    4,
                    Msg::PttGrant(w::PttGrant {
                        grant_id: 77,
                        ttl_ms: 250,
                    }),
                )
                .await
                .unwrap();
            assert!(matches!(
                correlated(&mut fixture, 4).await,
                Msg::PttEvent(w::PttEvent { kind: 1, .. })
            ));
            // A stale revoke cannot close the different current grant.
            fixture
                .core
                .send(5, Msg::PttRevoke(w::PttRevoke { grant_id: 76 }))
                .await
                .unwrap();
            assert!(matches!(
                correlated(&mut fixture, 5).await,
                Msg::PttEvent(w::PttEvent { kind: 5, .. })
            ));
        }
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        script.send(Script::Block(entered, resume_rx)).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while entered_rx.try_recv().is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        fixture
            .core
            .send(
                10,
                Msg::SetMicMute(w::SetMicMute {
                    expected_generation: owned.generation,
                    muted: false,
                }),
            )
            .await
            .unwrap();
        fixture
            .core
            .send(
                11,
                Msg::PttGrant(w::PttGrant {
                    grant_id: 77,
                    ttl_ms: 250,
                }),
            )
            .await
            .unwrap();
        fixture.core.send(12, acquire()).await.unwrap();
        for id in 13..26 {
            fixture.core.send(id, list()).await.unwrap();
        }
        fixture.core.send(90, list()).await.unwrap();
        assert!(matches!(
            correlated(&mut fixture, 90).await,
            Msg::Error(w::Error { code: 3, .. })
        ));
        let reduction = match mode {
            0 => Msg::SetMicMute(w::SetMicMute {
                expected_generation: owned.generation,
                muted: true,
            }),
            1 => Msg::PttRevoke(w::PttRevoke { grant_id: 77 }),
            _ => Msg::ReleaseMicMute(w::ReleaseMicMute {}),
        };
        fixture.core.send(91, reduction).await.unwrap();
        let response = correlated(&mut fixture, 91).await;
        match mode {
            0 => assert!(matches!(
                response,
                Msg::MuteSnapshot(w::MuteSnapshot {
                    muted: true,
                    guard_active: true,
                    ..
                })
            )),
            1 => assert!(matches!(
                response,
                Msg::PttEvent(w::PttEvent {
                    grant_id: 77,
                    kind: 3,
                    reason: 2,
                    ..
                })
            )),
            _ => assert!(matches!(
                response,
                Msg::MuteSnapshot(w::MuteSnapshot {
                    muted: false,
                    guard_active: false,
                    ..
                })
            )),
        }
        // Off publishes even when the generic queue rejects it and native control is still blocked.
        fixture
            .core
            .send(92, Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }))
            .await
            .unwrap();
        empty_metadata(&mut fixture).await;
        resume.send(()).unwrap();
        let mut rejected = [false; 3];
        tokio::time::timeout(Duration::from_secs(2), async {
            while !rejected.into_iter().all(|value| value) {
                let received = fixture.core.recv().await.unwrap();
                let id = received.frame.request_id;
                if (10..=12).contains(&id) {
                    assert!(matches!(
                        received.frame.msg,
                        Some(Msg::Error(_)) | Some(Msg::PttEvent(w::PttEvent { kind: 3..=5, .. }))
                    ));
                    rejected[(id - 10) as usize] = true;
                }
            }
        })
        .await
        .unwrap();
        fixture
            .core
            .send(100, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(fixture.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    }
    watched_metadata_is_closed_while_native_control_is_blocked().await;
}

async fn watched_metadata_is_closed_while_native_control_is_blocked() {
    let (mut source, script) = scripted::child(2).await;
    let mut listener = child().await;
    let (source_port, listener_port) = ports();
    listener
        .core
        .send(2, Msg::StartSession(start(2, listener_port, source_port)))
        .await
        .unwrap();
    session_event(&mut listener.core, 1, listener_port).await;
    source
        .core
        .send(2, Msg::StartSession(start(1, source_port, listener_port)))
        .await
        .unwrap();
    session_event(&mut source.core, 1, source_port).await;
    session_event(&mut source.core, 6, 0).await;
    session_event(&mut listener.core, 6, 0).await;
    script
        .send(Script::Event(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(
                MediaInfo::new(
                    "fixture".into(),
                    "Private title".into(),
                    "Private artist".into(),
                    MediaSessionToken(1),
                    1,
                    PlaybackState::Paused,
                    MediaCommands {
                        play: true,
                        ..MediaCommands::default()
                    },
                )
                .unwrap(),
            ),
        }))
        .unwrap();
    script
        .send(Script::Event(ScriptEvent::SetCurrentMediaSession {
            session: Some(MediaSessionToken(1)),
        }))
        .unwrap();
    source
        .core
        .send(3, Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Msg::MediaSessions(snapshot) = next(&mut source.core).await {
                if !snapshot.sessions.is_empty() {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    let (entered, entered_rx) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    script.send(Script::Block(entered, resume_rx)).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while entered_rx.try_recv().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source
        .core
        .send(
            10,
            Msg::MediaCommand(w::MediaCommand {
                token: 1,
                expected_revision: 1,
                command: 1,
            }),
        )
        .await
        .unwrap();
    for id in 11..26 {
        source.core.send(id, list()).await.unwrap();
    }
    source.core.send(90, list()).await.unwrap();
    assert!(matches!(
        correlated(&mut source, 90).await,
        Msg::Error(w::Error { code: 3, .. })
    ));
    source
        .core
        .send(91, Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }))
        .await
        .unwrap();
    empty_metadata(&mut source).await;
    assert!(matches!(
        correlated(&mut source, 91).await,
        Msg::Error(w::Error { code: 3, .. })
    ));
    resume.send(()).unwrap();
    assert!(matches!(
        correlated(&mut source, 10).await,
        Msg::MediaCommandResult(w::MediaCommandResult { result: 5 })
    ));
    // The still-authorized listener is independent of this source's metadata permission.
    listener
        .core
        .send(0, Msg::Ping(w::Ping { serial: 456 }))
        .await
        .unwrap();
    loop {
        if let Msg::Pong(pong) = next(&mut listener.core).await {
            assert_eq!(pong.serial, 456);
            break;
        }
    }
    source
        .core
        .send(100, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    listener
        .core
        .send(100, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    assert_eq!(
        listener.handle.await.unwrap().unwrap(),
        ExitReason::Shutdown
    );
}

async fn empty_metadata(fixture: &mut Fixture) {
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Msg::MediaSessions(snapshot) = next(&mut fixture.core).await {
                assert!(snapshot.sessions.is_empty());
                assert_eq!(snapshot.current_token, None);
                return;
            }
        }
    })
    .await
    .unwrap();
}
