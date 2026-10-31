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
            drift: None,
            clock_source: 0,
            latency: None,
            native_discontinuity_flags: 0,
            jitter_rebuffers: 2,
            render_underruns: 3,
            remote_receiver_quality: Some(w::RemoteReceiverQuality {
                received_packets: 3654,
                lost_frames: 131,
                late_frames: 0,
            }),
        }],
    }));
    health.request_id = 0;
    assert!(contract::validate_frame(&health).is_ok());
    health.worker_generation = 0;
    assert!(contract::validate_frame(&health).is_err());
}
#[test]
fn drift_health_validates_numeric_bounds_and_clock_codes() {
    let session = w::SessionHealth {
        session_id: vec![1; 16],
        counters: Some(w::AudioHealth::default()),
        state: 2,
        ..Default::default()
    };
    let valid = |session| {
        let mut message = frame(Msg::Health(w::Health {
            sessions: vec![session],
        }));
        message.request_id = 0;
        contract::validate_frame(&message).is_ok()
    };
    assert!(valid(session.clone()));
    for clock_source in 0..=4 {
        for (ratio_ppm, error) in [(-5000, -48_000.0), (5000, 48_000.0), (0, 0.0)] {
            let mut session = session.clone();
            session.clock_source = clock_source;
            session.drift = Some(w::DriftDiagnostics {
                ratio_ppm,
                filtered_fill_error_frames: error,
                coarse_corrections: u64::MAX,
            });
            assert!(valid(session));
        }
    }
    for clock_source in [-1, 5, i32::MAX] {
        let mut session = session.clone();
        session.clock_source = clock_source;
        assert!(!valid(session));
    }
    for (ratio_ppm, error) in [
        (-5001, 0.0),
        (5001, 0.0),
        (0, -48_000.1),
        (0, 48_000.1),
        (0, f64::NAN),
        (0, f64::INFINITY),
        (0, f64::NEG_INFINITY),
    ] {
        let mut session = session.clone();
        session.drift = Some(w::DriftDiagnostics {
            ratio_ppm,
            filtered_fill_error_frames: error,
            coarse_corrections: 0,
        });
        assert!(!valid(session));
    }
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

fn microphone_selection() -> w::DeviceSelection {
    w::DeviceSelection {
        selector: Some(w::device_selection::Selector::FollowDefault(2)),
    }
}

fn hotkey(key_code: u32) -> w::PttHotkey {
    w::PttHotkey {
        key_code,
        control: true,
        ..Default::default()
    }
}

fn media_info(token: u64) -> w::MediaInfo {
    w::MediaInfo {
        token,
        revision: 1,
        app_id: "player".into(),
        title: "PRIVATE_TRACK_TITLE".into(),
        subtitle: "PRIVATE_TRACK_ARTIST".into(),
        position_ms: None,
        duration_ms: None,
        state: 0,
        commands: 31,
    }
}

fn media_snapshot() -> w::MediaSessions {
    w::MediaSessions {
        sessions: vec![media_info(1)],
        current_token: Some(1),
        revision: 1,
    }
}

fn m5_m6_requests() -> Vec<Msg> {
    vec![
        Msg::AcquireMicMute(w::AcquireMicMute {
            selection: Some(microphone_selection()),
        }),
        Msg::SetMicMute(w::SetMicMute {
            expected_generation: 1,
            muted: true,
        }),
        Msg::ReleaseMicMute(w::ReleaseMicMute {}),
        Msg::ConfigurePtt(w::ConfigurePtt {
            binding: Some(hotkey(0x04)),
            reserved: vec![hotkey(0x05)],
        }),
        Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }),
        Msg::MediaCommand(w::MediaCommand {
            token: 1,
            expected_revision: 1,
            command: 1,
        }),
    ]
}

fn m5_m6_responses() -> Vec<Msg> {
    vec![
        Msg::MuteSnapshot(w::MuteSnapshot {
            endpoint_id: "MICROPHONE".into(),
            generation: 1,
            muted: true,
            guard_active: true,
        }),
        Msg::PttEvent(w::PttEvent {
            kind: 3,
            observed_at_ns: 0,
            expires_at_ns: 0,
            ..Default::default()
        }),
        Msg::MediaSessions(media_snapshot()),
        Msg::MediaCommandResult(w::MediaCommandResult { result: 1 }),
    ]
}

