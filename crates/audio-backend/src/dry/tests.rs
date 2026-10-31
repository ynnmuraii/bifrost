use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn format() -> AudioFormat {
    AudioFormat {
        sample_rate: TARGET_SAMPLE_RATE,
        channels: 2,
    }
}

fn id(value: &str) -> DeviceId {
    DeviceId::new(value).unwrap()
}

fn selection(value: &str) -> DeviceSelection {
    DeviceSelection::Explicit(id(value))
}

fn device(value: &str, direction: Direction) -> Device {
    Device {
        id: id(value),
        label: "Test endpoint".into(),
        direction,
        format: AudioFormat {
            sample_rate: 44_100,
            channels: 1,
        },
    }
}

fn backend() -> DryBackend {
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        exclude_own_process_tree: true,
        endpoint_mute: true,
        media: true,
        global_ptt: true,
    });
    backend
        .apply(ScriptEvent::UpsertDevice(device("mic", Direction::Capture)))
        .unwrap();
    backend
        .apply(ScriptEvent::UpsertDevice(device(
            "speaker",
            Direction::Render,
        )))
        .unwrap();
    backend
        .apply(ScriptEvent::SetDefaults(Defaults {
            multimedia: DefaultEndpoints {
                capture: Some(id("mic")),
                render: Some(id("speaker")),
            },
            communications: DefaultEndpoints {
                capture: Some(id("mic")),
                render: Some(id("speaker")),
            },
        }))
        .unwrap();
    drain(&mut backend);
    backend
}

fn drain(backend: &mut DryBackend) {
    while backend.poll_event().is_some() {}
}

fn capture_request() -> CaptureSpec {
    CaptureSpec::Microphone {
        endpoint: DeviceSelection::FollowDefault(DefaultRole::Communications),
        format: format(),
    }
}

fn render_request() -> RenderRequest {
    RenderRequest {
        device: DeviceSelection::FollowDefault(DefaultRole::Multimedia),
        requested: format(),
    }
}

fn info(token: u64, revision: u64) -> MediaInfo {
    MediaInfo::new(
        "private-app".into(),
        "private-title".into(),
        "private-subtitle".into(),
        MediaSessionToken(token),
        revision,
        PlaybackState::Paused,
        MediaCommands {
            play: true,
            pause: true,
            toggle: true,
            next: true,
            previous: true,
        },
    )
    .unwrap()
}

fn set_media(backend: &mut DryBackend, token: u64, revision: u64) {
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(token),
            info: Some(info(token, revision)),
        })
        .unwrap();
}

fn request(
    request_id: u64,
    token: u64,
    revision: u64,
    command: MediaCommand,
) -> MediaCommandRequest {
    MediaCommandRequest {
        request_id,
        session: MediaSessionToken(token),
        expected_revision: revision,
        command,
    }
}

#[test]
fn capture_is_chunk_independent_timestamped_and_uses_actual_native_format() {
    let mut backend = backend();
    let samples = Arc::new(Mutex::new(Vec::new()));
    let blocks = Arc::new(Mutex::new(Vec::new()));
    let output = samples.clone();
    let metadata = blocks.clone();
    let mut stream = backend
        .start_capture(
            capture_request(),
            Box::new(move |block| {
                output.lock().unwrap().extend_from_slice(block.samples);
                metadata
                    .lock()
                    .unwrap()
                    .push((block.frames, block.timestamp_ns, block.format));
                assert!(!format!("{block:?}").contains("samples"));
                assert!(!block.discontinuity);
            }),
        )
        .unwrap();
    assert_eq!(
        stream.format(),
        AudioFormat {
            sample_rate: 44_100,
            channels: 1
        }
    );
    stream.pump(441).unwrap();
    stream.pump(441).unwrap();
    let expected: Vec<_> = (0..882).map(|i| (i % 256) as f32 / 128.0 - 1.0).collect();
    assert_eq!(*samples.lock().unwrap(), expected);
    assert_eq!(
        *blocks.lock().unwrap(),
        vec![
            (441, 0, stream.format()),
            (441, 10_000_000, stream.format())
        ]
    );
    let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
    let generation = guard.snapshot().unwrap().generation;
    guard.set_capture_mute(generation, true).unwrap();
    stream.pump(3).unwrap();
    assert_eq!(&samples.lock().unwrap()[882..], &[0.0; 3]);
    assert_eq!(stream.processed_frames(), 885);
    assert_eq!(stream.pump(0), Err(BackendError::InvalidArgument));
    assert_eq!(
        stream.pump(MAX_PUMP_FRAMES + 1),
        Err(BackendError::InvalidArgument)
    );
}

