use ipc::{
    audio_worker::{self as contract, AudioLink},
    Framed,
};
use prost::Message;
use protocol::audio_worker::{self as w, audio_frame::Msg};
fn attach() -> Msg {
    Msg::Attach(w::Attach {
        launch_token: vec![167; 32],
        backend: 2,
        core_silence_ms: 1000,
    })
}
fn ready() -> Msg {
    Msg::Ready(w::Ready {
        backend: 2,
        probed: false,
        capabilities: (1..=6)
            .map(|capability| w::CapabilityAvailability {
                capability,
                availability: 3,
                reason: 1,
            })
            .collect(),
    })
}
fn start() -> w::StartSession {
    w::StartSession {
        session_id: vec![3; 16],
        role: 1,
        profile: 1,
        capture: Some(w::CaptureSpec {
            kind: 1,
            device: None,
        }),
        render: None,
        local_bind_ip: "127.0.0.1".into(),
        local_udp_port: 40000,
        peer_ip: "127.0.0.1".into(),
        peer_udp_port: 40001,
        forward: Some(w::DirectionMaterial {
            key: vec![167; 32],
            nonce_prefix: vec![9; 4],
        }),
        reverse: Some(w::DirectionMaterial {
            key: vec![168; 32],
            nonce_prefix: vec![10; 4],
        }),
        local_scope_id: 0,
        peer_scope_id: 0,
        initial_authorization_ms: 1000,
    }
}
fn frame(message: Msg) -> w::AudioFrame {
    w::AudioFrame {
        major: contract::FRAME_MAJOR,
        minor: contract::FRAME_MINOR,
        seq: 1,
        worker_generation: 7,
        request_id: 1,
        msg: Some(message),
    }
}
#[test]
fn messages_and_nested_debug_never_expose_secrets() {
    for message in [attach(), Msg::StartSession(start())] {
        let text = format!("{:?}", frame(message));
        assert!(text.contains("REDACTED"));
        assert!(!text.contains(&format!("{:?}", vec![167u8; 32])));
    }
}
#[test]
fn start_validation_rejects_keys_addresses_roles_and_reserved_mic() {
    assert!(contract::validate_frame(&frame(Msg::StartSession(start()))).is_ok());
    for mutation in 0..8 {
        let mut request = start();
        match mutation {
            0 => request.forward.as_mut().unwrap().key.truncate(31),
            1 => request.reverse.as_mut().unwrap().nonce_prefix.push(1),
            2 => request.peer_ip = "example.invalid".into(),
            3 => request.peer_udp_port = 47902,
            4 => request.local_bind_ip = "0.0.0.0".into(),
            5 => request.role = 2,
            6 => request.capture.as_mut().unwrap().kind = 4,
            7 => request.local_scope_id = 1,
            _ => unreachable!(),
        }
        assert!(
            contract::validate_frame(&frame(Msg::StartSession(request))).is_err(),
            "mutation {mutation}"
        );
    }
}
#[test]
fn authorization_ttls_and_renew_session_ids_are_bounded() {
    for ttl_ms in [0, 1, 1000, 1001] {
        let valid = (1..=1000).contains(&ttl_ms);
        let mut request = start();
        request.initial_authorization_ms = ttl_ms;
        assert_eq!(
            contract::validate_frame(&frame(Msg::StartSession(request))).is_ok(),
            valid
        );
        assert_eq!(
            contract::validate_frame(&frame(Msg::RenewSession(w::RenewSession {
                session_id: vec![3; 16],
                ttl_ms,
            })))
            .is_ok(),
            valid
        );
    }
    for length in [0, 15, 17] {
        assert!(
            contract::validate_frame(&frame(Msg::RenewSession(w::RenewSession {
                session_id: vec![3; length],
                ttl_ms: 1000,
            })))
            .is_err()
        );
    }
    let mut renew = frame(Msg::RenewSession(w::RenewSession {
        session_id: vec![3; 16],
        ttl_ms: 1000,
    }));
    renew.request_id = 0;
    assert!(contract::validate_frame(&renew).is_err());
}