#[test]
fn m5_m6_exact_tags_and_outer_request_correlations() {
    let messages = m5_m6_requests().into_iter().chain(m5_m6_responses());
    for (message, tag) in messages.zip([18u32, 19, 20, 21, 30, 31, 47, 48, 60, 61]) {
        // A minimal envelope contains only the oneof field and its length-delimited body.
        let encoded = w::AudioFrame {
            msg: Some(message.clone()),
            ..Default::default()
        }
        .encode_to_vec();
        let key = (u32::from(encoded[0] & 0x7f)) | (u32::from(encoded[1]) << 7);
        assert_eq!(key, (tag << 3) | 2);
        for request_id in [0, 1, u64::MAX] {
            let mut candidate = frame(message.clone());
            candidate.request_id = request_id;
            let valid = match tag {
                47 | 48 => true,
                60 => request_id == 0,
                _ => request_id != 0,
            };
            assert_eq!(
                contract::validate_frame(&candidate).is_ok(),
                valid,
                "tag {tag}"
            );
        }
    }
}

#[test]
fn mute_selection_endpoint_and_generation_are_strict() {
    for selection in [
        None,
        Some(w::DeviceSelection { selector: None }),
        Some(w::DeviceSelection {
            selector: Some(w::device_selection::Selector::FollowDefault(0)),
        }),
        Some(w::DeviceSelection {
            selector: Some(w::device_selection::Selector::DeviceId(String::new())),
        }),
    ] {
        assert!(
            contract::validate_frame(&frame(Msg::AcquireMicMute(w::AcquireMicMute { selection })))
                .is_err()
        );
    }
    for generation in [0, 1, u64::MAX] {
        assert_eq!(
            contract::validate_frame(&frame(Msg::SetMicMute(w::SetMicMute {
                expected_generation: generation,
                muted: false,
            })))
            .is_ok(),
            generation != 0
        );
        for endpoint_id in [
            "".to_owned(),
            "é".repeat(256),
            "é".repeat(257),
            "mic\n".into(),
        ] {
            let valid = generation != 0 && endpoint_id.len() == 512;
            assert_eq!(
                contract::validate_frame(&frame(Msg::MuteSnapshot(w::MuteSnapshot {
                    endpoint_id,
                    generation,
                    muted: false,
                    guard_active: false,
                })))
                .is_ok(),
                valid
            );
        }
    }
}

#[test]
fn ptt_hotkeys_enforce_hid_bounds_uniqueness_and_collisions() {
    for key_code in [0, 3, 4, 0x73, 0x74, u32::MAX] {
        let request = w::ConfigurePtt {
            binding: Some(hotkey(key_code)),
            reserved: vec![],
        };
        assert_eq!(
            contract::validate_frame(&frame(Msg::ConfigurePtt(request))).is_ok(),
            (4..=0x73).contains(&key_code)
        );
    }
    for count in [0, 16, 17] {
        let request = w::ConfigurePtt {
            binding: None,
            reserved: (0..count).map(|index| hotkey(4 + index)).collect(),
        };
        assert_eq!(
            contract::validate_frame(&frame(Msg::ConfigurePtt(request))).is_ok(),
            count <= 16
        );
    }
    for request in [
        w::ConfigurePtt {
            binding: Some(hotkey(4)),
            reserved: vec![hotkey(4)],
        },
        w::ConfigurePtt {
            binding: None,
            reserved: vec![hotkey(4), hotkey(4)],
        },
        w::ConfigurePtt {
            binding: None,
            reserved: vec![hotkey(0)],
        },
    ] {
        assert!(contract::validate_frame(&frame(Msg::ConfigurePtt(request))).is_err());
    }
    let mut different_modifiers = hotkey(4);
    different_modifiers.shift = true;
    assert!(
        contract::validate_frame(&frame(Msg::ConfigurePtt(w::ConfigurePtt {
            binding: Some(hotkey(4)),
            reserved: vec![different_modifiers],
        })))
        .is_ok()
    );
}