#[test]
fn process_capture_has_no_endpoint_and_never_falls_back_or_accepts_wrong_format() {
    let mut backend = backend();
    let blocks = Arc::new(Mutex::new(Vec::new()));
    let output = blocks.clone();
    let mut stream = backend
        .start_capture(
            CaptureSpec::SystemExcludingOwnProcessTree { format: format() },
            Box::new(move |block| {
                output
                    .lock()
                    .unwrap()
                    .push((block.frames, block.samples.len(), block.timestamp_ns))
            }),
        )
        .unwrap();
    assert_eq!(stream.device(), None);
    assert_eq!(
        stream.feedback_guarantee(),
        FeedbackGuarantee::OwnProcessTreeExcluded
    );
    stream.pump(480).unwrap();
    stream.pump(480).unwrap();
    assert_eq!(
        *blocks.lock().unwrap(),
        vec![(480, 960, 0), (480, 960, 10_000_000)]
    );
    assert!(matches!(
        backend.start_capture(
            CaptureSpec::SystemExcludingOwnProcessTree {
                format: AudioFormat {
                    sample_rate: 44_100,
                    channels: 2
                },
            },
            Box::new(|_| {})
        ),
        Err(BackendError::InvalidArgument)
    ));
    backend.capabilities.exclude_own_process_tree = false;
    assert!(matches!(
        backend.start_capture(
            CaptureSpec::SystemExcludingOwnProcessTree { format: format() },
            Box::new(|_| {})
        ),
        Err(BackendError::Unsupported)
    ));
    for request in [
        CaptureSpec::EndpointLoopback {
            endpoint: selection("speaker"),
            format: format(),
        },
        CaptureSpec::Monitor {
            sink: selection("speaker"),
            format: format(),
        },
    ] {
        let stream = backend.start_capture(request, Box::new(|_| {})).unwrap();
        assert_eq!(stream.device(), Some(&id("speaker")));
        assert_eq!(stream.feedback_guarantee(), FeedbackGuarantee::None);
    }
}

