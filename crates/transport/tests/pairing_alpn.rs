//! `pair/1` and `input/1` share one listener on `47901`, told apart by ALPN
//! (`docs/05-protocol.md`, «Сопряжение»: «Сопряжение — отдельный bounded state machine на
//! ALPN `lan-workspace/pair/1`»).
//!
//! The manager peeks the ClientHello with `LazyConfigAcceptor`: a `pair/1` offer while a
//! [`PairingGate`] window is open is handshaked with the unknown-peer pairing config and
//! handed to the application as a [`LinkEvent::Pairing`]; a `pair/1` offer with no window is
//! dropped before the handshake; everything else keeps the pinned link path. No fixed port is
//! used — every manager binds an ephemeral loopback port and every identity lives in its own
//! temp directory.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use geometry::NodeId;
use identity::{now_unix_ms, IdentityPaths, NodeIdentity, PairingWindow, TrustStore, TrustedPeer};
use protocol::wire;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use transport::{BackoffBounds, LinkEvent, LinkLiveness, LinkManager, ManagerConfig, PairingGate};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous bounds: the tests wait for events, never for a fixed sleep.
const TIMEOUT: Duration = Duration::from_secs(10);
/// A short "nothing must arrive" window.
const QUIET: Duration = Duration::from_millis(700);

const LIVENESS: LinkLiveness = LinkLiveness {
    ping_ms: 1_000,
    offline_ms: 3_000,
};

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lws-pairing-alpn-tests-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temporary directory");
    root
}

fn identity_at(root: &std::path::Path, name: &str) -> NodeIdentity {
    NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join(name)))
        .expect("identity")
        .0
}

fn hello_for(id: NodeId, name: &str) -> wire::Hello {
    wire::Hello {
        node_id: id.as_bytes().to_vec(),
        display_name: name.to_owned(),
        can_own: true,
        session_kind: wire::SessionKind::X11 as i32,
        ..Default::default()
    }
}

fn trust_of(peers: &[(NodeId, SocketAddr)]) -> TrustStore {
    let mut trust = TrustStore::new();
    for (peer, address) in peers {
        trust.trust(TrustedPeer::paired(
            *peer,
            "peer",
            Some(address.to_string()),
            now_unix_ms(),
        ));
    }
    trust
}

async fn free_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind an ephemeral port");
    listener.local_addr().expect("local address")
}

struct TestNode {
    events: mpsc::Receiver<LinkEvent>,
    runner: JoinHandle<()>,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

/// Spawns a manager on an ephemeral loopback port with the given pairing gate.
async fn spawn_node(
    name: &'static str,
    identity: Arc<NodeIdentity>,
    listen: SocketAddr,
    trusted: &[(NodeId, SocketAddr)],
    dial: &[SocketAddr],
    gate: PairingGate,
) -> TestNode {
    let id = identity.node_id();
    let config = ManagerConfig {
        identity,
        trust: trust_of(trusted),
        local_hello: hello_for(id, name),
        listen,
        peers: dial.to_vec(),
        max_members: 3,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
        pairing: gate,
    };
    let (manager, events) = LinkManager::new(config).expect("manager");
    let manager = Arc::new(manager);
    let runner = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = manager.run().await;
        })
    };
    TestNode { events, runner }
}