#[test]
fn ptt_event_timestamps_do_not_wrap_or_extend_press_grants() {
    for kind in [-1, 0, 1, 2, 3, 4, i32::MAX] {
        for (observed_at_ns, expires_at_ns) in [
            (0, 0),
            (0, 1),
            (1, 1),
            (1, 2),
            (1, 250_000_001),
            (1, 250_000_002),
            (u64::MAX, 1),
            (u64::MAX - 1, u64::MAX),
        ] {
            let valid = match kind {
                1 => {
                    expires_at_ns > observed_at_ns && expires_at_ns - observed_at_ns <= 250_000_000
                }
                2 => observed_at_ns == 0 && expires_at_ns > 0,
                3 => observed_at_ns == 0 && expires_at_ns == 0,
                _ => false,
            };
            assert_eq!(
                contract::validate_frame(&frame(Msg::PttEvent(w::PttEvent {
                    kind,
                    observed_at_ns,
                    expires_at_ns,
                    ..Default::default()
                })))
                .is_ok(),
                valid,
                "{kind}/{observed_at_ns}/{expires_at_ns}"
            );
        }
    }
}

#[test]
fn remote_talk_grants_are_bounded_and_distinct_from_physical_observations() {
    for grant_id in [0, 1, u64::MAX] {
        for ttl_ms in [0, 1, 250, 251, u32::MAX] {
            assert_eq!(
                contract::validate_frame(&frame(Msg::PttGrant(w::PttGrant { grant_id, ttl_ms })))
                    .is_ok(),
                grant_id != 0 && (1..=250).contains(&ttl_ms)
            );
        }
        assert_eq!(
            contract::validate_frame(&frame(Msg::PttRevoke(w::PttRevoke { grant_id }))).is_ok(),
            grant_id != 0
        );
    }
    for (kind, reason, valid) in [
        (3, 1, true),
        (4, 5, true),
        (5, 6, true),
        (3, 0, false),
        (4, 6, false),
        (5, 5, false),
    ] {
        assert_eq!(
            contract::validate_frame(&frame(Msg::PttEvent(w::PttEvent {
                kind,
                grant_id: 7,
                reason,
                ..Default::default()
            })))
            .is_ok(),
            valid
        );
    }
}

fn valid_media(snapshot: w::MediaSessions) -> bool {
    let mut candidate = frame(Msg::MediaSessions(snapshot));
    candidate.request_id = 0;
    contract::validate_frame(&candidate).is_ok()
}

#[test]
fn media_snapshot_bounds_tokens_revisions_and_optional_times() {
    for count in [0, 1, 32, 33] {
        assert_eq!(
            valid_media(w::MediaSessions {
                sessions: (1..=count).map(media_info).collect(),
                current_token: None,
                revision: 1,
            }),
            count <= 32
        );
    }
    for mutation in 0..8 {
        let mut snapshot = media_snapshot();
        match mutation {
            0 => snapshot.revision = 0,
            1 => snapshot.sessions[0].revision = 0,
            2 => snapshot.sessions[0].token = 0,
            3 => snapshot.sessions.push(snapshot.sessions[0].clone()),
            4 => snapshot.current_token = Some(0),
            5 => snapshot.current_token = Some(2),
            6 => snapshot.sessions[0].commands = 32,
            7 => snapshot.sessions[0].state = 4,
            _ => unreachable!(),
        }
        assert!(!valid_media(snapshot), "mutation {mutation}");
    }
    for state in [-1, 0, 1, 2, 3, 4, i32::MAX] {
        for commands in [0, 1, 31, 32, u32::MAX] {
            for (position_ms, duration_ms) in [
                (None, None),
                (Some(0), None),
                (None, Some(0)),
                (Some(u64::MAX), Some(1)),
                (Some(0), Some(u64::MAX)),
            ] {
                let mut snapshot = media_snapshot();
                let session = &mut snapshot.sessions[0];
                session.state = state;
                session.commands = commands;
                session.position_ms = position_ms;
                session.duration_ms = duration_ms;
                assert_eq!(
                    valid_media(snapshot),
                    (0..=3).contains(&state) && commands <= 31
                );
            }
        }
    }
}

