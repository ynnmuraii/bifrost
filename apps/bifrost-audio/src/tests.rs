use super::*;

#[test]
fn explicit_dry_media_fixture_is_revisioned_and_commandable() {
    let mut backend = dry_backend().unwrap();
    let token = MediaSessionToken(1);
    assert_eq!(backend.current_media_session().unwrap(), Some(token));
    let initial = backend.media_sessions().unwrap().remove(0);
    assert_eq!(initial.session, token);
    assert_eq!(initial.revision, 1);
    assert!(!initial.title().is_empty());
    assert_eq!(initial.state, PlaybackState::Paused);
    for command in [
        MediaCommand::Play,
        MediaCommand::Pause,
        MediaCommand::Toggle,
        MediaCommand::Next,
        MediaCommand::Previous,
    ] {
        assert!(initial.commands.contains(command));
    }
    for (id, command, expected) in [
        (1, MediaCommand::Play, PlaybackState::Playing),
        (2, MediaCommand::Pause, PlaybackState::Paused),
        (3, MediaCommand::Toggle, PlaybackState::Playing),
    ] {
        backend
            .media_command(MediaCommandRequest {
                request_id: id,
                session: token,
                expected_revision: id,
                command,
            })
            .unwrap();
        backend.pump_control();
        let current = backend.media_sessions().unwrap().remove(0);
        assert_eq!(current.session, token);
        assert_eq!(current.revision, id + 1);
        assert_eq!(current.state, expected);
    }
}