#[test]
fn started_ack_requires_native_port_without_implying_path_validation() {
    let mut event = frame(Msg::SessionEvent(w::SessionEvent {
        session_id: vec![3; 16],
        kind: w::EventKind::Started as i32,
        reason: 0,
        local_udp_port: 40000,
    }));
    event.request_id = 0;
    // Native initialization is acknowledged independently of peer/path readiness.
    assert!(contract::validate_frame(&event).is_ok());
    let Some(Msg::SessionEvent(payload)) = event.msg.as_mut() else {
        unreachable!();
    };
    payload.kind = w::EventKind::PathValidated as i32;
    payload.local_udp_port = 0;
    assert!(contract::validate_frame(&event).is_ok());
}

#[test]
fn session_event_kinds_reasons_and_ports_preserve_wire_contract() {
    assert_eq!(w::EventKind::Started as i32, 1);
    assert_eq!(w::EventKind::Degraded as i32, 2);
    assert_eq!(w::EventKind::DeviceInvalidated as i32, 3);
    assert_eq!(w::EventKind::RotationRequired as i32, 4);
    assert_eq!(w::EventKind::Stopped as i32, 5);
    assert_eq!(w::EventKind::PathValidated as i32, 6);
    assert_eq!(w::StopReason::AuthorizationLost as i32, 9);
    for kind in [-1, 0, 1, 2, 3, 4, 5, 6, 7, i32::MAX] {
        for reason in -1..=10 {
            for local_udp_port in [0, 40000, 47902, 65536] {
                let mut event = frame(Msg::SessionEvent(w::SessionEvent {
                    session_id: vec![3; 16],
                    kind,
                    reason,
                    local_udp_port,
                }));
                event.request_id = 0;
                let valid = (1..=6).contains(&kind)
                    && if kind == 5 {
                        (1..=9).contains(&reason)
                    } else {
                        reason == 0
                    }
                    && if kind == 1 {
                        local_udp_port == 40000
                    } else {
                        local_udp_port == 0
                    };
                assert_eq!(
                    contract::validate_frame(&event).is_ok(),
                    valid,
                    "kind={kind}, reason={reason}, port={local_udp_port}"
                );
            }
        }
    }
}

#[test]
fn stopped_authorization_lost_is_valid_but_unknown_reasons_are_not() {
    for reason in 0..=10 {
        let mut stopped = frame(Msg::SessionEvent(w::SessionEvent {
            session_id: vec![3; 16],
            kind: 5,
            reason,
            local_udp_port: 0,
        }));
        stopped.request_id = 0;
        assert_eq!(
            contract::validate_frame(&stopped).is_ok(),
            (1..=9).contains(&reason)
        );
    }
}