#[test]
fn callbacks_execute_outside_control_locks_and_stop_and_drop_release_captures() {
    let mut backend = backend();
    let state = backend.state.clone();
    let held = Arc::new(());
    let weak = Arc::downgrade(&held);
    let mut stream = backend
        .start_render(
            render_request(),
            Box::new(move |pcm| {
                assert!(state.try_lock().is_ok());
                assert_eq!(Arc::strong_count(&held), 1);
                pcm.fill(0.25);
            }),
        )
        .unwrap();
    stream.pump(10).unwrap();
    assert!(weak.upgrade().is_some());
    stream.stop().unwrap();
    stream.stop().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(stream.pump(10), Err(BackendError::Stopped));
    let calls = Arc::new(AtomicUsize::new(0));
    let output = calls.clone();
    let stream = backend
        .start_capture(
            capture_request(),
            Box::new(move |_| {
                output.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .unwrap();
    drop(stream);
    assert_eq!(Arc::strong_count(&calls), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[test]
fn render_records_deterministic_bounded_samples_and_counts_all_frames() {
    let mut backend = backend();
    let mut value = 0usize;
    let mut stream = backend
        .start_render(
            render_request(),
            Box::new(move |pcm| {
                for sample in pcm {
                    *sample = value as f32;
                    value += 1;
                }
            }),
        )
        .unwrap();
    for _ in 0..20 {
        stream.pump(256).unwrap();
    }
    assert_eq!(stream.processed_frames(), 5120);
    let recorded: Vec<_> = stream.rendered_samples().collect();
    assert_eq!(recorded.len(), RENDER_SAMPLE_CAPACITY);
    assert_eq!(recorded[0], 1024.0);
    assert_eq!(recorded[RENDER_SAMPLE_CAPACITY - 1], 5119.0);
}

#[test]
fn role_defaults_are_distinct_and_open_streams_remain_pinned_until_recreated() {
    let mut backend = backend();
    backend
        .apply(ScriptEvent::UpsertDevice(device(
            "other",
            Direction::Render,
        )))
        .unwrap();
    let mut old = backend
        .start_render(render_request(), Box::new(|_| {}))
        .unwrap();
    let mut defaults = backend.defaults().unwrap();
    defaults.multimedia.render = Some(id("other"));
    backend.apply(ScriptEvent::SetDefaults(defaults)).unwrap();
    assert_eq!(old.device(), Some(&id("speaker")));
    let fresh = backend
        .start_render(render_request(), Box::new(|_| {}))
        .unwrap();
    assert_eq!(fresh.device(), Some(&id("other")));
    let communications = backend
        .start_render(
            RenderRequest {
                device: DeviceSelection::FollowDefault(DefaultRole::Communications),
                requested: format(),
            },
            Box::new(|_| {}),
        )
        .unwrap();
    assert_eq!(communications.device(), Some(&id("speaker")));
    backend
        .apply(ScriptEvent::RemoveDevice(id("speaker")))
        .unwrap();
    assert_eq!(old.pump(1), Err(BackendError::DeviceGone));
    assert_eq!(old.pump(1), Err(BackendError::Stopped));
    assert_eq!(backend.defaults().unwrap().communications.render, None);
    let mut events = Vec::new();
    while let Some(event) = backend.poll_event() {
        events.push(event);
    }
    assert!(events.contains(&Event::Device(DeviceEvent::DefaultChanged {
        direction: Direction::Render,
        role: DefaultRole::Multimedia,
    })));
    assert!(events.contains(&Event::Device(DeviceEvent::Removed(id("speaker")))));
}

#[test]
fn format_change_and_invalidation_stop_streams_without_clearing_physical_mute() {
    let mut backend = backend();
    let mut stream = backend
        .start_capture(capture_request(), Box::new(|_| {}))
        .unwrap();
    backend
        .apply(ScriptEvent::SetMute {
            device: id("mic"),
            muted: true,
        })
        .unwrap();
    let mut changed = device("mic", Direction::Capture);
    changed.format.channels = 2;
    backend.apply(ScriptEvent::UpsertDevice(changed)).unwrap();
    assert_eq!(stream.pump(1), Err(BackendError::DeviceGone));
    assert!(backend.capture_mute(&selection("mic")).unwrap().muted);
    let mut stream = backend
        .start_capture(capture_request(), Box::new(|_| {}))
        .unwrap();
    backend
        .apply(ScriptEvent::InvalidateDevice(id("mic")))
        .unwrap();
    assert_eq!(stream.pump(1), Err(BackendError::DeviceGone));
}

#[test]
fn mute_guard_restores_owned_changes_and_never_clears_preexisting_mute() {
    let mut backend = backend();
    {
        let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
        let generation = guard.snapshot().unwrap().generation;
        assert!(!guard.original_muted());
        assert!(matches!(
            backend.acquire_mute(&selection("mic")),
            Err(BackendError::Busy)
        ));
        assert_eq!(
            guard.set_capture_mute(generation + 1, true),
            Err(BackendError::StaleRevision)
        );
        guard.set_capture_mute(generation, true).unwrap();
        guard.set_capture_mute(generation, false).unwrap();
        guard.set_capture_mute(generation, true).unwrap();
    }
    assert!(!backend.capture_mute(&selection("mic")).unwrap().muted);
    backend
        .apply(ScriptEvent::SetMute {
            device: id("mic"),
            muted: true,
        })
        .unwrap();
    let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
    let generation = guard.snapshot().unwrap().generation;
    assert!(guard.original_muted());
    assert_eq!(
        guard.set_capture_mute(generation, false),
        Err(BackendError::PermissionDenied)
    );
    guard.restore().unwrap();
    assert!(guard.snapshot().unwrap().muted);
    assert_eq!(
        guard.set_capture_mute(generation, true),
        Err(BackendError::Stopped)
    );
}

#[test]
fn same_value_external_write_invalidates_ownership_and_old_endpoint_is_never_restored() {
    let mut backend = backend();
    let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
    let generation = guard.snapshot().unwrap().generation;
    guard.set_capture_mute(generation, true).unwrap();
    backend
        .apply(ScriptEvent::SetMute {
            device: id("mic"),
            muted: true,
        })
        .unwrap();
    assert_eq!(guard.restore(), Err(BackendError::OwnershipLost));
    assert!(backend.capture_mute(&selection("mic")).unwrap().muted);
    let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
    backend
        .apply(ScriptEvent::SetMute {
            device: id("mic"),
            muted: false,
        })
        .unwrap();
    assert_eq!(
        guard.set_capture_mute(generation, true),
        Err(BackendError::OwnershipLost)
    );
    drop(guard);
    assert!(!backend.capture_mute(&selection("mic")).unwrap().muted);
    let mut guard = backend.acquire_mute(&selection("mic")).unwrap();
    guard.set_capture_mute(generation, true).unwrap();
    backend.apply(ScriptEvent::RemoveDevice(id("mic"))).unwrap();
    backend
        .apply(ScriptEvent::UpsertDevice(device("mic", Direction::Capture)))
        .unwrap();
    backend
        .apply(ScriptEvent::SetMute {
            device: id("mic"),
            muted: true,
        })
        .unwrap();
    assert_eq!(guard.restore(), Err(BackendError::DeviceGone));
    assert!(backend.capture_mute(&selection("mic")).unwrap().muted);
}

#[test]
fn media_session_identity_is_distinct_from_app_id_and_commands_complete_asynchronously() {
    let mut backend = backend();
    assert!(backend.media_sessions().unwrap().is_empty());
    set_media(&mut backend, 1, 1);
    set_media(&mut backend, 2, 1);
    assert_eq!(backend.media_sessions().unwrap().len(), 2);
    drain(&mut backend);
    let stale = request(0, 1, 0, MediaCommand::Play);
    backend.media_command(stale).unwrap();
    assert_eq!(backend.poll_event(), None);
    backend.pump_control();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 0,
            result: Err(BackendError::StaleRevision),
        })
    );
    assert_eq!(backend.last_media_command(), None);
    for (revision, command, expected_state) in [
        (1, MediaCommand::Play, PlaybackState::Playing),
        (2, MediaCommand::Pause, PlaybackState::Paused),
        (3, MediaCommand::Toggle, PlaybackState::Playing),
        (4, MediaCommand::Next, PlaybackState::Playing),
        (5, MediaCommand::Previous, PlaybackState::Playing),
    ] {
        let command_request = request(revision, 1, revision, command);
        backend.media_command(command_request).unwrap();
        backend.pump_control();
        assert_eq!(
            backend.poll_event(),
            Some(Event::MediaCommandCompleted {
                request_id: revision,
                result: Ok(())
            })
        );
        assert_eq!(backend.last_media_command(), Some(command_request));
        let sessions = backend.media_sessions().unwrap();
        assert_eq!(sessions[0].revision, revision + 1);
        assert_eq!(sessions[0].state, expected_state);
        assert_eq!(sessions[1].revision, 1);
        assert_eq!(sessions[0].app(), "private-app");
        assert_eq!(sessions[0].title(), "private-title");
        assert_eq!(sessions[0].subtitle(), "private-subtitle");
        assert!(!format!("{:?}", sessions[0]).contains("private"));
        while let Some(event) = backend.poll_event() {
            assert!(!format!("{event:?}").contains("private"));
        }
    }
}

#[test]
fn media_requests_recheck_revision_and_session_at_execution_and_completions_cannot_overflow() {
    let mut backend = backend();
    set_media(&mut backend, 1, 1);
    backend
        .media_command(request(1, 1, 1, MediaCommand::Play))
        .unwrap();
    set_media(&mut backend, 1, 2);
    backend.pump_control();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 1,
            result: Err(BackendError::StaleRevision),
        })
    );
    drain(&mut backend);
    for request_id in 0..MEDIA_REQUEST_CAPACITY as u64 {
        backend
            .media_command(request(request_id, 1, 2, MediaCommand::Next))
            .unwrap();
    }
    assert_eq!(
        backend.media_command(request(99, 1, 2, MediaCommand::Play)),
        Err(BackendError::Busy)
    );
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: None,
        })
        .unwrap();
    backend.pump_control();
    assert_eq!(
        backend.media_command(request(99, 1, 2, MediaCommand::Play)),
        Err(BackendError::Busy)
    );
    for _ in 0..EVENT_CAPACITY + 5 {
        backend.apply(ScriptEvent::Availability(None)).unwrap();
    }
    assert_eq!(backend.poll_event(), Some(Event::ResyncRequired));
    for request_id in 0..MEDIA_REQUEST_CAPACITY as u64 {
        assert_eq!(
            backend.poll_event(),
            Some(Event::MediaCommandCompleted {
                request_id,
                result: Err(BackendError::DeviceGone),
            })
        );
    }
    assert_eq!(
        backend.apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(info(1, 3)),
        }),
        Err(BackendError::StaleRevision)
    );
}

