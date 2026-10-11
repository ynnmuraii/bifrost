#![cfg(windows)]
//! Windows transport: the real named pipe, its DACL, the first-instance flag and
//! the fact that the kernel — not this crate — decides who may connect.

use std::sync::atomic::{AtomicU64, Ordering};

use ipc::peer::{
    AllowAnyPeer, CredentialSource, PeerCheck, PeerCredentials, PeerRejected, SameUser,
};
use ipc::transport::{self, Endpoint, PipeOptions, PipeSecurity};
use ipc::{AgentLink, IpcError};
use protocol::ipc::{agent_frame, AgentAttach, AgentMode, AgentReady};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A fresh pipe name per test: the tests in one binary run in parallel, and a
/// pipe name is global to the machine.
fn endpoint(tag: &str) -> Endpoint {
    let sid = transport::current_user_sid_string().expect("current user SID");
    Endpoint::pipe(format!(
        "{}-test-{tag}-{}",
        transport::gui_pipe_name(&sid),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A peer check that insists on a uid, to show what Windows does and does not
/// report.
struct RequireUid;

impl PeerCheck for RequireUid {
    fn check(&self, peer: &PeerCredentials) -> Result<(), PeerRejected> {
        match peer.uid {
            Some(_) => Ok(()),
            None => Err(PeerRejected {
                peer: *peer,
                expected: "a uid".to_owned(),
            }),
        }
    }
}

#[test]
fn the_dacl_grants_full_access_to_the_current_user_sid_only() {
    let sid = transport::current_user_sid_string().expect("current user SID");
    assert!(sid.starts_with("S-1-"), "not a SID: {sid}");
    assert!(
        sid.split('-')
            .skip(1)
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())),
        "unexpected SID shape: {sid}"
    );

    let expected = format!("D:P(A;;GA;;;{sid})");
    assert_eq!(transport::gui_sddl(&sid), expected);
    assert_eq!(
        transport::gui_pipe_name(&sid),
        format!(r"\\.\pipe\lan-workspace-{sid}")
    );

    // The descriptor the pipe is actually created with is that same DACL, and a
    // malformed SDDL is refused instead of silently granting nothing.
    assert_eq!(
        PipeSecurity::for_current_user().expect("descriptor").sddl(),
        expected
    );
    assert_eq!(
        PipeSecurity::for_user_sid(&sid).expect("descriptor").sddl(),
        expected
    );
    let different = PipeSecurity::for_user_sid("S-1-5-18").expect("descriptor");
    assert_eq!(different.sddl(), "D:P(A;;GA;;;S-1-5-18)");
}

#[tokio::test]
async fn a_pipe_name_taken_by_someone_else_is_refused_not_served() {
    let security = PipeSecurity::for_current_user().expect("descriptor");

    // Both ways somebody else can own the name: every instance taken (a
    // single-instance pipe), and the name itself already claimed (which
    // FILE_FLAG_FIRST_PIPE_INSTANCE then refuses with ERROR_ACCESS_DENIED).
    for options in [PipeOptions::agent_pair(), PipeOptions::gui()] {
        let endpoint = endpoint("squat");
        let squatter = security
            .create(endpoint.name(), &options)
            .expect("the squatter creates the first instance");

        let error = transport::listen(&endpoint).unwrap_err();
        assert!(
            matches!(error, IpcError::AlreadyRunning { .. }),
            "an owned name must not be served: {error:?}"
        );

        // With the name free the very same call succeeds, so the failure above is
        // attributable to the name being taken and not to something else.
        drop(squatter);
        let listener = transport::listen(&endpoint).expect("the name is free now");
        assert_eq!(listener.name(), endpoint.name());
        assert!(listener.sddl().starts_with("D:P(A;;GA;;;S-1-"));
    }
}

#[tokio::test]
async fn attach_and_ready_round_trip_over_a_real_named_pipe() {
    let endpoint = endpoint("roundtrip");
    let token = [0x5Au8; ipc::agent::LAUNCH_TOKEN_LEN];
    let mut listener = transport::listen(&endpoint).expect("listening");

    let agent = tokio::spawn(async move {
        let stream = listener.accept(&SameUser::current()).await.expect("accept");
        let mut link = AgentLink::new(stream, 0, 3);

        let frame = link.recv().await.expect("AgentAttach");
        let attach =
            ipc::agent::expect_attach(frame.msg.as_ref().expect("payload")).expect("attach");
        ipc::agent::verify_launch_token(&token, &attach.launch_token).expect("launch token");
        assert_eq!(attach.mode, AgentMode::Normal as i32);

        link.send(agent_frame::Msg::Ready(AgentReady {
            capabilities: vec!["capture".to_owned(), "inject".to_owned()],
            platform: "platform-win".to_owned(),
            session_kind: "windows".to_owned(),
            backend: None,
        }))
        .await
        .expect("AgentReady");

        attach.core_silence_ms
    });

    let stream = transport::connect(&endpoint).await.expect("connecting");
    let mut core = AgentLink::new(stream, 0, 3);
    core.attach(AgentAttach {
        launch_token: token.to_vec(),
        mode: AgentMode::Normal as i32,
        emergency_hotkey: "Ctrl+Alt+Shift+F12".to_owned(),
        core_silence_ms: 300,
        require_emergency_hotkey: true,
        ..Default::default()
    })
    .await
    .expect("sending AgentAttach");

    let frame = core.recv().await.expect("AgentReady");
    match frame.msg {
        Some(agent_frame::Msg::Ready(ready)) => {
            assert_eq!(ready.platform, "platform-win");
            assert_eq!(ready.session_kind, "windows");
            assert_eq!(ready.capabilities, ["capture", "inject"]);
        }
        other => panic!("expected AgentReady, got {other:?}"),
    }
    assert_eq!(agent.await.expect("agent task"), 300);
}

#[tokio::test]
async fn the_pipe_dacl_is_the_identity_and_no_uid_is_reported() {
    let endpoint = endpoint("dacl");
    let mut listener = transport::listen(&endpoint).expect("listening");

    let connecting = tokio::spawn({
        let endpoint = endpoint.clone();
        async move { transport::connect(&endpoint).await }
    });
    let error = listener.accept(&RequireUid).await.unwrap_err();
    match error {
        IpcError::PeerRejected(rejected) => {
            assert_eq!(rejected.peer.source, CredentialSource::PipeDacl);
            assert_eq!(rejected.peer.uid, None, "Windows reports no uid");
            assert_eq!(rejected.peer.gid, None);
            assert_eq!(rejected.peer.pid, None);
        }
        other => panic!("expected a peer rejection, got {other:?}"),
    }
    // The kernel let the client in; the hook is what refused it.
    assert!(connecting.await.expect("client task").is_ok());

    // SameUser accepts exactly what the DACL implies, and a refused peer does not
    // damage the listener.
    assert!(SameUser::current()
        .check(&PeerCredentials::pipe_dacl())
        .is_ok());
    assert!(AllowAnyPeer.check(&PeerCredentials::pipe_dacl()).is_ok());

    let connecting = tokio::spawn({
        let endpoint = endpoint.clone();
        async move { transport::connect(&endpoint).await }
    });
    let stream = listener.accept(&SameUser::current()).await.expect("accept");
    drop(stream);
    assert!(connecting.await.expect("client task").is_ok());
}

#[tokio::test]
async fn the_agent_pair_is_two_connected_ends_and_only_one_pair_per_name() {
    let (core, agent) = transport::agent_pair().await.expect("agent pair");
    let mut core = AgentLink::new(core, 0, 3);
    let mut agent = AgentLink::new(agent, 0, 3);

    core.send(agent_frame::Msg::Ping(protocol::ipc::Ping { nonce: 9 }))
        .await
        .expect("Ping");
    let frame = agent.recv().await.expect("Ping frame");
    assert_eq!(frame.seq, 1);
    agent
        .send(agent_frame::Msg::Pong(protocol::ipc::Pong { nonce: 9 }))
        .await
        .expect("Pong");
    assert_eq!(core.recv().await.expect("Pong frame").seq, 1);
}
