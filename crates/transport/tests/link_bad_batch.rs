//! Audit F4: an input batch carrying an event with no wire form must not end the link.
//!
//! `to_wire_event` refuses `InputEvent::PointerRelative` (`protocol.proto`'s `InputEvent` has
//! no relative member), and `write_message` turned *any* conversion error into
//! `LinkDownReason::Protocol`: one such event — or one absolute position whose `display_id`
//! was empty, which the encoder emits until an `EnterPrepare` names a display — closed the
//! whole mTLS link and, with it, the session.
//!
//! The batch now keeps the events it can express (a key or button edge in the same batch is
//! not lost), the rest are dropped with a rate-limited log, and the link stays up.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use identity::{IdentityPaths, NodeIdentity, TrustStore, TrustedPeer, now_unix_ms};
use input_model::{HidKey, InputEvent, KeyState};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use transport::link::LinkConfig;
use transport::{read_frame, Direction, LinkLiveness, LinkReport, Stream};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lws-link-bad-batch-tests-{}-{}-{label}",
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

#[tokio::test(flavor = "current_thread")]
async fn a_batch_with_an_unrepresentable_event_does_not_end_the_link() {
    let root = temp_root("bad-batch");
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
    let tcp = tokio::net::TcpStream::connect(address).await.expect("connect");
    tcp.set_nodelay(true).expect("nodelay");
    let tls = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .expect("client handshake");
    let mut peer = accepting.await.expect("the peer handshake finished");

    // `(frame kind, event count)` of everything the peer reads.
    let seen: Arc<std::sync::Mutex<Vec<(&'static str, usize)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader = {
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            loop {
                match read_frame(&mut peer).await {
                    Ok(envelope) => match envelope.payload {
                        Some(protocol::wire::envelope::Payload::InputBatch(batch)) => {
                            let count = batch.events.len();
                            seen.lock().expect("lock").push(("batch", count));
                        }
                        Some(protocol::wire::envelope::Payload::Shutdown(_)) => {
                            seen.lock().expect("lock").push(("shutdown", 0));
                            return;
                        }
                        Some(protocol::wire::envelope::Payload::Hello(_)) => {
                            seen.lock().expect("lock").push(("hello", 0));
                        }
                        Some(_) => seen.lock().expect("lock").push(("other", 0)),
                        None => seen.lock().expect("lock").push(("empty", 0)),
                    },
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
        queue_capacity: 16,
        liveness: LinkLiveness::default(),
    })
    .expect("spawn a link");
    tokio::time::sleep(Duration::from_millis(20)).await;

    let lease_id = [7u8; 16];
    // The first batch mixes an event with no wire form with a key-up: only the relative
    // pointer delta may be dropped, and the key-up has to arrive (a lost up is a stuck key).
    link.handle
        .send(focus_router::Message::InputBatch {
            lease_id,
            input_batch_seq: 0,
            events: vec![
                InputEvent::PointerRelative { dx: 3, dy: 4 },
                InputEvent::Key {
                    key: HidKey::keyboard(0x04),
                    state: KeyState::Up,
                },
            ],
        })
        .await
        .expect("the link is alive");
    link.handle
        .send(focus_router::Message::InputBatch {
            lease_id,
            input_batch_seq: 0,
            events: vec![InputEvent::Key {
                key: HidKey::keyboard(0x05),
                state: KeyState::Down,
            }],
        })
        .await
        .expect("the link is still alive after the unrepresentable event");

    // Two batches must reach the peer (the key-up from the mixed batch and the later press),
    // and the link must not have ended: the refusal used to be a link teardown.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let batches = seen
            .lock()
            .expect("lock")
            .iter()
            .filter(|(name, _)| *name == "batch")
            .count();
        if batches >= 2 || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let observed = seen.lock().expect("lock").clone();
    assert!(
        !link.task.is_finished(),
        "the link must still be up; the peer read {observed:?}"
    );
    let batches: Vec<usize> = observed
        .iter()
        .filter(|(name, _)| *name == "batch")
        .map(|(_, count)| *count)
        .collect();
    assert_eq!(
        batches,
        vec![1, 1],
        "the key-up of the mixed batch and the later press must both arrive; read {observed:?}"
    );

    reader.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// The inbound half of audit F4: a frame this node cannot read into its vocabulary — here an
/// absolute position with an empty `display_id`, which is exactly what our own encoder emits
/// before an `EnterPrepare` names a display — must cost that frame, not the connection.
#[tokio::test(flavor = "current_thread")]
async fn an_unreadable_inbound_batch_does_not_end_the_link() {
    let root = temp_root("inbound-bad-batch");
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
    let tcp = tokio::net::TcpStream::connect(address).await.expect("connect");
    tcp.set_nodelay(true).expect("nodelay");
    let tls = connector
        .connect(identity::server_name(address.ip()), tcp)
        .await
        .expect("client handshake");
    let mut peer = accepting.await.expect("the peer handshake finished");

    let (reports, mut report_rx) = tokio::sync::mpsc::channel::<LinkReport>(16);
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
        queue_capacity: 16,
        liveness: LinkLiveness::default(),
    })
    .expect("spawn a link");

    // Our link writes its `Hello` first (it is the dialer) and the peer adopts the connection
    // id from it, exactly as the handshake does.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let ours = read_frame(&mut peer).await.expect("our Hello");
    let connection_id = ours.connection_id.clone();
    assert_eq!(connection_id.len(), 16, "the dialer chose a connection id");

    // The peer answers our `Hello` (seq 1), then sends one batch with an empty display id
    // (seq 2) and one good batch (seq 3).
    let peer_hello = protocol::wire::Hello {
        node_id: server_id.as_bytes().to_vec(),
        display_name: "server".to_owned(),
        can_own: true,
        ..Default::default()
    };
    let envelope = |seq: u64, payload: protocol::wire::envelope::Payload| protocol::wire::Envelope {
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        connection_id: connection_id.clone(),
        seq,
        lease_id: Vec::new(),
        payload: Some(payload),
    };
    let batch = |x: f64, display: &str| {
        protocol::wire::envelope::Payload::InputBatch(protocol::wire::InputBatch {
            events: vec![protocol::wire::InputEvent {
                capture_monotonic_us: 0,
                event: Some(protocol::wire::input_event::Event::PointerAbsolute(
                    protocol::wire::Position {
                        display_id: display.to_owned(),
                        x,
                        y: 1.0,
                    },
                )),
            }],
        })
    };
    // `write_frame` on the peer's own direction: the connection id is adopted from our Hello
    // during the handshake, so an empty one is fine on the wire here.
    transport::write_frame(
        &mut peer,
        &envelope(1, protocol::wire::envelope::Payload::Hello(peer_hello)),
    )
    .await
    .expect("the peer hello is writable");
    transport::write_frame(&mut peer, &envelope(2, batch(9.0, "")))
        .await
        .expect("the bad batch is writable");
    transport::write_frame(&mut peer, &envelope(3, batch(10.0, "eDP-1")))
        .await
        .expect("the good batch is writable");

    // The good batch must reach the application, and the link must still be up.
    let mut batches = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(50), report_rx.recv()).await {
            Ok(Some(LinkReport::Message {
                message: transport::convert::Inbound::Focus(focus_router::Message::InputBatch { events, .. }),
                ..
            })) => {
                assert_eq!(events.len(), 1, "the good batch carries its one event");
                batches += 1;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }
    assert_eq!(batches, 1, "the readable batch must still arrive");
    assert!(
        !link.task.is_finished(),
        "an unreadable frame must not end the link"
    );

    let _ = std::fs::remove_dir_all(&root);
}