/// Connects to a manager that was just spawned, waiting until its listener is bound.
///
/// `spawn_node` returns as soon as `manager.run()` is spawned, and the listener is bound
/// inside `run()`; `LinkManager` exposes no readiness signal, so the first dial races the
/// bind and can come back `ECONNREFUSED` (seen on `ubuntu-latest`). Retrying the connect is
/// the right place for the wait: a refused connect is not a `pair/1` or `input/1` result,
/// and the manager's own behaviour after the handshake starts is what these tests assert on.
async fn connect_when_listening(address: SocketAddr) -> TcpStream {
    /// How long a just-spawned manager may take to bind before the test gives up.
    const LISTEN_TIMEOUT: Duration = Duration::from_secs(5);
    const RETRY_DELAY: Duration = Duration::from_millis(20);

    let deadline = Instant::now() + LISTEN_TIMEOUT;
    loop {
        match TcpStream::connect(address).await {
            Ok(stream) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                assert!(
                    Instant::now() < deadline,
                    "no listener on {address} within {}s: {error}",
                    LISTEN_TIMEOUT.as_secs()
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(error) => panic!("cannot reach {address}: {error}"),
        }
    }
}

/// Waits for the first event that satisfies `predicate`, ignoring the others.
async fn wait_for_event(
    events: &mut mpsc::Receiver<LinkEvent>,
    predicate: impl Fn(&LinkEvent) -> bool,
    timeout: Duration,
) -> Option<LinkEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(event)) if predicate(&event) => return Some(event),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// `Ok(Err(_))` is a failed handshake, `Err(_)` is the client hanging until the timeout;
/// both mean the connection did not complete.
fn refused(
    result: Result<
        std::io::Result<tokio_rustls::client::TlsStream<TcpStream>>,
        tokio::time::error::Elapsed,
    >,
) -> bool {
    matches!(result, Ok(Err(_)) | Err(_))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_dialer_is_handed_to_the_application_while_the_window_is_open() {
    let root = temp_root("pair-open");
    let a = identity_at(&root, "a");
    let b = identity_at(&root, "b");
    let attempt_id = [0x5Au8; 16];

    let gate = PairingGate::closed();
    gate.open(PairingWindow::open(0, attempt_id));
    let address = free_address().await;
    let mut node = spawn_node("a", Arc::new(a), address, &[], &[], gate).await;

    let config = identity::pairing_client_config(&b).expect("pairing client config");
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = connect_when_listening(address).await;
    let local = tcp.local_addr().expect("local address");
    let _stream = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .expect("the pair/1 handshake completes while the window is open");

    let event = wait_for_event(
        &mut node.events,
        |event| matches!(event, LinkEvent::Pairing(_)),
        TIMEOUT,
    )
    .await
    .expect("the manager hands the connection to the application");
    match event {
        LinkEvent::Pairing(connection) => {
            assert_eq!(connection.window.attempt_id(), &attempt_id);
            assert_eq!(connection.address, local);
        }
        other => panic!("unexpected event {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_dialer_is_refused_when_no_window_is_open() {
    let root = temp_root("pair-closed");
    let a = identity_at(&root, "a");
    let b = identity_at(&root, "b");

    let address = free_address().await;
    let mut node = spawn_node("a", Arc::new(a), address, &[], &[], PairingGate::closed()).await;

    let config = identity::pairing_client_config(&b).expect("pairing client config");
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = connect_when_listening(address).await;
    let result = tokio::time::timeout(
        TIMEOUT,
        connector.connect(identity::server_name(address.ip()), tcp),
    )
    .await;
    assert!(
        refused(result),
        "a pair/1 offer with no open window must not complete a handshake"
    );
    assert!(
        wait_for_event(
            &mut node.events,
            |event| matches!(event, LinkEvent::Pairing(_)),
            QUIET,
        )
        .await
        .is_none(),
        "no Pairing event may be produced without a window"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_peer_on_input_is_still_refused_with_a_window_open() {
    let root = temp_root("input-unknown");
    let a = identity_at(&root, "a");
    let b = identity_at(&root, "b");

    let gate = PairingGate::closed();
    gate.open(PairingWindow::open(0, [0x11u8; 16]));
    let address = free_address().await;
    let a_id = a.node_id();
    // `a` pins nobody, but its window is open: an unknown peer on input/1 must still be
    // refused by the pin, and must not be mistaken for a pairing connection.
    let mut node = spawn_node("a", Arc::new(a), address, &[], &[], gate).await;

    let trust = trust_of(&[(a_id, address)]);
    let config = identity::trusted_client_config(&b, &trust).expect("client config");
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = connect_when_listening(address).await;
    let result = tokio::time::timeout(
        TIMEOUT,
        connector.connect(identity::server_name(address.ip()), tcp),
    )
    .await;
    // TLS 1.3 lets the client finish before the server's client-certificate rejection
    // reaches it, so the client result is not the proof here: the manager's silence is (no
    // `Hello` is ever exchanged, so no `Up` — and the input ALPN never yields `Pairing`).
    let _ = result;
    assert!(
        wait_for_event(
            &mut node.events,
            |event| matches!(event, LinkEvent::Pairing(_) | LinkEvent::Up { .. }),
            QUIET,
        )
        .await
        .is_none(),
        "neither a Pairing nor an Up event may be produced for a refused input peer"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_input_link_still_forms_while_a_window_is_open() {
    let root = temp_root("input-pinned");
    let a = identity_at(&root, "a");
    let b = identity_at(&root, "b");
    let a_id = a.node_id();
    let b_id = b.node_id();

    let a_address = free_address().await;
    let b_address = free_address().await;

    let gate = PairingGate::closed();
    gate.open(PairingWindow::open(0, [0x22u8; 16]));
    let mut node_a = spawn_node("a", Arc::new(a), a_address, &[(b_id, b_address)], &[], gate).await;
    let _node_b = spawn_node(
        "b",
        Arc::new(b),
        b_address,
        &[(a_id, a_address)],
        &[a_address],
        PairingGate::closed(),
    )
    .await;

    let event = wait_for_event(
        &mut node_a.events,
        |event| matches!(event, LinkEvent::Up { .. }),
        TIMEOUT,
    )
    .await
    .expect("the pinned input link forms while the pairing gate is open");
    match event {
        LinkEvent::Up { peer, .. } => assert_eq!(peer, b_id),
        other => panic!("unexpected event {other:?}"),
    }
    assert!(
        wait_for_event(
            &mut node_a.events,
            |event| matches!(event, LinkEvent::Pairing(_)),
            QUIET,
        )
        .await
        .is_none(),
        "an input/1 link must never be reported as a pairing connection"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// R3: an abandoned `pair/1` handshake must release its admission slot.
///
/// `PairingGate::admit` runs before the TLS handshake, so a connection that sends a
/// ClientHello and then disappears used to leave `pending` incremented for ever: three of
/// them pinned the window and the fourth, honest dialer was refused. The `PendingGuard` in
/// `serve_connection` releases the slot whenever the connection never reaches the
/// application (failed handshake, dropped event), so the window still admits the fourth and
/// the peer is handed over as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_pair_handshakes_do_not_pin_the_window() {
    use tokio::io::AsyncWriteExt;

    let root = temp_root("pair-abandoned");
    let a = identity_at(&root, "a");
    let b = identity_at(&root, "b");
    let attempt_id = [0x33u8; 16];

    let gate = PairingGate::closed();
    gate.open(PairingWindow::open(0, attempt_id));
    let address = free_address().await;
    let mut node = spawn_node("a", Arc::new(a), address, &[], &[], gate.clone()).await;

    // A real ClientHello for the pair ALPN, built once and replayed.
    let config = identity::pairing_client_config(&b).expect("pairing client config");
    let mut hello = Vec::new();
    tokio_rustls::rustls::ClientConnection::new(
        Arc::new(config.clone()),
        identity::server_name(address.ip()),
    )
    .expect("client connection")
    .write_tls(&mut hello)
    .expect("client hello");
    assert!(!hello.is_empty(), "the ClientHello must be non-empty");

    for attempt in 0..3 {
        let mut tcp = connect_when_listening(address).await;
        tcp.write_all(&hello).await.expect("send the ClientHello");
        tcp.shutdown().await.expect("close the connection");
        // Give the server time to parse the hello, admit, and fail the handshake.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = attempt;
    }

    assert_eq!(
        gate.window(0).map(|window| window.pending()),
        Some(0),
        "an abandoned handshake must release its slot"
    );

    // The fourth dialer is a real one and must still be admitted and handed over.
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = connect_when_listening(address).await;
    let _stream = tokio::time::timeout(
        TIMEOUT,
        connector.connect(identity::server_name(address.ip()), tcp),
    )
    .await
    .expect("the fourth handshake must not hang")
    .expect("the fourth pair/1 handshake completes");
    let event = wait_for_event(
        &mut node.events,
        |event| matches!(event, LinkEvent::Pairing(_)),
        TIMEOUT,
    )
    .await
    .expect("the manager hands the fourth connection to the application");
    match event {
        LinkEvent::Pairing(connection) => assert_eq!(connection.window.attempt_id(), &attempt_id),
        other => panic!("unexpected event {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

/// R3: a TCP connection that never sends a ClientHello is closed after the handshake
/// timeout instead of pinning a task for the life of the manager.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_client_hello_is_closed_after_the_timeout() {
    use tokio::io::AsyncReadExt;

    let root = temp_root("pair-silent");
    let a = identity_at(&root, "a");

    let gate = PairingGate::closed();
    gate.open(PairingWindow::open(0, [0x44u8; 16]));
    let address = free_address().await;
    let _node = spawn_node("a", Arc::new(a), address, &[], &[], gate).await;

    let mut tcp = connect_when_listening(address).await;
    // `LIVENESS.offline_ms` is the listener's handshake timeout; the socket must be closed
    // shortly after it, well before this generous bound.
    let mut buffer = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(8), tcp.read(&mut buffer)).await;
    assert!(
        matches!(read, Ok(Ok(0))),
        "the manager must close a silent connection after the handshake timeout, got {read:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_channel_authenticates_certificate_without_opening_pairing() {
    let root = temp_root("request-closed");
    let a = Arc::new(identity_at(&root, "a"));
    let b = identity_at(&root, "b");
    let address = free_address().await;
    let mut node = spawn_node(
        "a",
        Arc::clone(&a),
        address,
        &[],
        &[],
        PairingGate::closed(),
    )
    .await;
    drop(connect_when_listening(address).await);
    let stream = transport::pairing::dial_request(&b, address, a.node_id())
        .await
        .expect("request TLS");
    let event = wait_for_event(
        &mut node.events,
        |event| matches!(event, LinkEvent::PairingRequest(_)),
        TIMEOUT,
    )
    .await
    .expect("request only");
    match event {
        LinkEvent::PairingRequest(connection) => assert_eq!(connection.peer, b.node_id()),
        other => panic!("unexpected {other:?}"),
    }
    drop(stream);
    let mismatch = transport::pairing::dial_request(&b, address, NodeId::new([0; 32]))
        .await
        .expect_err("different discovery identity");
    assert_ne!(
        mismatch.kind(),
        std::io::ErrorKind::Unsupported,
        "certificate mismatch must not downgrade"
    );
    drop(node);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restricted_window_never_hands_a_different_certificate_to_sas() {
    let root = temp_root("restricted");
    let a = Arc::new(identity_at(&root, "a"));
    let b = identity_at(&root, "b");
    let other = identity_at(&root, "other");
    let gate = PairingGate::closed();
    let mut window = PairingWindow::open(0, [7; 16]);
    window.restrict_to(b.node_id());
    gate.open(window);
    let address = free_address().await;
    let mut node = spawn_node("a", a, address, &[], &[], gate).await;
    let tcp = connect_when_listening(address).await;
    let connector = TlsConnector::from(Arc::new(identity::pairing_client_config(&other).unwrap()));
    let _wrong = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await;
    assert!(wait_for_event(
        &mut node.events,
        |event| matches!(event, LinkEvent::Pairing(_)),
        QUIET
    )
    .await
    .is_none());
    let tcp = connect_when_listening(address).await;
    let connector = TlsConnector::from(Arc::new(identity::pairing_client_config(&b).unwrap()));
    let _right = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .expect("requester allowed");
    assert!(wait_for_event(
        &mut node.events,
        |event| matches!(event, LinkEvent::Pairing(_)),
        TIMEOUT
    )
    .await
    .is_some());
    drop(node);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn an_explicit_legacy_alpn_refusal_is_distinct_from_network_or_identity_errors() {
    let root = temp_root("request-legacy");
    let target = identity_at(&root, "target");
    let requester = identity_at(&root, "requester");
    let target_id = target.node_id();
    let config = identity::pairing_server_config(&target).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        assert!(TlsAcceptor::from(Arc::new(config))
            .accept(tcp)
            .await
            .is_err());
    });
    let error = tokio::time::timeout(
        TIMEOUT,
        transport::pairing::dial_request(&requester, address, target_id),
    )
    .await
    .expect("bounded TLS handshake")
    .expect_err("old target has only pair/1");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    server.await.unwrap();
    let error = transport::pairing::dial_request(&requester, address, target_id)
        .await
        .unwrap_err();
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::Unsupported,
        "closed TCP port must not trigger a legacy downgrade"
    );
    let _ = std::fs::remove_dir_all(root);
}