#[derive(Default)]
pub(super) struct Observed {
    samples: AtomicU64,
    rms: AtomicU64,
    tone: AtomicU64,
    pumps: AtomicU64,
}
impl Observed {
    pub(super) fn observe(&self, stream: &DryStream) {
        self.pumps.fetch_add(1, Ordering::Relaxed);
        let (mut energy, mut samples, mut real, mut imaginary) = (0.0, 0, 0.0, 0.0);
        for (index, sample) in stream.rendered_samples().enumerate() {
            assert!(sample.is_finite());
            energy += f64::from(sample).powi(2);
            samples += 1;
            if index % 2 == 0 {
                let angle = std::f64::consts::TAU * 375.0 * (index / 2) as f64 / 48000.0;
                real += f64::from(sample) * angle.cos();
                imaginary += f64::from(sample) * angle.sin();
            }
        }
        if samples > 0 {
            self.samples.fetch_add(samples, Ordering::Relaxed);
            self.rms.store(
                (energy / samples as f64).sqrt().to_bits(),
                Ordering::Relaxed,
            );
            self.tone.store(
                (4.0 * real.hypot(imaginary) / samples as f64).to_bits(),
                Ordering::Relaxed,
            );
        }
    }
    fn rms(&self) -> f64 {
        f64::from_bits(self.rms.load(Ordering::Relaxed))
    }
}
#[test]
fn arguments_reject_secrets_and_implicit_backend_switches() {
    assert!(parse(vec!["--dry".into()]).is_err());
    assert!(parse(vec!["--token".into(), "secret".into()]).is_err());
    assert!(parse(vec![]).is_err());
}
#[cfg(any(unix, windows))]
mod local {
    mod lifecycle;
    mod m5;
    mod m6;
    mod privacy;
    mod rotation;
    mod scripted;
    use super::*;
    use ipc::{
        peer::SameUser,
        transport::{self, Endpoint},
    };
    use std::net::UdpSocket;
    fn test_endpoint(_directory: &tempfile::TempDir) -> Endpoint {
        #[cfg(unix)]
        {
            Endpoint::socket(_directory.path().join("audio.sock"))
        }
        #[cfg(windows)]
        {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            Endpoint::pipe(format!(
                r"\\.\pipe\bifrost-audio-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        }
    }
    struct Fixture {
        core: AudioLink<transport::ClientStream>,
        handle: tokio::task::JoinHandle<Result<ExitReason, RoleError>>,
        observed: Arc<Observed>,
        renew_ids: Vec<u8>,
        _directory: tempfile::TempDir,
    }
    async fn child() -> Fixture {
        child_with_factory(None).await
    }
    #[tokio::test]
    async fn dry_capture_pump_keeps_realtime_cadence() {
        let mut fixture = child().await;
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local = ports().0;
        fixture
            .core
            .send(
                2,
                Msg::StartSession(start(
                    1,
                    local,
                    u32::from(peer.local_addr().unwrap().port()),
                )),
            )
            .await
            .unwrap();
        session_event(&mut fixture.core, 1, local).await;
        let first = fixture.observed.pumps.load(Ordering::Relaxed);
        let began = Instant::now();
        let measurement = tokio::time::sleep(Duration::from_millis(1200));
        tokio::pin!(measurement);
        let mut heartbeat = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = &mut measurement => break,
                _ = heartbeat.tick() => {
                    fixture.core.send(0, Msg::Ping(w::Ping { serial: 1 })).await.unwrap();
                    renew(&mut fixture.core, 13, 1000).await;
                }
                received = fixture.core.recv() => {
                    match received.unwrap().frame.msg.unwrap() {
                        Msg::Error(error) => panic!("cadence fixture rejected: {}", error.code),
                        Msg::SessionEvent(event) if event.kind == 5 => panic!("capture stopped early: {}", event.reason),
                        _ => {}
                    }
                }
            }
        }
        let seconds = began.elapsed().as_secs_f64();
        let callbacks = fixture.observed.pumps.load(Ordering::Relaxed) - first;
        let fps = callbacks as f64 / seconds;
        println!("Dry capture cadence on {}: {callbacks} callbacks / {seconds:.4}s = {fps:.3} fps (480-frame/10ms blocks)", std::env::consts::OS);
        assert!(
            (95.0..=105.0).contains(&fps),
            "Dry callback clock must track real time: {fps:.3}fps"
        );
        fixture
            .core
            .send(3, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(fixture.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    }

    async fn child_with_factory(
        factory: Option<fn() -> Result<DryBackend, BackendError>>,
    ) -> Fixture {
        child_with_backend(factory, 2).await
    }
    async fn child_with_backend(
        factory: Option<fn() -> Result<DryBackend, BackendError>>,
        backend: i32,
    ) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = test_endpoint(&directory);
        let mut listener = transport::listen(&endpoint).unwrap();
        let observed = Arc::new(Observed::default());
        let child_observed = observed.clone();
        let child_endpoint = endpoint.clone();
        let handle = tokio::spawn(async move {
            let io = transport::connect(&child_endpoint).await.unwrap();
            run_connection_options(
                io,
                LaunchToken::new([71; 32].into()),
                Options {
                    dry_factory: factory,
                    observed: Some(child_observed),
                },
            )
            .await
        });
        let io = listener.accept(&SameUser::current()).await.unwrap();
        let mut core = AudioLink::core(io, 9).unwrap();
        core.send(
            1,
            Msg::Attach(w::Attach {
                launch_token: vec![71; 32],
                backend,
                core_silence_ms: 1000,
            }),
        )
        .await
        .unwrap();
        let ready = next(&mut core).await;
        let Msg::Ready(ready) = ready else {
            panic!("ready missing")
        };
        assert!(!ready.probed);
        assert!(ready
            .capabilities
            .iter()
            .all(|capability| capability.reason == 1));
        Fixture {
            core,
            handle,
            observed,
            renew_ids: Vec::new(),
            _directory: directory,
        }
    }
    async fn next(core: &mut AudioLink<transport::ClientStream>) -> Msg {
        tokio::time::timeout(Duration::from_secs(3), core.recv())
            .await
            .unwrap()
            .unwrap()
            .frame
            .msg
            .take()
            .unwrap()
    }
    fn ports() -> (u32, u32) {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        (
            u32::from(a.local_addr().unwrap().port()),
            u32::from(b.local_addr().unwrap().port()),
        )
    }
    fn start(role: i32, local: u32, peer: u32) -> w::StartSession {
        w::StartSession {
            session_id: vec![13; 16],
            role,
            profile: 1,
            capture: if role == 1 {
                Some(w::CaptureSpec {
                    kind: 1,
                    device: None,
                })
            } else {
                None
            },
            render: if role == 2 {
                Some(w::DeviceSelection {
                    selector: Some(w::device_selection::Selector::FollowDefault(1)),
                })
            } else {
                None
            },
            local_bind_ip: "127.0.0.1".into(),
            local_udp_port: local,
            peer_ip: "127.0.0.1".into(),
            peer_udp_port: peer,
            forward: Some(w::DirectionMaterial {
                key: vec![21; 32],
                nonce_prefix: vec![31; 4],
            }),
            reverse: Some(w::DirectionMaterial {
                key: vec![22; 32],
                nonce_prefix: vec![32; 4],
            }),
            local_scope_id: 0,
            peer_scope_id: 0,
            initial_authorization_ms: 1000,
            source_local_sound_released: false,
        }
    }
    async fn renew(core: &mut AudioLink<transport::ClientStream>, id: u8, ttl_ms: u32) {
        core.send(
            100,
            Msg::RenewSession(w::RenewSession {
                session_id: vec![id; 16],
                ttl_ms,
            }),
        )
        .await
        .unwrap();
    }
    async fn session_event(
        core: &mut AudioLink<transport::ClientStream>,
        kind: i32,
        port: u32,
    ) -> w::SessionEvent {
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut ping = tokio::time::interval(Duration::from_millis(100));
            let mut serial = 1;
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                        renew(core, 13, 1000).await;
                        serial += 1;
                    }
                    frame = core.recv() => {
                        let received = frame.unwrap().frame;
                        match received.msg.unwrap() {
                            Msg::SessionEvent(event) => {
                                assert_eq!(event.session_id, vec![13; 16]);
                                assert_eq!(event.kind, kind);
                                assert_eq!(event.local_udp_port, port);
                                return event;
                            }
                            Msg::Error(error) if kind == 5 && received.request_id == 100
                                && error.code == 6 && error.session_id == vec![13; 16] => {
                                // An automatic renewal can race terminal path failure.
                            }
                            Msg::Error(error) => panic!("session error: {}", error.code),
                            _ => {}
                        }
                    }
                }
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn listener_native_started_precedes_peer_and_path_validation() {
        let mut listener = child().await;
        let (a, b) = ports();
        listener
            .core
            .send(2, Msg::StartSession(start(2, b, a)))
            .await
            .unwrap();
        assert_eq!(session_event(&mut listener.core, 1, b).await.reason, 0);
        let absent_peer = UdpSocket::bind(("127.0.0.1", a as u16)).unwrap();
        let mut ping = tokio::time::interval(Duration::from_millis(50));
        let quiet = tokio::time::sleep(Duration::from_millis(300));
        tokio::pin!(quiet);
        loop {
            tokio::select! {
                _ = &mut quiet => break,
                _ = ping.tick() => {
                    listener.core.send(0, Msg::Ping(w::Ping { serial: 1 })).await.unwrap();
                    renew(&mut listener.core, 13, 1000).await;
                }
                frame = listener.core.recv() => {
                    match frame.unwrap().frame.msg.unwrap() {
                        Msg::SessionEvent(event) => panic!("event before source start: {}", event.kind),
                        Msg::Error(error) => panic!("listener error: {}", error.code),
                        _ => {}
                    }
                }
            }
        }
        drop(absent_peer);
        let mut source = child().await;
        source
            .core
            .send(2, Msg::StartSession(start(1, a, b)))
            .await
            .unwrap();
        assert_eq!(session_event(&mut source.core, 1, a).await.reason, 0);
        assert_eq!(session_event(&mut source.core, 6, 0).await.reason, 0);
        assert_eq!(session_event(&mut listener.core, 6, 0).await.reason, 0);
        for fixture in [&mut source, &mut listener] {
            fixture
                .core
                .send(3, Msg::Shutdown(w::Shutdown {}))
                .await
                .unwrap();
        }
        assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    #[tokio::test]
    async fn listener_without_peer_stops_for_path_timeout_while_lease_renews() {
        let mut listener = child().await;
        let (a, b) = ports();
        let began = Instant::now();
        listener
            .core
            .send(2, Msg::StartSession(start(2, b, a)))
            .await
            .unwrap();
        assert_eq!(session_event(&mut listener.core, 1, b).await.reason, 0);
        let stopped = session_event(&mut listener.core, 5, 0).await;
        assert_eq!(stopped.reason, 5);
        assert_eq!(
            stopped.path_diagnostic,
            w::PathDiagnostic::NoAuthenticatedPeerPackets as i32
        );
        assert!(began.elapsed() >= Duration::from_secs(2));
        listener
            .core
            .send(3, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    async fn drive(
        source: &mut Fixture,
        listener: &mut Fixture,
        duration: Duration,
    ) -> (bool, bool) {
        let deadline = Instant::now() + duration;
        let (mut source_active, mut listener_received) = (false, false);
        let mut serial = 1;
        let mut ping = tokio::time::interval(Duration::from_millis(100));
        while Instant::now() < deadline {
            tokio::select! {
                _=ping.tick()=>{
                    source.core.send(0,Msg::Ping(w::Ping {serial})).await.unwrap();
                    listener.core.send(0,Msg::Ping(w::Ping {serial})).await.unwrap();
                    renew(&mut source.core,13,1000).await;
                    renew(&mut listener.core,13,1000).await;
                    serial+=1;
                },
                frame=source.core.recv()=>{if let Some(Msg::Health(health))=frame.unwrap().frame.msg {
                    for session in &health.sessions {
                        assert!(session.drift.is_none());
                        if session.capture_active == Some(true) {
                            assert!(matches!(session.clock_source, 1 | 2));
                        }
                        if session.sending == Some(true) {
                            assert!(session.path.as_ref().unwrap().validated);
                        }
                    }
                    source_active|=health.sessions.iter().any(|session|session.capture_active==Some(true)&&session.sending==Some(true)&&session.sent_media_packets>0);
                }},
                frame=listener.core.recv()=>{if let Some(Msg::Health(health))=frame.unwrap().frame.msg {
                    for session in &health.sessions {
                        if session.counters.as_ref().unwrap().received_packets > 0 {
                            assert!(session.path.as_ref().unwrap().validated);
                        }
                    }
                    listener_received|=health.sessions.iter().any(|session|
                        session.counters.as_ref().unwrap().received_packets>0
                            && session.capture_active==Some(false)
                            && session.drift.is_some()
                            && matches!(session.clock_source, 3 | 4));
                }},
            }
        }
        (source_active, listener_received)
    }
    #[tokio::test]
    async fn two_children_real_local_ipc_udp_observations_mute_stop_and_one_use() {
        let mut source = child().await;
        let mut listener = child().await;
        let (a, b) = ports();
        source
            .core
            .send(2, Msg::ProbeBackend(w::ProbeBackend {}))
            .await
            .unwrap();
        loop {
            if let Msg::Ready(ready) = next(&mut source.core).await {
                assert!(ready.probed);
                assert_eq!(ready.backend, 2);
                break;
            }
        }
        source
            .core
            .send(
                3,
                Msg::ListDevices(w::ListDevices {
                    direction: 1,
                    page: 0,
                    inventory_revision: 0,
                }),
            )
            .await
            .unwrap();
        loop {
            if let Msg::Devices(devices) = next(&mut source.core).await {
                assert_eq!(devices.devices.len(), 1);
                assert!(devices.complete);
                break;
            }
        }
        listener
            .core
            .send(4, Msg::StartSession(start(2, b, a)))
            .await
            .unwrap();
        assert_eq!(session_event(&mut listener.core, 1, b).await.reason, 0);
        source
            .core
            .send(4, Msg::StartSession(start(1, a, b)))
            .await
            .unwrap();
        assert_eq!(session_event(&mut source.core, 1, a).await.reason, 0);
        assert_eq!(session_event(&mut source.core, 6, 0).await.reason, 0);
        assert_eq!(session_event(&mut listener.core, 6, 0).await.reason, 0);
        let (active, received) =
            drive(&mut source, &mut listener, Duration::from_millis(2300)).await;
        assert!(active && received);
        assert!(listener.observed.samples.load(Ordering::Relaxed) > 48000);
        assert!(listener.observed.rms() > 0.1);
        assert!(f64::from_bits(listener.observed.tone.load(Ordering::Relaxed)) > 0.15);
        listener
            .core
            .send(
                5,
                Msg::SetGain(w::SetGain {
                    session_id: vec![13; 16],
                    gain_percent: 100,
                    muted: true,
                }),
            )
            .await
            .unwrap();
        drive(&mut source, &mut listener, Duration::from_millis(600)).await;
        assert!(listener.observed.rms() < 1e-5);
        source
            .core
            .send(
                6,
                Msg::StopSession(w::StopSession {
                    session_id: vec![13; 16],
                }),
            )
            .await
            .unwrap();
        loop {
            if let Msg::SessionEvent(event) = next(&mut source.core).await {
                if event.kind == 5 {
                    assert_eq!(event.reason, 1);
                    break;
                }
            }
        }
        source
            .core
            .send(7, Msg::StartSession(start(1, a, b)))
            .await
            .unwrap();
        loop {
            if let Msg::Error(error) = next(&mut source.core).await {
                assert_eq!(error.code, 4);
                break;
            }
        }
        let mut reused = start(1, a, b);
        reused.session_id = vec![14; 16];
        source
            .core
            .send(8, Msg::StartSession(reused))
            .await
            .unwrap();
        loop {
            if let Msg::Error(error) = next(&mut source.core).await {
                assert_eq!(error.code, 5);
                break;
            }
        }
        source
            .core
            .send(9, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        listener
            .core
            .send(9, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    async fn drive_many(
        sources: &mut [Fixture],
        listener: &mut Fixture,
        duration: Duration,
    ) -> w::Health {
        let deadline = Instant::now() + duration;
        let mut health = w::Health::default();
        let mut ping = tokio::time::interval(Duration::from_millis(100));
        let mut serial = 1;
        while Instant::now() < deadline {
            tokio::select! {
                _ = ping.tick() => {
                    for source in sources.iter_mut() {
                        source.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                        for id in source.renew_ids.clone() {
                            renew(&mut source.core, id, 1000).await;
                        }
                    }
                    listener.core.send(0, Msg::Ping(w::Ping { serial })).await.unwrap();
                    for id in listener.renew_ids.clone() {
                        renew(&mut listener.core, id, 1000).await;
                    }
                    serial += 1;
                }
                frame = listener.core.recv() => {
                    match frame.unwrap().frame.msg {
                        Some(Msg::Health(current)) => health = current,
                        Some(Msg::Error(error)) => panic!("listener error: {}", error.code),
                        _ => {}
                    }
                }
            }
        }
        health
    }
    #[tokio::test]
    async fn listener_child_mixes_three_independent_sessions_until_each_stops() {
        let mut listener = child().await;
        let mut sources = Vec::new();
        let (_, b) = ports();
        for index in 0..3u8 {
            let source = child().await;
            let (a, _) = ports();
            let setup = |role, local, peer| {
                let mut session = start(role, local, peer);
                session.session_id = vec![40 + index; 16];
                session.profile = if index == 1 { 2 } else { 1 };
                session.forward.as_mut().unwrap().key = vec![60 + index; 32];
                session.reverse.as_mut().unwrap().key = vec![70 + index; 32];
                session.forward.as_mut().unwrap().nonce_prefix = vec![80 + index; 4];
                session.reverse.as_mut().unwrap().nonce_prefix = vec![90 + index; 4];
                session
            };
            listener
                .core
                .send(2, Msg::StartSession(setup(2, b, a)))
                .await
                .unwrap();
            let mut source = source;
            source
                .core
                .send(2, Msg::StartSession(setup(1, a, b)))
                .await
                .unwrap();
            source.renew_ids.push(40 + index);
            listener.renew_ids.push(40 + index);
            sources.push(source);
        }
        let health = drive_many(&mut sources, &mut listener, Duration::from_secs(3)).await;
        assert_eq!(health.sessions.len(), 3);
        assert!(health.sessions.iter().all(|session| session
            .counters
            .as_ref()
            .unwrap()
            .received_packets
            > 10));
        for session in &health.sessions {
            let drift = session
                .drift
                .as_ref()
                .expect("shared playout diagnostics reach child Health");
            assert!((-1000..=1000).contains(&drift.ratio_ppm));
            assert!(drift.filtered_fill_error_frames.is_finite());
            assert!(matches!(session.clock_source, 3 | 4));
        }
        assert!(listener.observed.rms() > 0.1);
        listener
            .core
            .send(
                3,
                Msg::SetGain(w::SetGain {
                    session_id: vec![41; 16],
                    gain_percent: 25,
                    muted: true,
                }),
            )
            .await
            .unwrap();
        listener
            .core
            .send(
                4,
                Msg::StopSession(w::StopSession {
                    session_id: vec![40; 16],
                }),
            )
            .await
            .unwrap();
        listener.renew_ids.retain(|id| *id != 40);
        let remaining = drive_many(&mut sources, &mut listener, Duration::from_millis(1200)).await;
        assert_eq!(remaining.sessions.len(), 2);
        for session in &remaining.sessions {
            let before = health
                .sessions
                .iter()
                .find(|before| before.session_id == session.session_id)
                .unwrap();
            assert!(
                session.counters.as_ref().unwrap().received_packets
                    > before.counters.as_ref().unwrap().received_packets
            );
        }
        assert!(listener.observed.rms() > 0.05);
        for id in [41, 42] {
            listener
                .core
                .send(
                    5,
                    Msg::StopSession(w::StopSession {
                        session_id: vec![id; 16],
                    }),
                )
                .await
                .unwrap();
        }
        listener.renew_ids.clear();
        drive_many(&mut sources, &mut listener, Duration::from_millis(200)).await;
        let stopped_pumps = listener.observed.pumps.load(Ordering::Relaxed);
        drive_many(&mut sources, &mut listener, Duration::from_millis(150)).await;
        assert_eq!(
            listener.observed.pumps.load(Ordering::Relaxed),
            stopped_pumps
        );
        for source in &mut sources {
            source
                .core
                .send(6, Msg::Shutdown(w::Shutdown {}))
                .await
                .unwrap();
        }
        listener
            .core
            .send(6, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        for source in sources {
            assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
        }
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    #[tokio::test]
    async fn source_child_shares_one_socket_for_three_sessions() {
        let mut listener = child().await;
        let mut sources = vec![child().await];
        let (a, b) = ports();
        for index in 0..3u8 {
            for (role, local, peer) in [(2, b, a), (1, a, b)] {
                let mut session = start(role, local, peer);
                session.session_id = vec![100 + index; 16];
                session.forward.as_mut().unwrap().key = vec![110 + index; 32];
                session.reverse.as_mut().unwrap().key = vec![120 + index; 32];
                session.forward.as_mut().unwrap().nonce_prefix = vec![130 + index; 4];
                session.reverse.as_mut().unwrap().nonce_prefix = vec![140 + index; 4];
                if role == 2 {
                    listener
                        .core
                        .send(2, Msg::StartSession(session))
                        .await
                        .unwrap();
                } else {
                    sources[0]
                        .core
                        .send(2, Msg::StartSession(session))
                        .await
                        .unwrap();
                }
            }
            listener.renew_ids.push(100 + index);
            sources[0].renew_ids.push(100 + index);
        }
        let health = drive_many(&mut sources, &mut listener, Duration::from_millis(1600)).await;
        assert_eq!(health.sessions.len(), 3);
        assert!(health.sessions.iter().all(|session| session
            .counters
            .as_ref()
            .unwrap()
            .received_packets
            > 10));
        assert!(listener.observed.rms() > 0.1);
        sources[0]
            .core
            .send(6, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        listener
            .core
            .send(6, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(
            sources.remove(0).handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    async fn lease_pair(expire_source: bool) {
        let mut source = child().await;
        let mut listener = child().await;
        let (a, b) = ports();
        let began = Instant::now();
        for id in [13, 14] {
            for role in [2, 1] {
                let mut session = start(
                    role,
                    if role == 1 { a } else { b },
                    if role == 1 { b } else { a },
                );
                session.session_id = vec![id; 16];
                session.forward.as_mut().unwrap().key = vec![id + 20; 32];
                session.reverse.as_mut().unwrap().key = vec![id + 40; 32];
                session.initial_authorization_ms = 1000;
                let core = if role == 1 {
                    &mut source.core
                } else {
                    &mut listener.core
                };
                core.send(2, Msg::StartSession(session)).await.unwrap();
            }
        }
        listener
            .core
            .send(
                3,
                Msg::SetGain(w::SetGain {
                    session_id: vec![14; 16],
                    gain_percent: 100,
                    muted: true,
                }),
            )
            .await
            .unwrap();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut stopped = None;
        let mut audible = false;
        let mut source_sent = false;
        let mut survivor_received = false;
        let mut pong_after_expiry = false;
        let mut expiry_started: Option<Instant> = None;
        let mut received_before_expiry = 0;
        let mut sent_before_expiry = 0;
        while began.elapsed() < Duration::from_secs(4) {
            let expired_for = expiry_started
                .map(|start| start.elapsed())
                .unwrap_or_default();
            if expired_for >= Duration::from_millis(1500) {
                break;
            }
            tokio::select! {
                _ = tick.tick() => {
                    audible |= listener.observed.rms() > 0.05;
                    if expiry_started.is_none() && audible && began.elapsed() >= Duration::from_millis(700) {
                        renew(&mut source.core, 13, 500).await;
                        renew(&mut listener.core, 13, 500).await;
                        expiry_started = Some(Instant::now());
                    }
                    if expired_for > Duration::from_millis(800) {
                        assert!(listener.observed.rms() < 1e-5, "expired PCM remained audible");
                    }
                    for (is_source, core) in [(true, &mut source.core), (false, &mut listener.core)] {
                        core.send(0, Msg::Ping(w::Ping { serial: 1 })).await.unwrap();
                        renew(core, 14, 500).await;
                        if expiry_started.is_none() || is_source != expire_source {
                            renew(core, 13, 500).await;
                        }
                    }
                }
                frame = source.core.recv() => {
                    match frame.unwrap().frame.msg.unwrap() {
                        Msg::SessionEvent(event) if event.kind == 5 => {
                            assert!(expire_source);
                            assert_eq!(event.session_id, vec![13; 16]);
                            assert_eq!(event.reason, 9);
                            stopped = Some(expiry_started.expect("media warmed up").elapsed());
                        }
                        Msg::Health(health) => {
                            if let Some(session) = health.sessions.iter().find(|session| session.session_id == vec![14; 16]) {
                                if expired_for < Duration::from_millis(500) {
                                    sent_before_expiry = session.sent_media_packets;
                                } else if expired_for > Duration::from_millis(800) {
                                    source_sent |= session.sent_media_packets > sent_before_expiry
                                        && session.sending == Some(true);
                                }
                            }
                        }
                        Msg::Pong(_) if expire_source && expired_for > Duration::from_millis(800) => pong_after_expiry = true,
                        Msg::Error(error) => panic!("source error: {}", error.code),
                        _ => {}
                    }
                }
                frame = listener.core.recv() => {
                    match frame.unwrap().frame.msg.unwrap() {
                        Msg::SessionEvent(event) if event.kind == 5 => {
                            assert!(!expire_source);
                            assert_eq!(event.session_id, vec![13; 16]);
                            assert_eq!(event.reason, 9);
                            stopped = Some(expiry_started.expect("media warmed up").elapsed());
                        }
                        Msg::Health(health) => {
                            if let Some(session) = health.sessions.iter().find(|session| session.session_id == vec![14; 16]) {
                                let packets = session.counters.as_ref().unwrap().received_packets;
                                if expired_for < Duration::from_millis(500) {
                                    received_before_expiry = packets;
                                } else if expired_for > Duration::from_millis(800) {
                                    survivor_received |= packets > received_before_expiry;
                                }
                            }
                        }
                        Msg::Pong(_) if !expire_source && expired_for > Duration::from_millis(800) => pong_after_expiry = true,
                        Msg::Error(error) => panic!("listener error: {}", error.code),
                        _ => {}
                    }
                }
            }
        }
        assert!(audible && source_sent && survivor_received && pong_after_expiry);
        assert!(stopped.unwrap() <= Duration::from_millis(650));
        let expired = if expire_source {
            &mut source
        } else {
            &mut listener
        };
        for (request, id) in [(201, 13), (202, 99)] {
            expired
                .core
                .send(
                    request,
                    Msg::RenewSession(w::RenewSession {
                        session_id: vec![id; 16],
                        ttl_ms: 1000,
                    }),
                )
                .await
                .unwrap();
            loop {
                let frame = expired.core.recv().await.unwrap().frame;
                if frame.request_id == request {
                    let Some(Msg::Error(error)) = frame.msg else {
                        panic!("renewal error missing")
                    };
                    assert_eq!(error.code, 6);
                    assert_eq!(error.session_id, vec![id; 16]);
                    break;
                }
            }
        }
        for fixture in [&mut source, &mut listener] {
            fixture
                .core
                .send(300, Msg::Shutdown(w::Shutdown {}))
                .await
                .unwrap();
        }
        assert_eq!(source.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
        assert_eq!(
            listener.handle.await.unwrap().unwrap(),
            ExitReason::Shutdown
        );
    }
    #[tokio::test]
    async fn blocked_start_expires_without_late_send_and_renew_cannot_resurrect() {
        fn slow_backend() -> Result<DryBackend, BackendError> {
            thread::sleep(Duration::from_millis(300));
            dry_backend()
        }
        let mut fixture = child_with_factory(Some(slow_backend)).await;
        let (a, _) = ports();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut session = start(1, a, u32::from(peer.local_addr().unwrap().port()));
        session.initial_authorization_ms = 50;
        fixture
            .core
            .send(2, Msg::StartSession(session))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        fixture
            .core
            .send(
                77,
                Msg::RenewSession(w::RenewSession {
                    session_id: vec![13; 16],
                    ttl_ms: 1000,
                }),
            )
            .await
            .unwrap();
        let rejected = tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                let frame = fixture.core.recv().await.unwrap().frame;
                assert!(
                    !matches!(&frame.msg, Some(Msg::SessionEvent(event)) if event.kind == 1 || event.kind == 6),
                    "expired blocked start emitted readiness"
                );
                if frame.request_id == 77 {
                    let Some(Msg::Error(error)) = frame.msg else { panic!("renewal error missing") };
                    assert_eq!(error.code, 6);
                    break;
                }
            }
        }).await;
        assert!(rejected.is_ok(), "renew queued behind blocked backend");
        loop {
            match next(&mut fixture.core).await {
                Msg::SessionEvent(event) if event.kind == 1 || event.kind == 6 => {
                    panic!("expired blocked start emitted readiness: {}", event.kind);
                }
                Msg::SessionEvent(event) if event.kind == 5 => {
                    assert_eq!(event.reason, 9);
                    break;
                }
                Msg::Error(error) => panic!("unexpected start error: {}", error.code),
                _ => {}
            }
        }
        assert_eq!(fixture.observed.pumps.load(Ordering::Relaxed), 0);
        assert_eq!(
            peer.recv(&mut [0; 2048]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(UdpSocket::bind(("127.0.0.1", a as u16)).unwrap());
        fixture
            .core
            .send(78, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(fixture.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    }
    #[tokio::test]
    async fn ping_only_source_lease_expires_send_gate_while_other_session_renews() {
        lease_pair(true).await;
    }
    #[tokio::test]
    async fn listener_lease_expires_render_gate_while_other_session_renews() {
        lease_pair(false).await;
    }
    #[tokio::test]
    async fn failed_first_start_releases_socket_and_later_bind_must_match() {
        let mut fixture = child().await;
        let (a, b) = ports();
        let mut failed = start(2, a, b);
        failed.render = Some(w::DeviceSelection {
            selector: Some(w::device_selection::Selector::DeviceId("MISSING".into())),
        });
        fixture
            .core
            .send(2, Msg::StartSession(failed))
            .await
            .unwrap();
        loop {
            match next(&mut fixture.core).await {
                Msg::Error(error) => {
                    assert_eq!(error.session_id, vec![13; 16]);
                    break;
                }
                Msg::SessionEvent(event) => panic!("failed init emitted event: {}", event.kind),
                _ => {}
            }
        }
        drop(UdpSocket::bind(("127.0.0.1", a as u16)).unwrap());
        let mut first = start(1, a, b);
        first.session_id = vec![14; 16];
        first.forward.as_mut().unwrap().key = vec![23; 32];
        first.reverse.as_mut().unwrap().key = vec![24; 32];
        fixture
            .core
            .send(3, Msg::StartSession(first))
            .await
            .unwrap();
        let mut incompatible = start(1, b, a);
        incompatible.session_id = vec![15; 16];
        incompatible.forward.as_mut().unwrap().key = vec![25; 32];
        incompatible.reverse.as_mut().unwrap().key = vec![26; 32];
        fixture
            .core
            .send(4, Msg::StartSession(incompatible))
            .await
            .unwrap();
        loop {
            if let Msg::Error(error) = next(&mut fixture.core).await {
                assert_eq!(error.session_id, vec![15; 16]);
                assert_eq!(error.code, 6);
                break;
            }
        }
        fixture
            .core
            .send(
                5,
                Msg::StopSession(w::StopSession {
                    session_id: vec![14; 16],
                }),
            )
            .await
            .unwrap();
        loop {
            if let Msg::SessionEvent(event) = next(&mut fixture.core).await {
                if event.kind == 5 {
                    assert_eq!(event.session_id, vec![14; 16]);
                    break;
                }
            }
        }
        drop(UdpSocket::bind(("127.0.0.1", a as u16)).unwrap());
        fixture
            .core
            .send(6, Msg::Shutdown(w::Shutdown {}))
            .await
            .unwrap();
        assert_eq!(fixture.handle.await.unwrap().unwrap(), ExitReason::Shutdown);
    }
    #[tokio::test]
    async fn silence_exits_and_bad_attach_cannot_probe() {
        let mut fixture = child().await;
        assert_eq!(fixture.observed.pumps.load(Ordering::Relaxed), 0);
        let (a, b) = ports();
        fixture
            .core
            .send(2, Msg::StartSession(start(1, a, b)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(fixture.observed.pumps.load(Ordering::Relaxed) > 0);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), fixture.handle)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            ExitReason::CoreLost
        );
        let pumps = fixture.observed.pumps.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(fixture.observed.pumps.load(Ordering::Relaxed), pumps);
        let (core, child) = tokio::io::duplex(65536);
        let task = tokio::spawn(run_connection(child, LaunchToken::new([1; 32].into())));
        let mut core = AudioLink::core(core, 1).unwrap();
        core.send(
            1,
            Msg::Attach(w::Attach {
                launch_token: vec![2; 32],
                backend: 1,
                core_silence_ms: 1000,
            }),
        )
        .await
        .unwrap();
        assert!(task.await.unwrap().is_err());
    }
}