#[test]
fn media_metadata_limits_count_utf8_bytes_and_reject_controls() {
    for field in 0..3 {
        let max = if field == 0 { 128 } else { 256 };
        for value in [
            String::new(),
            "é".repeat(max / 2),
            "é".repeat(max / 2 + 1),
            "private\n".into(),
        ] {
            let valid = value.len() <= max && !value.chars().any(char::is_control);
            let mut snapshot = media_snapshot();
            let session = &mut snapshot.sessions[0];
            match field {
                0 => session.app_id = value,
                1 => session.title = value,
                2 => session.subtitle = value,
                _ => unreachable!(),
            }
            assert_eq!(valid_media(snapshot), valid, "field {field}");
        }
    }
}

#[test]
fn media_command_enums_tokens_and_revisions_are_closed() {
    for command in [-1, 0, 1, 2, 3, 4, 5, 6, i32::MAX] {
        for token in [0, 1, u64::MAX] {
            for expected_revision in [0, 1, u64::MAX] {
                assert_eq!(
                    contract::validate_frame(&frame(Msg::MediaCommand(w::MediaCommand {
                        token,
                        expected_revision,
                        command,
                    })))
                    .is_ok(),
                    token != 0 && expected_revision != 0 && (1..=5).contains(&command)
                );
            }
        }
    }
    for result in [-1, 0, 1, 2, 3, 4, 5, 6, i32::MAX] {
        assert_eq!(
            contract::validate_frame(&frame(Msg::MediaCommandResult(w::MediaCommandResult {
                result,
            })))
            .is_ok(),
            (1..=5).contains(&result)
        );
    }
}

#[test]
fn raw_nested_and_received_media_debug_redact_metadata() {
    let snapshot = media_snapshot();
    let received = contract::Received {
        frame: frame(Msg::MediaSessions(snapshot.clone())),
        secrets: None,
        token: None,
    };
    for text in [
        format!("{:?}", snapshot.sessions[0]),
        format!("{snapshot:?}"),
        format!("{:?}", received.frame),
        format!("{received:?}"),
    ] {
        assert!(text.contains("REDACTED"));
        assert!(!text.contains("PRIVATE_TRACK_TITLE"));
        assert!(!text.contains("PRIVATE_TRACK_ARTIST"));
    }
}

#[tokio::test]
async fn m5_m6_link_roundtrips_enforce_direction_and_redact_metadata() {
    let (a, b) = tokio::io::duplex(65536);
    let mut core = AudioLink::core(a, 7).unwrap();
    let mut child = AudioLink::worker(b);
    core.send(1, attach()).await.unwrap();
    child.recv().await.unwrap();
    child.send(1, ready()).await.unwrap();
    core.recv().await.unwrap();
    for message in m5_m6_requests() {
        assert!(child.send(2, message.clone()).await.is_err());
        core.send(2, message.clone()).await.unwrap();
        assert_eq!(child.recv().await.unwrap().frame.msg, Some(message));
    }
    for message in m5_m6_responses() {
        let id = if matches!(&message, Msg::MediaSessions(_)) {
            0
        } else {
            2
        };
        assert!(core.send(id, message.clone()).await.is_err());
        child.send(id, message.clone()).await.unwrap();
        let received = core.recv().await.unwrap();
        assert!(format!("{received:?}").contains("REDACTED"));
        assert_eq!(received.frame.msg, Some(message));
    }
}

#[test]
fn maximum_media_snapshot_remains_within_private_frame_limit() {
    let mut snapshot = w::MediaSessions {
        sessions: (1..=32).map(media_info).collect(),
        current_token: Some(32),
        revision: u64::MAX,
    };
    for session in &mut snapshot.sessions {
        session.app_id = "a".repeat(128);
        session.title = "t".repeat(256);
        session.subtitle = "s".repeat(256);
        session.revision = u64::MAX;
        session.position_ms = Some(u64::MAX);
        session.duration_ms = Some(u64::MAX);
    }
    let mut candidate = frame(Msg::MediaSessions(snapshot));
    candidate.request_id = 0;
    assert!(candidate.encoded_len() <= 64 * 1024);
    assert!(contract::validate_frame(&candidate).is_ok());
}

