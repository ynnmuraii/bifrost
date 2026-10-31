use super::scripted::{self, Script};
use super::*;

async fn response(fixture: &mut Fixture, request_id: u64) -> Msg {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut received = scripted::next_frame(fixture).await;
            let message = received.frame.msg.take().unwrap();
            if received.frame.request_id == request_id {
                return message;
            }
            assert_eq!(received.frame.request_id, 0, "uncorrelated response");
            assert!(matches!(
                message,
                Msg::Pong(_)
                    | Msg::Health(_)
                    | Msg::MuteSnapshot(_)
                    | Msg::SessionEvent(_)
                    | Msg::PttEvent(_)
            ));
        }
    })
    .await
    .unwrap()
}

async fn snapshot(fixture: &mut Fixture, request_id: u64) -> w::MuteSnapshot {
    let Msg::MuteSnapshot(snapshot) = response(fixture, request_id).await else {
        panic!("missing correlated mute snapshot")
    };
    assert_eq!(snapshot.endpoint_id, "DRY_MIC");
    assert!(snapshot.generation > 0);
    snapshot
}

async fn ptt(fixture: &mut Fixture, request_id: u64, kind: i32) -> w::PttEvent {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut received = scripted::next_frame(fixture).await;
            match received.frame.msg.take().unwrap() {
                Msg::PttEvent(event) => {
                    assert_eq!(received.frame.request_id, request_id);
                    assert_eq!(event.kind, kind);
                    if kind == 3 {
                        assert_eq!(event.observed_at_ns, 0);
                        assert_eq!(event.expires_at_ns, 0);
                    }
                    return event;
                }
                Msg::Pong(_) | Msg::Health(_) | Msg::MuteSnapshot(_) | Msg::SessionEvent(_) => {
                    assert_eq!(received.frame.request_id, 0);
                }
                other => panic!("unexpected PTT response: {other:?}"),
            }
        }
    })
    .await
    .unwrap()
}

fn advance(script: &std::sync::mpsc::Sender<Script>) -> u64 {
    let now_ns = clock::now_ns().expect("host monotonic clock");
    script
        .send(Script::Event(ScriptEvent::AdvanceTime { now_ns }))
        .unwrap();
    now_ns
}

fn acquire() -> Msg {
    Msg::AcquireMicMute(w::AcquireMicMute {
        selection: Some(w::DeviceSelection {
            selector: Some(w::device_selection::Selector::DeviceId("DRY_MIC".into())),
        }),
    })
}