#[test]
fn media_permissions_limits_and_debug_redaction_are_enforced() {
    let script = ScriptEvent::SetMedia {
        session: MediaSessionToken(1),
        info: Some(info(1, 1)),
    };
    assert!(!format!("{script:?}").contains("private"));
    let construct = |app, title, subtitle| {
        MediaInfo::new(
            app,
            title,
            subtitle,
            MediaSessionToken(1),
            0,
            PlaybackState::Stopped,
            MediaCommands::default(),
        )
    };
    assert!(construct("a".repeat(128), "t".repeat(256), "s".repeat(256)).is_ok());
    assert_eq!(
        construct("é".repeat(65), String::new(), String::new()),
        Err(BackendError::InvalidArgument)
    );
    assert_eq!(
        construct(String::new(), "t".repeat(257), String::new()),
        Err(BackendError::InvalidArgument)
    );
    assert_eq!(
        construct(String::new(), String::new(), "s".repeat(257)),
        Err(BackendError::InvalidArgument)
    );
    let mut backend = backend();
    let mut restricted = info(1, 1);
    restricted.commands.next = false;
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(restricted),
        })
        .unwrap();
    backend
        .media_command(request(1, 1, 1, MediaCommand::Next))
        .unwrap();
    backend.pump_control();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 1,
            result: Err(BackendError::Unsupported),
        })
    );
    for token in 2..=MEDIA_SESSION_CAPACITY as u64 {
        set_media(&mut backend, token, 1);
    }
    assert_eq!(
        backend.apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(99),
            info: Some(info(99, 1)),
        }),
        Err(BackendError::Busy)
    );
}

