//! Audit F3: a saturated outbound queue must not be able to swallow a link's `close()`.
//!
//! The manager closes a link it refuses at `Up` (the loser of a simultaneous dial) through
//! `LinkHandle::close`, and that link's task is already detached from the map, so nothing
//! else ends it. With `close` implemented as a `try_send` on the bounded queue, a full queue
//! dropped the `Shutdown`: the peer saw a bare socket close instead of the frame, and the
//! losing link ran on until the queue drained.
//!
//! The test keeps the link's task unpolled (`current_thread`) while it fills the queue, so
//! "the queue is full when `close` is called" is deterministic instead of a race.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use identity::{now_unix_ms, IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use transport::link::LinkConfig;
use transport::{read_frame, Direction, LinkLiveness, LinkReport, Stream};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lws-link-close-tests-{}-{}-{label}",
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

fn trust_of(peers: &[(geometry::NodeId, SocketAddr)]) -> TrustStore {
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

/// A link whose queue is already full when `close()` is called still tells the peer, and its
/// task still ends.
#[tokio::test(flavor = "current_thread")]
async fn a_saturated_queue_cannot_swallow_the_link_close() {
    let root = temp_root("saturated-close");
    let client = identity_at(&root, "client");
    let server = identity_at(&root, "server");
    let client_id = client.node_id();
    let server_id = server.node_id();

    let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind an ephemeral port");
    let address = listener.local_addr().expect("local address");

    let server_config =
        identity::trusted_server_config(&server, &trust_of(&[(client_id, address)]))
            .expect("server config");
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let accepting = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        acceptor.accept(tcp).await.expect("handshake")
    });

    let client_config =
        identity::trusted_client_config(&client, &trust_of(&[(server_id, address)]))
            .expect("client config");
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let tcp = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    tcp.set_nodelay(true).expect("nodelay");
    let tls = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .expect("client handshake");
    let mut peer = accepting.await.expect("the peer handshake finished");

    // The peer reads continuously from here on: frames must be consumed as they arrive, or a
    // Windows RST on our side (closing with unread data) would discard what the peer has not
    // read yet.
    let seen: Arc<std::sync::Mutex<Vec<&'static str>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader = {
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                match read_frame(&mut peer).await {
                    Ok(envelope) => {
                        let name = match envelope.payload {
                            Some(protocol::wire::envelope::Payload::Shutdown(_)) => "shutdown",
                            Some(protocol::wire::envelope::Payload::Hello(_)) => "hello",
                            Some(protocol::wire::envelope::Payload::InputBatch(_)) => "batch",
                            Some(_) => "other",
                            None => "empty",
                        };
                        seen.lock().expect("lock").push(name);
                        if name == "shutdown" {
                            return;
                        }
                    }
                    // The link ended and dropped its socket: whatever was read before stands.
                    Err(_) => return,
                }
            }
        })
    };

    let (reports, _report_rx) = tokio::sync::mpsc::channel::<LinkReport>(8);
    let hello = protocol::wire::Hello {
        node_id: client_id.as_bytes().to_vec(),
        display_name: "client".to_owned(),
        can_own: true,
        ..Default::default()
    };
    let link = transport::link::spawn(LinkConfig {
        peer: server_id,
        local: client_id,
        direction: Direction::Outgoing,
        key: identity::random16().expect("entropy"),
        local_hello: hello,
        stream: Stream::Client(Box::new(tls)),
        reports,
        queue_capacity: 1,
        liveness: LinkLiveness::default(),
    })
    .expect("spawn a link");

    // Let the link's task run once before the queue is filled: it writes its `Hello` and its
    // reader task consumes the peer's post-handshake messages, so our receive buffer is empty
    // when the link closes (a Windows close with unread data sends an RST, which would discard
    // the very frame this test is about, on the peer's side).
    tokio::time::sleep(Duration::from_millis(50)).await;
    let lease_id = [7u8; 16];
    let one = vec![input_model::InputEvent::PointerAbsolute(
        input_model::Position::new(1.0, 2.0),
    )];
    assert!(
        link.handle.send_motion(lease_id, one.clone()),
        "the capacity-1 queue takes one item"
    );
    assert!(
        !link.handle.send_motion(lease_id, one),
        "the queue is full now, so the next motion is dropped"
    );
    link.handle.close();

    // What the peer read decides it: the `Shutdown` frame must arrive (the `Hello` and the
    // queued motion come first; either order of the last two is fine).
    let _ = tokio::time::timeout(Duration::from_secs(5), reader).await;
    assert!(
        seen.lock().expect("lock").contains(&"shutdown"),
        "the peer must receive a Shutdown frame even though the queue was full; read: {:?}",
        seen.lock().expect("lock")
    );

    let _ = tokio::time::timeout(Duration::from_secs(5), link.task).await;
    let _ = std::fs::remove_dir_all(&root);
}