#[tokio::test]
async fn m5_mute_ownership_and_absolute_ptt_leases_over_audio_link() {
    let (mut fixture, script) = scripted::child(0).await;
    fixture
        .core
        .send(2, Msg::ProbeBackend(w::ProbeBackend {}))
        .await
        .unwrap();
    let Msg::Ready(ready) = response(&mut fixture, 2).await else {
        panic!("missing correlated probe Ready")
    };
    assert!(ready.probed);
    assert_eq!(ready.backend, 2);
    for capability in [4, 5] {
        let available = ready
            .capabilities
            .iter()
            .find(|entry| entry.capability == capability)
            .unwrap();
        assert_eq!(available.availability, 1);
        assert_eq!(available.reason, 0);
    }

    fixture.core.send(10, acquire()).await.unwrap();
    let original = snapshot(&mut fixture, 10).await;
    assert!(original.guard_active);
    assert!(!original.muted);
    // Repeated acquisition must neither replace the guard nor prevent bounded reads.
    for request_id in 11..14 {
        fixture.core.send(request_id, acquire()).await.unwrap();
        let Msg::Error(error) = response(&mut fixture, request_id).await else {
            panic!("repeated acquisition must be Busy")
        };
        assert_eq!(error.code, 3);
    }
    fixture
        .core
        .send(
            14,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: original.generation,
                muted: true,
            }),
        )
        .await
        .unwrap();
    let muted = snapshot(&mut fixture, 14).await;
    assert_eq!(muted.generation, original.generation);
    assert!(muted.guard_active && muted.muted);
    fixture
        .core
        .send(15, Msg::ReleaseMicMute(w::ReleaseMicMute {}))
        .await
        .unwrap();
    let restored = snapshot(&mut fixture, 15).await;
    assert_eq!(restored.generation, original.generation);
    assert!(!restored.guard_active);
    assert_eq!(restored.muted, original.muted);

    fixture.core.send(16, acquire()).await.unwrap();
    let current = snapshot(&mut fixture, 16).await;
    fixture
        .core
        .send(
            17,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: current.generation + 1,
                muted: true,
            }),
        )
        .await
        .unwrap();
    let Msg::Error(error) = response(&mut fixture, 17).await else {
        panic!("stale generation write must fail")
    };
    assert_eq!(error.code, 6);
    // A rejected write may relinquish native ownership, but must not change the endpoint.
    fixture
        .core
        .send(18, Msg::ReleaseMicMute(w::ReleaseMicMute {}))
        .await
        .unwrap();
    match response(&mut fixture, 18).await {
        Msg::MuteSnapshot(snapshot) => {
            assert_eq!(snapshot.endpoint_id, "DRY_MIC");
            assert_eq!(snapshot.generation, current.generation);
            assert!(!snapshot.guard_active && !snapshot.muted);
        }
        Msg::Error(error) => assert_eq!(error.code, 6),
        other => panic!("unexpected stale-guard release response: {other:?}"),
    }
    fixture.core.send(19, acquire()).await.unwrap();
    let reacquired = snapshot(&mut fixture, 19).await;
    assert!(reacquired.guard_active && !reacquired.muted);
    assert_eq!(reacquired.generation, current.generation);
    fixture
        .core
        .send(20, Msg::ReleaseMicMute(w::ReleaseMicMute {}))
        .await
        .unwrap();
    assert!(!snapshot(&mut fixture, 20).await.guard_active);

    fixture
        .core
        .send(
            30,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: Some(w::PttHotkey {
                    key_code: 4,
                    control: true,
                    alt: false,
                    shift: true,
                    meta: false,
                }),
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 30, 3).await;
    // Earlier explicit mute fenced native holds. Reconfiguration is not proof of
    // key-up: the scripted backend must report an actual physical release first.
    script.send(Script::Event(ScriptEvent::PttRelease)).unwrap();
    assert_eq!(ptt(&mut fixture, 0, 3).await.reason, 8);

    let observed = advance(&script);
    script.send(Script::Event(ScriptEvent::PttPress)).unwrap();
    let pressed = ptt(&mut fixture, 0, 1).await;
    assert_eq!(pressed.observed_at_ns, observed);
    assert_eq!((pressed.grant_id, pressed.reason), (0, 0));
    assert!(pressed.expires_at_ns > observed);
    assert_eq!(
        pressed.expires_at_ns - observed,
        audio_backend::PTT_LEASE_NS
    );
    assert!(pressed.expires_at_ns > clock::now_ns().unwrap());

    tokio::time::sleep(Duration::from_millis(10)).await;
    let renewed_at = advance(&script);
    script.send(Script::Event(ScriptEvent::PttRenew)).unwrap();
    let renewed = ptt(&mut fixture, 0, 2).await;
    assert_eq!(renewed.observed_at_ns, 0);
    assert!(renewed.expires_at_ns > pressed.expires_at_ns);
    assert_eq!(
        renewed.expires_at_ns,
        renewed_at + audio_backend::PTT_LEASE_NS
    );
    fixture
        .core
        .send(31, Msg::PttReleaseAll(w::PttReleaseAll {}))
        .await
        .unwrap();
    assert!(matches!(
        response(&mut fixture, 31).await,
        Msg::PttEvent(w::PttEvent {
            grant_id: 0,
            kind: 3,
            reason: 2,
            ..
        })
    ));
    // The native key is still held. A queued renewal cannot resurrect its lease.
    script.send(Script::Event(ScriptEvent::PttRenew)).unwrap();
    script.send(Script::Event(ScriptEvent::PttRelease)).unwrap();
    assert_eq!(ptt(&mut fixture, 0, 3).await.reason, 8);

    let observed = advance(&script);
    script.send(Script::Event(ScriptEvent::PttPress)).unwrap();
    let pressed = ptt(&mut fixture, 0, 1).await;
    assert_eq!(pressed.observed_at_ns, observed);
    let stalled_at = tokio::time::Instant::now();
    script
        .send(Script::Stall(Duration::from_millis(350)))
        .unwrap();
    // Queue OS-aligned samples now: after the stall these native events are stale.
    advance(&script);
    script.send(Script::Event(ScriptEvent::PttRenew)).unwrap();
    script.send(Script::Event(ScriptEvent::PttRelease)).unwrap();
    advance(&script);
    script.send(Script::Event(ScriptEvent::PttPress)).unwrap();

    let deadline_ns = pressed.expires_at_ns + 50_000_000;
    let remaining = deadline_ns.saturating_sub(clock::now_ns().unwrap());
    let mut ping = tokio::time::interval(Duration::from_millis(20));
    let mut serial = 500;
    let mut pongs = 0;
    tokio::time::timeout(Duration::from_nanos(remaining), async {
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    fixture.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                    serial += 1;
                }
                received = fixture.core.recv() => {
                    let mut received = received.unwrap();
                    assert_eq!(received.frame.request_id, 0);
                    match received.frame.msg.take().unwrap() {
                        Msg::Pong(pong) => {
                            // Earlier next_frame helper pings may still be queued.
                            assert!(pong.serial == 1 || (500..serial).contains(&pong.serial));
                            if pong.serial >= 500 { pongs += 1; }
                        }
                        Msg::PttEvent(event) => {
                            assert_eq!(event.kind, 3, "blocked worker must not extend a lease");
                            assert_eq!(event.observed_at_ns, 0);
                            assert_eq!(event.expires_at_ns, 0);
                            break;
                        }
                        Msg::Health(_) | Msg::MuteSnapshot(_) => {}
                        other => panic!("unexpected blocked-worker response: {other:?}"),
                    }
                }
            }
        }
    })
    .await
    .expect("PTT release must arrive by absolute TTL + 50ms while native control is blocked");
    assert!(clock::now_ns().unwrap() <= deadline_ns);
    assert!(stalled_at.elapsed() < Duration::from_millis(350));
    assert!(
        pongs > 0,
        "IPC must remain responsive while native control is blocked"
    );

    // Continue past the blocked poll and drain both the delayed renewal and stale press.
    let quiet_until = stalled_at + Duration::from_millis(550);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(quiet_until) => break,
            _ = ping.tick() => {
                fixture.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                serial += 1;
            }
            received = fixture.core.recv() => {
                let mut received = received.unwrap();
                assert_eq!(received.frame.request_id, 0);
                match received.frame.msg.take().unwrap() {
                    Msg::PttEvent(event) => {
                        assert_eq!(event.kind, 3, "late renewal or stale press reopened an expired grant");
                        assert_eq!(event.observed_at_ns, 0);
                        assert_eq!(event.expires_at_ns, 0);
                    }
                    Msg::Pong(_) | Msg::Health(_) | Msg::MuteSnapshot(_) => {}
                    other => panic!("unexpected stale-event response: {other:?}"),
                }
            }
        }
    }

    fixture
        .core
        .send(
            31,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: None,
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 31, 3).await;

    // A separate media session remains authorized while mic-only grants expire.
    let mut listener = child().await;
    let (source_port, listener_port) = ports();
    listener
        .core
        .send(40, Msg::StartSession(start(2, listener_port, source_port)))
        .await
        .unwrap();
    session_event(&mut listener.core, 1, listener_port).await;
    fixture
        .core
        .send(40, Msg::StartSession(start(1, source_port, listener_port)))
        .await
        .unwrap();
    session_event(&mut fixture.core, 1, source_port).await;
    session_event(&mut fixture.core, 6, 0).await;
    session_event(&mut listener.core, 6, 0).await;

    fixture.core.send(41, acquire()).await.unwrap();
    let owned = snapshot(&mut fixture, 41).await;
    fixture
        .core
        .send(
            42,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: owned.generation,
                muted: true,
            }),
        )
        .await
        .unwrap();
    assert!(snapshot(&mut fixture, 42).await.muted);
    fixture
        .core
        .send(
            43,
            Msg::PttGrant(w::PttGrant {
                grant_id: 100,
                ttl_ms: 250,
            }),
        )
        .await
        .unwrap();
    let event = ptt(&mut fixture, 43, 1).await;
    assert_eq!((event.grant_id, event.reason), (100, 0));
    fixture
        .core
        .send(
            44,
            Msg::PttGrant(w::PttGrant {
                grant_id: 100,
                ttl_ms: 250,
            }),
        )
        .await
        .unwrap();
    assert_eq!(ptt(&mut fixture, 44, 2).await.grant_id, 100);
    fixture
        .core
        .send(45, Msg::PttRevoke(w::PttRevoke { grant_id: 99 }))
        .await
        .unwrap();
    let event = ptt(&mut fixture, 45, 5).await;
    assert_eq!((event.grant_id, event.reason), (99, 2));

    // Stall only generic native work; Ping and media authorization do not renew the mic grant.
    let stalled = tokio::time::Instant::now();
    script
        .send(Script::Stall(Duration::from_millis(400)))
        .unwrap();
    let before = listener.observed.pumps.load(Ordering::Relaxed);
    let mut ticks = tokio::time::interval(Duration::from_millis(20));
    tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    fixture.core.send(0, Msg::Ping(w::Ping { serial: 900 })).await.unwrap();
                    listener.core.send(0, Msg::Ping(w::Ping { serial: 900 })).await.unwrap();
                    renew(&mut listener.core, 13, 1000).await;
                    renew(&mut fixture.core, 13, 1000).await;
                }
                received = fixture.core.recv() => {
                    let mut received = received.unwrap();
                    if let Some(Msg::PttEvent(event)) = received.frame.msg.take() {
                        assert_eq!((received.frame.request_id, event.grant_id, event.kind, event.reason), (0, 100, 3, 1));
                        break;
                    }
                }
            }
        }
    }).await.expect("guard timer must remute before blocked backend returns");
    assert!(stalled.elapsed() < Duration::from_millis(400));
    fixture
        .core
        .send(
            54,
            Msg::PttGrant(w::PttGrant {
                grant_id: 104,
                ttl_ms: 20,
            }),
        )
        .await
        .unwrap();
    let stale = ptt(&mut fixture, 54, 3).await;
    assert_eq!(
        (stale.grant_id, stale.reason),
        (104, 1),
        "queued grant must not receive a fresh TTL"
    );
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(listener.observed.pumps.load(Ordering::Relaxed) > before);
    fixture
        .core
        .send(
            46,
            Msg::PttGrant(w::PttGrant {
                grant_id: 101,
                ttl_ms: 250,
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 46, 1).await;
    fixture
        .core
        .send(47, Msg::PttRevoke(w::PttRevoke { grant_id: 101 }))
        .await
        .unwrap();
    let event = ptt(&mut fixture, 47, 3).await;
    assert_eq!((event.grant_id, event.reason), (101, 2));
    fixture
        .core
        .send(
            48,
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: owned.generation,
                muted: false,
            }),
        )
        .await
        .unwrap();
    assert!(!snapshot(&mut fixture, 48).await.muted);
    fixture
        .core
        .send(
            49,
            Msg::PttGrant(w::PttGrant {
                grant_id: 102,
                ttl_ms: 250, // Exercise persistent-unmute policy, not an expired transit window.
            }),
        )
        .await
        .unwrap();
    let event = ptt(&mut fixture, 49, 5).await;
    assert_eq!((event.grant_id, event.reason), (102, 6));
    fixture
        .core
        .send(50, Msg::ReleaseMicMute(w::ReleaseMicMute {}))
        .await
        .unwrap();
    assert!(!snapshot(&mut fixture, 50).await.muted);
    script
        .send(Script::Event(ScriptEvent::SetMute {
            device: audio_backend::DeviceId::new("DRY_MIC").unwrap(),
            muted: true,
        }))
        .unwrap();
    loop {
        let frame = scripted::next_frame(&mut fixture).await;
        if matches!(
            frame.frame.msg,
            Some(Msg::MuteSnapshot(w::MuteSnapshot { muted: true, .. }))
        ) {
            break;
        }
    }
    fixture.core.send(51, acquire()).await.unwrap();
    assert!(snapshot(&mut fixture, 51).await.muted);
    fixture
        .core
        .send(
            52,
            Msg::PttGrant(w::PttGrant {
                grant_id: 103,
                ttl_ms: 100,
            }),
        )
        .await
        .unwrap();
    let event = ptt(&mut fixture, 52, 4).await;
    assert_eq!((event.grant_id, event.reason), (103, 5));
    listener
        .core
        .send(53, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    assert_eq!(
        listener.handle.await.unwrap().unwrap(),
        ExitReason::Shutdown
    );
    fixture
        .core
        .send(32, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), fixture.handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        ExitReason::Shutdown,
    );
}

