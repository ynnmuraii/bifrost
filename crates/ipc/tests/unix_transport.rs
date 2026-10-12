#![cfg(unix)]
//! Unix transport: owner-only directory and socket modes, `SO_PEERCRED` as the
//! identity check, stale-socket handling and the `socketpair` of the agent
//! channel.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ipc::peer::{CredentialSource, SameUser};
use ipc::transport::{self, Endpoint};
use ipc::{AgentLink, IpcError};
use protocol::ipc::{agent_frame, AgentAttach, AgentMode, Ping, Pong};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A socket in its own directory, removed when the test ends.
struct TempSocket {
    dir: PathBuf,
    endpoint: Endpoint,
}

impl TempSocket {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lws-ipc-test-{}-{tag}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            endpoint: Endpoint::socket(dir.join("gui.sock")),
            dir,
        }
    }
}

impl Drop for TempSocket {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn the_socket_and_its_directory_are_owner_only() {
    let temp = TempSocket::new("modes");
    assert!(!temp.dir.exists());

    let listener = transport::listen(&temp.endpoint).expect("binding");
    assert_eq!(mode(&temp.dir), 0o700, "the runtime directory must be 0700");
    assert_eq!(mode(temp.endpoint.path()), 0o600, "the socket must be 0600");
    assert_eq!(
        listener.socket_mode().expect("mode"),
        0o600,
        "GuiListener must report the same mode it set"
    );
}

#[tokio::test]
async fn a_live_socket_is_reported_and_a_stale_one_is_replaced() {
    let temp = TempSocket::new("stale");
    let listener = transport::listen(&temp.endpoint).expect("first bind");

    // A second core must not steal a live socket.
    let error = transport::listen(&temp.endpoint).unwrap_err();
    assert!(
        matches!(error, IpcError::AlreadyRunning { .. }),
        "{error:?}"
    );

    // After the owner is gone the file is stale: it is replaced, not reported.
    drop(listener);
    assert!(
        temp.endpoint.path().exists(),
        "the file outlives the listener"
    );
    let listener = transport::listen(&temp.endpoint).expect("stale socket replaced");
    assert_eq!(listener.socket_mode().expect("mode"), 0o600);
}

#[tokio::test]
async fn so_peercred_is_the_check_and_it_reports_the_real_uid() {
    let temp = TempSocket::new("peercred");
    let uid = ipc::peer::current_uid().expect("unix uid");
    let mut listener = transport::listen(&temp.endpoint).expect("binding");

    // A peer check for somebody else must refuse the connection and say who it
    // actually saw.
    let connecting = tokio::spawn({
        let endpoint = temp.endpoint.clone();
        async move { transport::connect(&endpoint).await }
    });
    let error = listener
        .accept(&SameUser::for_uid(uid.wrapping_add(1)))
        .await
        .unwrap_err();
    match error {
        IpcError::PeerRejected(rejected) => {
            assert_eq!(rejected.peer.source, CredentialSource::PeerCred);
            assert_eq!(rejected.peer.uid, Some(uid));
            assert!(
                rejected.expected.contains(&uid.wrapping_add(1).to_string()),
                "{}",
                rejected.expected
            );
        }
        other => panic!("expected a peer rejection, got {other:?}"),
    }
    assert!(connecting.await.expect("client task").is_ok());

    // The real owner is served, and the accepted stream carries the channel.
    let connecting = tokio::spawn({
        let endpoint = temp.endpoint.clone();
        async move {
            let stream = transport::connect(&endpoint).await?;
            let mut link = AgentLink::new(stream, 0, 3);
            link.attach(AgentAttach {
                core_silence_ms: 250,
                ..Default::default()
            })
            .await?;
            Ok::<(), IpcError>(())
        }
    });
    let stream = listener.accept(&SameUser::current()).await.expect("accept");
    let mut link = AgentLink::new(stream, 0, 3);
    let frame = link.recv().await.expect("AgentAttach");
    let attach =
        ipc::agent::expect_attach(frame.msg.as_ref().expect("payload")).expect("attach payload");
    assert_eq!(attach.core_silence_ms, 250);
    assert_eq!(frame.seq, 1);
    connecting
        .await
        .expect("client task")
        .expect("client sends");
}

#[tokio::test]
async fn the_agent_pair_is_a_connected_socketpair() {
    let (core, agent) = transport::agent_pair().await.expect("agent pair");
    let mut core = AgentLink::new(core, 0, 3);
    let mut agent = AgentLink::new(agent, 0, 3);

    core.send(agent_frame::Msg::Ping(Ping { nonce: 4 }))
        .await
        .expect("Ping");
    let frame = agent.recv().await.expect("Ping frame");
    assert_eq!(frame.seq, 1);
    assert!(matches!(frame.msg, Some(agent_frame::Msg::Ping(ping)) if ping.nonce == 4));

    agent
        .send(agent_frame::Msg::Pong(Pong { nonce: 4 }))
        .await
        .expect("Pong");
    assert_eq!(core.recv().await.expect("Pong frame").seq, 1);
}

/// The `socketpair` of the agent channel is inherited, never connected to, so
/// there is no peer to authenticate; what it must carry is the channel itself,
/// including the `RECOVERY` rule of rule 7.
#[tokio::test]
async fn recovery_mode_over_a_live_agent_pair() {
    let (core, agent) = transport::agent_pair().await.expect("agent pair");
    let mut core = AgentLink::new(core, 0, 3);
    let mut agent = AgentLink::new(agent, 0, 3);

    core.attach(AgentAttach {
        launch_token: vec![1u8; ipc::agent::LAUNCH_TOKEN_LEN],
        mode: AgentMode::Recovery as i32,
        core_silence_ms: 200,
        ..Default::default()
    })
    .await
    .expect("Attach");

    let frame = agent.recv().await.expect("Attach frame");
    let attach = ipc::agent::expect_attach(frame.msg.as_ref().expect("payload")).expect("attach");
    assert_eq!(attach.mode, AgentMode::Recovery as i32);
    assert_eq!(attach.core_silence_ms, 200);
    // Rule 7 over a live channel, not just as a predicate.
    let capture = agent_frame::Msg::BeginCapture(protocol::ipc::BeginCapture::default());
    assert!(!ipc::agent::mode_allows(attach.mode, &capture));
    assert!(ipc::agent::mode_allows(
        attach.mode,
        &agent_frame::Msg::ReleaseKeys(protocol::ipc::ReleaseKeys::default())
    ));
}

/// A cancelled `accept` must keep a client that connects while it was pending.
///
/// Windows needed `PendingInstance` for exactly this (`ipc::transport::windows.rs`, «# Cancel
/// safety»): core polls `GuiListener::accept` as one branch of a `select!`, so a connection
/// usually arrives while an earlier `accept` is cancelled and dropped, and an implementation
/// that had already taken the pending instance closed the connection under that client. On Unix
/// the accept is a single `poll_read_io` that either registers a waker or performs `accept4(2)`,
/// so nothing can be lost — this test pins that property, and it fails the day the Unix path
/// grows the Windows window.
#[tokio::test]
async fn a_cancelled_accept_keeps_the_client_that_connects_while_it_waits() {
    use std::future::Future;
    use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let socket = TempSocket::new("cancel");
    let mut listener = transport::listen(&socket.endpoint).expect("listening");

    // The cancel window: one poll with nothing pending, then drop the future.
    {
        let mut pending = std::pin::pin!(listener.accept(&SameUser::current()));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(
            pending.as_mut().poll(&mut context).is_pending(),
            "nothing has connected yet"
        );
    }

    // A client that connects *after* that cancellation is still served.
    let endpoint = socket.endpoint.clone();
    let client = tokio::spawn(async move {
        transport::connect(&endpoint)
            .await
            .expect("connecting to the GUI socket")
    });
    let mut accepted = listener
        .accept(&SameUser::current())
        .await
        .expect("accepting the queued client");
    let mut client = client.await.expect("the client task");

    // One byte each way proves the accepted end is that connection.
    client.write_all(b"q").await.expect("writing one byte");
    let mut byte = [0u8; 1];
    accepted.read_exact(&mut byte).await.expect("reading it");
    assert_eq!(&byte, b"q");
}

/// A waker that never wakes anything: this test drives one poll by hand.
fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable, Waker};
    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    // SAFETY: every callback is a no-op on a null pointer, which is the documented contract
    // for a waker that never wakes.
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
