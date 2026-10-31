use super::*;
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
#[cfg(unix)]
mod local {
    use super::*;
    use ipc::{
        peer::SameUser,
        transport::{self, Endpoint},
    };
    use std::net::UdpSocket;
    struct Fixture {
        core: AudioLink<tokio::net::UnixStream>,
        handle: tokio::task::JoinHandle<Result<ExitReason, RoleError>>,
        observed: Arc<Observed>,
        _directory: tempfile::TempDir,
    }
    async fn child() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::socket(directory.path().join("audio.sock"));
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
                backend: 2,
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
            _directory: directory,
        }
    }
    async fn next(core: &mut AudioLink<tokio::net::UnixStream>) -> Msg {
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
        }
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
                _=ping.tick()=>{source.core.send(0,Msg::Ping(w::Ping {serial})).await.unwrap();listener.core.send(0,Msg::Ping(w::Ping {serial})).await.unwrap();serial+=1;},
                frame=source.core.recv()=>{if let Some(Msg::Health(health))=frame.unwrap().frame.msg {source_active|=health.sessions.iter().any(|session|session.capture_active==Some(true)&&session.sending==Some(true)&&session.sent_media_packets>0);}},
                frame=listener.core.recv()=>{if let Some(Msg::Health(health))=frame.unwrap().frame.msg {listener_received|=health.sessions.iter().any(|session|session.counters.as_ref().unwrap().received_packets>0&&session.capture_active==Some(false));}},
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
        source
            .core
            .send(4, Msg::StartSession(start(1, a, b)))
            .await
            .unwrap();
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