#[test]
fn device_limits_and_wrong_direction_are_rejected() {
    assert!(DeviceId::new("a".repeat(512)).is_ok());
    assert_eq!(
        DeviceId::new("a".repeat(513)),
        Err(BackendError::InvalidArgument)
    );
    assert_eq!(DeviceId::new(""), Err(BackendError::InvalidArgument));
    assert_eq!(
        DeviceId::new("invalid\0id"),
        Err(BackendError::InvalidArgument)
    );
    let mut invalid = device("invalid", Direction::Render);
    invalid.label = "x".repeat(257);
    let mut backend = backend();
    assert_eq!(
        backend.apply(ScriptEvent::UpsertDevice(invalid)),
        Err(BackendError::InvalidArgument)
    );
    assert!(matches!(
        backend.start_render(
            RenderRequest {
                device: selection("mic"),
                requested: format(),
            },
            Box::new(|_| {})
        ),
        Err(BackendError::InvalidArgument)
    ));
}

fn hotkey() -> PttHotkey {
    PttHotkey {
        key_code: 42,
        control: true,
        alt: false,
        shift: false,
        meta: false,
    }
}

#[test]
fn ptt_observes_release_unregisters_and_lock_session_timeout_fail_closed() {
    let mut backend = backend();
    assert_eq!(
        backend.apply(ScriptEvent::PttPress),
        Err(BackendError::Unsupported)
    );
    for reason in [
        PttCancellation::Locked,
        PttCancellation::SessionLost,
        PttCancellation::WorkerTimeout,
    ] {
        backend.configure_ptt(Some(hotkey()), &[]).unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        assert_eq!(
            backend.poll_event(),
            Some(Event::PttPressed {
                observed_at_ns: 0,
                expires_at_ns: PTT_LEASE_NS
            })
        );
        assert_eq!(backend.poll_event(), None);
        backend.apply(ScriptEvent::CancelPtt(reason)).unwrap();
        assert_eq!(
            backend.poll_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            })
        );
        assert_eq!(
            backend.apply(ScriptEvent::PttPress),
            Err(BackendError::Unsupported)
        );
    }
    backend.configure_ptt(Some(hotkey()), &[]).unwrap();
    backend.apply(ScriptEvent::PttPress).unwrap();
    backend.apply(ScriptEvent::PttRelease).unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttPressed {
            observed_at_ns: 0,
            expires_at_ns: PTT_LEASE_NS
        })
    );
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttReleased {
            reason: PttReleaseReason::PhysicalKeyUp,
        })
    );
    backend.apply(ScriptEvent::PttPress).unwrap();
    backend.configure_ptt(None, &[]).unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttPressed {
            observed_at_ns: 0,
            expires_at_ns: PTT_LEASE_NS
        })
    );
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttReleased {
            reason: PttReleaseReason::UnavailableOrReset,
        })
    );
}

#[test]
fn ptt_availability_changes_never_claim_physical_release() {
    for availability in [
        Some(BackendError::ServerUnavailable),
        Some(BackendError::PermissionDenied),
        None,
    ] {
        let mut backend = backend();
        backend.configure_ptt(Some(hotkey()), &[]).unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        drain(&mut backend);
        backend
            .apply(ScriptEvent::Availability(availability))
            .unwrap();
        assert_eq!(
            backend.poll_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::UnavailableOrReset,
            })
        );
        assert_eq!(backend.poll_event(), Some(Event::ResyncRequired));
        assert_eq!(backend.poll_event(), None);
    }
}

#[test]
fn physical_release_remains_observable_after_lease_expiry_or_overflow() {
    for overflow in [false, true] {
        let mut backend = backend();
        backend.configure_ptt(Some(hotkey()), &[]).unwrap();
        backend.apply(ScriptEvent::PttPress).unwrap();
        drain(&mut backend);
        if overflow {
            for _ in 0..=EVENT_CAPACITY {
                backend
                    .apply(ScriptEvent::SetMute {
                        device: id("mic"),
                        muted: false,
                    })
                    .unwrap();
            }
            assert_eq!(backend.poll_event(), Some(Event::ResyncRequired));
        } else {
            backend
                .apply(ScriptEvent::AdvanceTime {
                    now_ns: PTT_LEASE_NS,
                })
                .unwrap();
            assert_eq!(
                backend.poll_event(),
                Some(Event::PttReleased {
                    reason: PttReleaseReason::LeaseExpired,
                })
            );
        }
        drain(&mut backend);
        backend.apply(ScriptEvent::PttRelease).unwrap();
        assert_eq!(
            backend.poll_event(),
            Some(Event::PttReleased {
                reason: PttReleaseReason::PhysicalKeyUp,
            })
        );
        assert_eq!(backend.poll_event(), None);
    }
}