/// Registers an unusual chord only; never acquires a microphone or synthesizes input.
#[cfg(windows)]
#[tokio::test]
async fn native_windows_child_registers_and_releases_ptt_without_input() {
    let mut fixture = child_with_backend(None, 1).await;
    let key = w::PttHotkey {
        key_code: 0x73, // HID F24: Ctrl+Alt+Shift+F24.
        control: true,
        alt: true,
        shift: true,
        meta: false,
    };
    fixture
        .core
        .send(
            2,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: Some(key),
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    // A busy/unsupported desktop fails honestly instead of replacing another registration.
    ptt(&mut fixture, 2, 3).await;
    let until = tokio::time::Instant::now() + Duration::from_millis(300);
    let mut ping = tokio::time::interval(Duration::from_millis(30));
    let mut pongs = 0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(until) => break,
            _ = ping.tick() => {
                fixture.core.send(0, Msg::Ping(w::Ping { serial: 73 })).await.unwrap();
            }
            received = fixture.core.recv() => {
                let received = received.unwrap().frame;
                assert_eq!(received.request_id, 0);
                match received.msg.unwrap() {
                    // scripted::next_frame sends this heartbeat while waiting for admission.
                    Msg::Pong(pong) if pong.serial == 1 => {}
                    Msg::Pong(pong) => {
                        assert_eq!(pong.serial, 73);
                        pongs += 1;
                    }
                    Msg::Health(_) => {}
                    other => panic!("registration without input invented an event: {other:?}"),
                }
            }
        }
    }
    assert!(pongs > 0);
    fixture
        .core
        .send(
            3,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: None,
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 3, 3).await;
    // Re-registration also proves that explicit cleanup freed the native chord.
    fixture
        .core
        .send(
            4,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: Some(key),
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 4, 3).await;
    fixture
        .core
        .send(
            5,
            Msg::ConfigurePtt(w::ConfigurePtt {
                binding: None,
                reserved: Vec::new(),
            }),
        )
        .await
        .unwrap();
    ptt(&mut fixture, 5, 3).await;
    fixture
        .core
        .send(6, Msg::Shutdown(w::Shutdown {}))
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), fixture.handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        ExitReason::Shutdown
    );
}
