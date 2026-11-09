#![cfg(unix)]
use super::*;

async fn connected_child(lifetime: &str) -> (Root, Child, AudioLink<ClientStream>) {
    let root = Root::new();
    let endpoint = Endpoint::socket(root.0.join("session-error.sock"));
    let mut listener = transport::listen(&endpoint).unwrap();
    let same_user = ipc::peer::SameUser::current();
    let (worker_io, core_io) =
        tokio::join!(transport::connect(&endpoint), listener.accept(&same_user),);
    let mut worker_link = AudioLink::worker(worker_io.unwrap());
    let mut core_link = AudioLink::core(core_io.unwrap(), 7).unwrap();
    core_link
        .send(
            1,
            Msg::Attach(w::Attach {
                launch_token: vec![71; 32],
                backend: 2,
                core_silence_ms: contract::CORE_SILENCE_MS,
            }),
        )
        .await
        .unwrap();
    worker_link.recv().await.unwrap();
    let ready = w::Ready {
        backend: 2,
        probed: false,
        capabilities: (1..=6)
            .map(|capability| w::CapabilityAvailability {
                capability,
                availability: 3,
                reason: 1,
            })
            .collect(),
    };
    worker_link
        .send(1, Msg::Ready(ready.clone()))
        .await
        .unwrap();
    core_link.recv().await.unwrap();
    // Only owned inert process lifetime is exercised, never a native audio backend.
    let process = tokio::process::Command::new("/usr/bin/sleep")
        .arg(lifetime)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let job = crate::job::KillOnCoreExit::attach(&process).unwrap();
    let child = Child {
        pid: process.id().unwrap(),
        process,
        _job: job,
        _listener: listener,
        _token_file: TokenFile(root.0.join("unused-token")),
        link: core_link,
        ready,
    };
    (root, child, worker_link)
}

async fn error_case(
    request_id: u64,
    error_session: Vec<u8>,
) -> (Result<()>, Vec<AudioSupervisorEvent>, BTreeSet<[u8; 16]>) {
    let (_root, mut child, mut worker_link) = connected_child("30").await;
    let session_id = [13; 16];
    let (commands_tx, mut commands) = mpsc::channel(4);
    let (_priority_tx, mut priority) = mpsc::channel(4);
    let (_source_tx, mut source_sound) = watch::channel(Vec::new());
    let talk_epoch = AtomicU64::new(0);
    let explicit_mute_epoch = AtomicU64::new(0);
    let available = AtomicU64::new(7);
    let mut sessions = BTreeSet::from([session_id, [14; 16]]);
    let (events, mut consumer) = mpsc::channel(32);
    let (stop_peer, mut stopped_peer) = oneshot::channel::<()>();
    let peer = tokio::spawn(async move {
        let mic = || {
            Msg::MuteSnapshot(w::MuteSnapshot {
                endpoint_id: "OWNED_MIC".into(),
                generation: 23,
                muted: true,
                guard_active: true,
            })
        };
        worker_link.send(0, mic()).await.unwrap();
        worker_link
            .send(
                request_id,
                Msg::Error(w::Error {
                    code: 8,
                    session_id: error_session,
                }),
            )
            .await
            .unwrap();
        worker_link
            .send(
                0,
                Msg::SessionEvent(w::SessionEvent {
                    session_id: session_id.to_vec(),
                    kind: 5,
                    reason: 4,
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        worker_link.send(0, mic()).await.unwrap();
        loop {
            tokio::select! {
                _ = &mut stopped_peer => break,
                received = worker_link.recv() => {
                    match received.unwrap().frame.msg.unwrap() {
                        Msg::Ping(ping) => worker_link.send(0, Msg::Pong(w::Pong {
                            serial: ping.serial,
                        })).await.unwrap(),
                        message => panic!("unexpected supervisor command: {message:?}"),
                    }
                }
            }
        }
    });
    let observe = async move {
        let mut observed = Vec::new();
        let _ = timeout(Duration::from_millis(500), async {
            let mut mic_snapshots = 0;
            while let Some(event) = consumer.recv().await {
                if matches!(&event, AudioSupervisorEvent::MuteSnapshot { .. }) {
                    mic_snapshots += 1;
                }
                observed.push(event);
                if mic_snapshots == 2 {
                    break;
                }
            }
        })
        .await;
        drop(commands_tx);
        observed
    };
    let (outcome, observed) = tokio::join!(
        serve(
            &mut child,
            7,
            &mut commands,
            (
                &mut priority,
                &talk_epoch,
                &explicit_mute_epoch,
                &mut source_sound
            ),
            &events,
            &available,
            &mut sessions
        ),
        observe,
    );
    let _ = stop_peer.send(());
    peer.await.unwrap();
    child.reap().await;
    (outcome, observed, sessions)
}

#[tokio::test]
async fn supervisor_keeps_session_failure_nonfatal() {
    let (outcome, observed, sessions) = error_case(0, vec![13; 16]).await;
    assert_eq!(outcome, Ok(()), "session-local failure: {observed:?}");
    assert!(observed.iter().any(|event| matches!(event,
        AudioSupervisorEvent::CommandRejected { generation: 7, request_id: 0, error }
        if error.code == 8 && error.session_id == vec![13; 16])));
    assert!(observed.iter().any(|event| matches!(event,
        AudioSupervisorEvent::SessionEvent { generation: 7, event }
        if event.kind == 5 && event.session_id == vec![13; 16])));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event,
        AudioSupervisorEvent::MuteSnapshot { generation: 7, snapshot, .. }
        if snapshot.guard_active && snapshot.muted && snapshot.generation == 23))
            .count(),
        2
    );
    assert!(!sessions.contains(&[13; 16]));
    assert!(sessions.contains(&[14; 16]));
}

#[tokio::test]
async fn supervisor_rejects_unknown_unsolicited_and_uncorrelated_failures() {
    for (request_id, session) in [
        (0, vec![99; 16]),
        (0, Vec::new()),
        (1, vec![13; 16]),
        (2, vec![13; 16]),
    ] {
        let (outcome, observed, sessions) = error_case(request_id, session).await;
        assert_eq!(outcome, Err(AudioSupervisorError::InvalidFrame));
        assert!(!observed
            .iter()
            .any(|event| matches!(event, AudioSupervisorEvent::CommandRejected { .. })));
        assert!(sessions.contains(&[13; 16]));
    }
}

#[tokio::test]
async fn supervisor_graceful_shutdown_allows_native_stop_budget() {
    let (_root, mut child, _worker) = connected_child("1.5").await;
    child.wait_for_shutdown().await;
    assert!(
        child.process.try_wait().unwrap().unwrap().success(),
        "a normal restoration within the native two-second contract must not be killed"
    );
}

#[tokio::test]
async fn supervisor_graceful_shutdown_still_kills_a_hung_child() {
    let (_root, mut child, _worker) = connected_child("30").await;
    let start = Instant::now();
    child.wait_for_shutdown().await;
    assert!(!child.process.try_wait().unwrap().unwrap().success());
    assert!(start.elapsed() < Duration::from_secs(5));
}