#[tokio::test]
async fn wrong_direction_m5_m6_receives_do_not_advance_sequence() {
    for message in m5_m6_requests() {
        let (a, b) = tokio::io::duplex(65536);
        let mut raw = Framed::ipc(a);
        let mut core = AudioLink::core(b, 7).unwrap();
        raw.send(&frame(ready())).await.unwrap();
        core.recv().await.unwrap();
        let mut wrong_direction = frame(message);
        wrong_direction.seq = 2;
        raw.send(&wrong_direction).await.unwrap();
        assert!(core.recv().await.is_err());
        assert_eq!(core.inbound_expected(), 2);
    }
    for message in m5_m6_responses() {
        let (a, b) = tokio::io::duplex(65536);
        let mut raw = Framed::ipc(a);
        let mut child = AudioLink::worker(b);
        raw.send(&frame(attach())).await.unwrap();
        child.recv().await.unwrap();
        let mut wrong_direction = frame(message);
        wrong_direction.seq = 2;
        if matches!(&wrong_direction.msg, Some(Msg::MediaSessions(_))) {
            wrong_direction.request_id = 0;
        }
        raw.send(&wrong_direction).await.unwrap();
        assert!(child.recv().await.is_err());
        assert_eq!(child.inbound_expected(), 2);
    }
}

#[test]
fn acquire_mute_stable_ids_and_default_roles_follow_existing_selection_bounds() {
    for device_id in [
        "m".to_owned(),
        "é".repeat(256),
        "é".repeat(257),
        "mic\0".into(),
    ] {
        let valid = device_id.len() <= 512 && !device_id.chars().any(char::is_control);
        assert_eq!(
            contract::validate_frame(&frame(Msg::AcquireMicMute(w::AcquireMicMute {
                selection: Some(w::DeviceSelection {
                    selector: Some(w::device_selection::Selector::DeviceId(device_id)),
                }),
            })))
            .is_ok(),
            valid
        );
    }
    for role in [-1, 0, 1, 2, 3, i32::MAX] {
        assert_eq!(
            contract::validate_frame(&frame(Msg::AcquireMicMute(w::AcquireMicMute {
                selection: Some(w::DeviceSelection {
                    selector: Some(w::device_selection::Selector::FollowDefault(role)),
                }),
            })))
            .is_ok(),
            matches!(role, 1 | 2)
        );
    }
}

#[test]
fn latency_health_bounds_each_observed_stage_and_preserves_unknown_native_depth() {
    let valid = |latency| {
        let mut frame = frame(Msg::Health(w::Health {
            sessions: vec![w::SessionHealth {
                session_id: vec![1; 16],
                counters: Some(w::AudioHealth::default()),
                state: 2,
                latency: Some(latency),
                ..Default::default()
            }],
        }));
        frame.request_id = 0;
        contract::validate_frame(&frame).is_ok()
    };
    assert!(valid(w::LatencyDiagnostics::default()));
    assert!(valid(w::LatencyDiagnostics {
        packetization_us: 2_000_000,
        capture_queue_us: 2_000_000,
        codec_lookahead_us: 2_000_000,
        jitter_target_us: 2_000_000,
        jitter_buffer_us: 2_000_000,
        decoded_queue_us: 2_000_000,
        render_queue_us: 2_000_000,
        resampler_delay_us: 2_000_000,
        native_render_buffer_us: Some(2_000_000),
    }));
    for index in 0..9 {
        let mut latency = w::LatencyDiagnostics::default();
        match index {
            0 => latency.packetization_us = 2_000_001,
            1 => latency.capture_queue_us = 2_000_001,
            2 => latency.codec_lookahead_us = 2_000_001,
            3 => latency.jitter_target_us = 2_000_001,
            4 => latency.jitter_buffer_us = 2_000_001,
            5 => latency.decoded_queue_us = 2_000_001,
            6 => latency.render_queue_us = 2_000_001,
            7 => latency.resampler_delay_us = 2_000_001,
            8 => latency.native_render_buffer_us = Some(2_000_001),
            _ => unreachable!(),
        }
        assert!(!valid(latency), "unbounded stage {index}");
    }
}

#[test]
fn restart_required_is_an_append_only_bounded_private_error() {
    for code in [-1, 0, 1, 8, 9, 10, i32::MAX] {
        assert_eq!(
            contract::validate_frame(&frame(Msg::Error(w::Error {
                code,
                session_id: vec![1; 16],
            })))
            .is_ok(),
            (1..=9).contains(&code),
        );
    }
    assert_eq!(w::ErrorCode::RestartRequired as i32, 9);
}