#[test]
fn bounded_poll_overflow_revokes_held_press_and_discards_stale_edges() {
    let mut backend = backend();
    backend.configure_ptt(Some(hotkey()), &[]).unwrap();
    backend.apply(ScriptEvent::PttPress).unwrap();
    for _ in 0..EVENT_CAPACITY + 10 {
        backend
            .apply(ScriptEvent::SetMute {
                device: id("mic"),
                muted: false,
            })
            .unwrap();
    }
    assert_eq!(backend.poll_event(), Some(Event::ResyncRequired));
    let mut count = 0;
    while let Some(event) = backend.poll_event() {
        assert!(!matches!(
            event,
            Event::PttPressed { .. } | Event::PttRenewed { .. } | Event::PttReleased { .. }
        ));
        count += 1;
    }
    assert!(count <= EVENT_CAPACITY);
    backend.apply(ScriptEvent::PttPress).unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttPressed {
            observed_at_ns: 0,
            expires_at_ns: PTT_LEASE_NS
        })
    );
}

#[test]
fn unsupported_and_server_permission_failures_have_structured_explicit_probe_results() {
    let mut unsupported = DryBackend::new(Capabilities::default());
    assert_eq!(unsupported.media_sessions(), Err(BackendError::Unsupported));
    assert_eq!(
        unsupported.configure_ptt(Some(hotkey()), &[]),
        Err(BackendError::Unsupported)
    );
    assert!(matches!(
        unsupported.acquire_mute(&selection("mic")),
        Err(BackendError::Unsupported)
    ));
    assert_eq!(
        unsupported.probe_capabilities().process_exclude,
        Availability::Unavailable(BackendError::Unsupported)
    );
    let mut backend = backend();
    assert_eq!(
        backend.probe_capabilities().ptt_release,
        Some(ReleaseObservation::Native)
    );
    assert_eq!(backend.probe_capabilities().os_build, None);
    for error in [
        BackendError::ServerUnavailable,
        BackendError::PermissionDenied,
    ] {
        let mut stream = backend
            .start_render(render_request(), Box::new(|_| {}))
            .unwrap();
        backend
            .apply(ScriptEvent::Availability(Some(error)))
            .unwrap();
        assert_eq!(backend.devices(), Err(error));
        assert_eq!(backend.defaults(), Err(error));
        assert_eq!(stream.pump(1), Err(error));
        assert_eq!(stream.pump(1), Err(BackendError::Stopped));
        assert_eq!(backend.media_sessions(), Err(error));
        assert_eq!(
            backend.probe_capabilities().process_exclude,
            Availability::Unavailable(error)
        );
        backend.apply(ScriptEvent::Availability(None)).unwrap();
    }
}

#[test]
fn media_timeout_and_cancellation_complete_once_without_executing() {
    let mut backend = backend();
    set_media(&mut backend, 1, 1);
    drain(&mut backend);
    backend
        .media_command(request(1, 1, 1, MediaCommand::Play))
        .unwrap();
    backend
        .apply(ScriptEvent::TimeoutMediaRequest { request_id: 1 })
        .unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 1,
            result: Err(BackendError::Timeout),
        })
    );
    assert_eq!(
        backend.apply(ScriptEvent::TimeoutMediaRequest { request_id: 1 }),
        Err(BackendError::InvalidArgument)
    );
    backend
        .media_command(request(2, 1, 1, MediaCommand::Play))
        .unwrap();
    backend.cancel_media_commands();
    backend.cancel_media_commands();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 2,
            result: Err(BackendError::Cancelled),
        })
    );
    backend
        .media_command(request(3, 1, 1, MediaCommand::Play))
        .unwrap();
    backend
        .apply(ScriptEvent::Availability(Some(
            BackendError::ServerUnavailable,
        )))
        .unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::MediaCommandCompleted {
            request_id: 3,
            result: Err(BackendError::ServerUnavailable),
        })
    );
    backend.apply(ScriptEvent::Availability(None)).unwrap();
    backend.pump_control();
    assert_eq!(backend.last_media_command(), None);
    let snapshot = &backend.media_sessions().unwrap()[0];
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.state, PlaybackState::Paused);
    assert_eq!(snapshot.position_ms, None);
    drain(&mut backend);
    assert_eq!(backend.poll_event(), None);
}

#[test]
fn media_navigation_preserves_unknown_timeline_and_resets_known_position() {
    let mut backend = backend();
    set_media(&mut backend, 1, 1);
    backend
        .media_command(request(1, 1, 1, MediaCommand::Next))
        .unwrap();
    backend.pump_control();
    assert_eq!(backend.media_sessions().unwrap()[0].position_ms, None);
    let mut known = info(1, 3);
    known.position_ms = Some(9000);
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(known),
        })
        .unwrap();
    backend
        .media_command(request(2, 1, 3, MediaCommand::Previous))
        .unwrap();
    backend.pump_control();
    assert_eq!(backend.media_sessions().unwrap()[0].position_ms, Some(0));
}