#[test]
fn renewal_uses_unreserved_tag_70() {
    let renew = w::RenewSession {
        session_id: vec![3; 16],
        ttl_ms: 1000,
    };
    let body = renew.encode_to_vec();
    let mut tagged = vec![0xb2, 0x04, body.len() as u8];
    tagged.extend(body);
    let encoded = frame(Msg::RenewSession(renew)).encode_to_vec();
    assert!(encoded.ends_with(&tagged));
}
#[test]
fn pages_health_and_generation_are_bounded() {
    let device = w::Device {
        id: "DEVICE".into(),
        name: "Output".into(),
        direction: 1,
        sample_rate: 48000,
        channels: 2,
        is_default: false,
    };
    let page = w::Devices {
        direction: 1,
        page: 0,
        inventory_revision: 1,
        devices: vec![device.clone()],
        complete: true,
    };
    assert!(contract::validate_frame(&frame(Msg::Devices(page.clone()))).is_ok());
    let mut duplicated = page;
    duplicated.devices.push(device);
    assert!(contract::validate_frame(&frame(Msg::Devices(duplicated))).is_err());
    let mut health = frame(Msg::Health(w::Health {
        sessions: vec![w::SessionHealth {
            session_id: vec![1; 16],
            counters: Some(w::AudioHealth::default()),
            state: 2,
            capture_active: Some(false),
            sending: Some(true),
            sent_media_packets: 1,
        }],
    }));
    health.request_id = 0;
    assert!(contract::validate_frame(&health).is_ok());
    health.worker_generation = 0;
    assert!(contract::validate_frame(&health).is_err());
}
#[tokio::test]
async fn link_roundtrip_extracts_single_use_secrets_and_binds_generation() {
    let (a, b) = tokio::io::duplex(65536);
    let mut core = AudioLink::core(a, 7).unwrap();
    let mut child = AudioLink::worker(b);
    core.send(1, attach()).await.unwrap();
    let received = child.recv().await.unwrap();
    assert_eq!(received.token.as_ref().unwrap().expose_secret(), &[167; 32]);
    assert!(format!("{received:?}").contains("REDACTED"));
    assert!(
        matches!(received.frame.msg.as_ref(),Some(Msg::Attach(attach)) if attach.launch_token.is_empty())
    );
    child.send(1, ready()).await.unwrap();
    core.recv().await.unwrap();
    core.send(2, Msg::StartSession(start())).await.unwrap();
    let mut received = child.recv().await.unwrap();
    assert!(
        matches!(received.frame.msg.as_ref(),Some(Msg::StartSession(start)) if start.forward.is_none() && start.reverse.is_none())
    );
    let material = received.secrets.take().unwrap();
    let (key, prefix) = material.forward.into_parts();
    assert_eq!(&*key, &[167; 32]);
    assert_eq!(&*prefix, &[9; 4]);
    assert_eq!(child.generation(), Some(7));
    assert!(child
        .send(0, Msg::Ping(w::Ping { serial: 1 }))
        .await
        .is_err());
    core.send(
        3,
        Msg::RenewSession(w::RenewSession {
            session_id: vec![3; 16],
            ttl_ms: 1000,
        }),
    )
    .await
    .unwrap();
    let received = child.recv().await.unwrap();
    assert!(received.secrets.is_none() && received.token.is_none());
    assert!(format!("{received:?}").contains("REDACTED"));
    assert!(
        matches!(received.frame.msg, Some(Msg::RenewSession(renew)) if renew.session_id == vec![3; 16] && renew.ttl_ms == 1000)
    );
    assert!(child
        .send(
            4,
            Msg::RenewSession(w::RenewSession {
                session_id: vec![3; 16],
                ttl_ms: 1000,
            }),
        )
        .await
        .is_err());
}
#[tokio::test]
async fn invalid_frames_do_not_advance_sequence() {
    let (a, b) = tokio::io::duplex(65536);
    let mut raw = Framed::ipc(a);
    let mut child = AudioLink::worker(b);
    raw.send(&frame(attach())).await.unwrap();
    child.recv().await.unwrap();
    let mut malformed = frame(Msg::StartSession(start()));
    malformed.seq = 2;
    if let Some(Msg::StartSession(start)) = malformed.msg.as_mut() {
        start.forward.as_mut().unwrap().key.clear();
    }
    raw.send(&malformed).await.unwrap();
    assert!(child.recv().await.is_err());
    assert_eq!(child.inbound_expected(), 2);
}
#[test]
fn launch_token_file_is_bounded_and_one_use() {
    let root = std::env::temp_dir().join(format!("bifrost-audio-token-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("one.token");
    std::fs::write(&path, [42; 32]).unwrap();
    let token = contract::take_launch_token(&path).unwrap();
    assert_eq!(token.expose_secret(), &[42; 32]);
    assert!(!path.exists());
    assert!(contract::take_launch_token(&path).is_err());
    std::fs::remove_dir(root).unwrap();
}