#[test]
fn ptt_binding_accepts_only_documented_non_modifier_hid_usage_ids() {
    let mut backend = backend();
    for key_code in [0, 3, 0x74, 0xe0, u32::MAX] {
        assert_eq!(
            backend.configure_ptt(
                Some(PttHotkey {
                    key_code,
                    ..hotkey()
                }),
                &[]
            ),
            Err(BackendError::InvalidArgument)
        );
    }
    for key_code in [0x04, 0x73] {
        backend
            .configure_ptt(
                Some(PttHotkey {
                    key_code,
                    ..hotkey()
                }),
                &[],
            )
            .unwrap();
    }
}

#[test]
fn format_bounds_are_checked_before_stream_scratch_allocation() {
    assert!(AudioFormat {
        sample_rate: MAX_SAMPLE_RATE,
        channels: MAX_CHANNELS
    }
    .validate()
    .is_ok());
    let mut backend = backend();
    for invalid in [
        AudioFormat {
            sample_rate: 0,
            channels: 2,
        },
        AudioFormat {
            sample_rate: MAX_SAMPLE_RATE + 1,
            channels: 2,
        },
        AudioFormat {
            sample_rate: 48_000,
            channels: 0,
        },
        AudioFormat {
            sample_rate: 48_000,
            channels: MAX_CHANNELS + 1,
        },
        AudioFormat {
            sample_rate: 48_000,
            channels: u16::MAX,
        },
    ] {
        assert_eq!(invalid.validate(), Err(BackendError::InvalidArgument));
        let mut endpoint = device("invalid", Direction::Render);
        endpoint.format = invalid;
        assert_eq!(
            backend.apply(ScriptEvent::UpsertDevice(endpoint)),
            Err(BackendError::InvalidArgument)
        );
        assert!(matches!(
            backend.start_render(
                RenderRequest {
                    device: selection("speaker"),
                    requested: invalid,
                },
                Box::new(|_| {})
            ),
            Err(BackendError::InvalidArgument)
        ));
    }
}

#[test]
fn stream_snapshots_report_native_clock_and_invalidation_without_pumping() {
    let mut backend = backend();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let output = captured.clone();
    let mut stream = backend
        .start_capture(
            capture_request(),
            Box::new(move |block| {
                output.lock().unwrap().push((
                    block.timestamp_valid,
                    block.device_position,
                    block.timestamp_ns,
                ));
            }),
        )
        .unwrap();
    assert_eq!(
        stream.snapshot(),
        StreamSnapshot {
            status: StreamStatus::Running,
            device_position: Some(0),
            clock_frequency: Some(44_100),
            host_time_100ns: Some(0),
            underrun_frames: 0,
            dropped_capture_frames: 0,
        }
    );
    stream.pump(441).unwrap();
    stream.pump(441).unwrap();
    assert_eq!(
        *captured.lock().unwrap(),
        [(true, Some(0), 0), (true, Some(441), 10_000_000)]
    );
    assert_eq!(stream.snapshot().device_position, Some(882));
    assert_eq!(stream.snapshot().host_time_100ns, Some(200_000));
    backend
        .apply(ScriptEvent::InvalidateDevice(id("mic")))
        .unwrap();
    assert_eq!(stream.snapshot().status, StreamStatus::DeviceInvalidated);
    assert_eq!(stream.pump(1), Err(BackendError::DeviceGone));
    assert_eq!(stream.snapshot().status, StreamStatus::DeviceInvalidated);
    let mut render = backend
        .start_render(render_request(), Box::new(|_| {}))
        .unwrap();
    render.stop().unwrap();
    assert_eq!(render.snapshot().status, StreamStatus::Stopped);
    let mut render = backend
        .start_render(render_request(), Box::new(|_| {}))
        .unwrap();
    backend
        .apply(ScriptEvent::Availability(Some(
            BackendError::ServerUnavailable,
        )))
        .unwrap();
    assert_eq!(
        render.snapshot().status,
        StreamStatus::Failed(BackendError::ServerUnavailable)
    );
    assert_eq!(render.pump(1), Err(BackendError::ServerUnavailable));
    assert_eq!(
        render.snapshot().status,
        StreamStatus::Failed(BackendError::ServerUnavailable)
    );
}

#[test]
fn ptt_collisions_preserve_the_existing_binding_and_press() {
    let mut backend = backend();
    let chord = hotkey();
    backend.configure_ptt(Some(chord), &[]).unwrap();
    backend.apply(ScriptEvent::PttPress).unwrap();
    drain(&mut backend);
    assert_eq!(
        backend.configure_ptt(Some(chord), &[chord]),
        Err(BackendError::Conflict)
    );
    assert_eq!(backend.poll_event(), None);
    backend.apply(ScriptEvent::PttRelease).unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttReleased {
            reason: PttReleaseReason::PhysicalKeyUp,
        })
    );
    let different = PttHotkey {
        alt: !chord.alt,
        ..chord
    };
    backend.configure_ptt(Some(different), &[chord]).unwrap();
    backend.configure_ptt(None, &[different]).unwrap();
}

#[test]
fn ptt_lease_renewal_expiry_and_stalled_consumer_fail_closed() {
    let mut backend = backend();
    backend.configure_ptt(Some(hotkey()), &[]).unwrap();
    backend
        .apply(ScriptEvent::AdvanceTime { now_ns: 10 })
        .unwrap();
    backend.apply(ScriptEvent::PttPress).unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttPressed {
            observed_at_ns: 10,
            expires_at_ns: 10 + PTT_LEASE_NS
        })
    );
    let renewed_at = 10 + PTT_LEASE_NS / 2;
    backend
        .apply(ScriptEvent::AdvanceTime { now_ns: renewed_at })
        .unwrap();
    backend.apply(ScriptEvent::PttRenew).unwrap();
    let expiry = renewed_at + PTT_LEASE_NS;
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttRenewed {
            expires_at_ns: expiry
        })
    );
    backend
        .apply(ScriptEvent::AdvanceTime { now_ns: expiry })
        .unwrap();
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttReleased {
            reason: PttReleaseReason::LeaseExpired,
        })
    );
    assert_eq!(
        backend.apply(ScriptEvent::PttRenew),
        Err(BackendError::Stopped)
    );
    // Even if Press was queued while the consumer stalled, its absolute expiry
    // prevents reopening when it is eventually consumed.
    backend.apply(ScriptEvent::PttPress).unwrap();
    let now = expiry + PTT_LEASE_NS;
    backend
        .apply(ScriptEvent::AdvanceTime { now_ns: now })
        .unwrap();
    let Some(Event::PttPressed { expires_at_ns, .. }) = backend.poll_event() else {
        panic!("queued press")
    };
    assert!(now >= expires_at_ns);
    assert_eq!(
        backend.poll_event(),
        Some(Event::PttReleased {
            reason: PttReleaseReason::LeaseExpired,
        })
    );
    assert_eq!(
        backend.apply(ScriptEvent::AdvanceTime { now_ns: now - 1 }),
        Err(BackendError::InvalidArgument)
    );
}

#[test]
fn current_media_session_is_explicit_and_cleared_when_removed() {
    let mut backend = backend();
    assert_eq!(backend.current_media_session(), Ok(None));
    set_media(&mut backend, 1, 1);
    set_media(&mut backend, 2, 1);
    assert_eq!(backend.current_media_session(), Ok(None));
    while backend.poll_event().is_some() {}
    assert_eq!(
        backend.apply(ScriptEvent::SetCurrentMediaSession {
            session: Some(MediaSessionToken(99)),
        }),
        Err(BackendError::DeviceGone)
    );
    assert_eq!(backend.current_media_session(), Ok(None));
    backend
        .apply(ScriptEvent::SetCurrentMediaSession {
            session: Some(MediaSessionToken(2)),
        })
        .unwrap();
    assert_eq!(
        backend.current_media_session(),
        Ok(Some(MediaSessionToken(2)))
    );
    assert!(matches!(
        backend.poll_event(),
        Some(Event::MediaChanged {
            session: MediaSessionToken(2),
            info: Some(_)
        })
    ));
    backend
        .apply(ScriptEvent::SetCurrentMediaSession { session: None })
        .unwrap();
    assert_eq!(backend.current_media_session(), Ok(None));
    assert_eq!(backend.media_sessions().unwrap().len(), 2);
    backend
        .apply(ScriptEvent::SetCurrentMediaSession {
            session: Some(MediaSessionToken(1)),
        })
        .unwrap();
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: None,
        })
        .unwrap();
    assert_eq!(backend.current_media_session(), Ok(None));
    assert_eq!(backend.media_sessions().unwrap().len(), 1);
}

#[test]
fn unknown_playback_is_preserved_and_current_queries_report_availability() {
    let mut backend = backend();
    let mut media = info(1, 1);
    media.state = PlaybackState::Unknown;
    backend
        .apply(ScriptEvent::SetMedia {
            session: MediaSessionToken(1),
            info: Some(media),
        })
        .unwrap();
    assert_eq!(
        backend.media_sessions().unwrap()[0].state,
        PlaybackState::Unknown
    );
    backend
        .apply(ScriptEvent::Availability(Some(
            BackendError::ServerUnavailable,
        )))
        .unwrap();
    assert_eq!(
        backend.current_media_session(),
        Err(BackendError::ServerUnavailable)
    );
    backend.apply(ScriptEvent::Availability(None)).unwrap();
    backend.capabilities.media = false;
    assert_eq!(
        backend.current_media_session(),
        Err(BackendError::Unsupported)
    );
    assert_eq!(
        backend.apply(ScriptEvent::SetCurrentMediaSession { session: None }),
        Err(BackendError::Unsupported)
    );
}
