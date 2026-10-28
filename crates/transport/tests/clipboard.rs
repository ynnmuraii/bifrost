//! Loopback integration with real TLS: the clipboard channel's round trip, its limits, the
//! grant bookkeeping, the `Bind`-first rule and the `KVM-042` isolation from the input link.
//!
//! Every identity lives in its own temp directory and every listener binds an ephemeral
//! loopback port, so nothing here touches the user's profile or a fixed port. No mock: the
//! clipboard connections are real TLS sessions on `47902`'s channel/ALPN.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use geometry::NodeId;
use identity::{now_unix_ms, IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::frame::{encode_message, Channel};
use protocol::wire;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use transport::convert::Inbound;
use transport::{
    dial_clipboard, BackoffBounds, ClipboardListener, ClipboardMessage, ClipboardViolation, Grants,
    LinkEvent, LinkLiveness, LinkManager, ManagerConfig, Outgoing,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous bounds: the tests wait for events, never for a fixed sleep.
const FORM_TIMEOUT: Duration = Duration::from_secs(20);

/// Every socket wait in this file is bounded.
///
/// A frame that never arrives must fail the test with the step that stalled, not hold a CI
/// runner until the job is cancelled: the round trip below once sat in `recv` for 75 minutes on
/// `windows-latest`. The whole 300 KiB transfer takes milliseconds on loopback, so this is
/// slack, not a deadline the transfer races against.
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Awaits I/O under [`IO_TIMEOUT`], naming the step in the panic.
async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    match tokio::time::timeout(IO_TIMEOUT, future).await {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {IO_TIMEOUT:?}"),
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lws-clipboard-tests-{}-{}-{label}",
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

/// Two trust stores in which each identity pins the other.
fn paired(a: &NodeIdentity, b: &NodeIdentity) -> (TrustStore, TrustStore) {
    let mut a_trust = TrustStore::new();
    a_trust.trust(TrustedPeer::paired(
        b.node_id(),
        "peer",
        None,
        now_unix_ms(),
    ));
    let mut b_trust = TrustStore::new();
    b_trust.trust(TrustedPeer::paired(
        a.node_id(),
        "peer",
        None,
        now_unix_ms(),
    ));
    (a_trust, b_trust)
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

async fn free_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind an ephemeral port");
    listener.local_addr().expect("local address")
}

/// A deterministic, UTF-8-safe payload of `len` bytes.
fn payload_of(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % 26) as u8).collect()
}

struct TestNode {
    manager: Arc<LinkManager>,
    events: mpsc::Receiver<LinkEvent>,
    runner: JoinHandle<()>,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

const LIVENESS: LinkLiveness = LinkLiveness {
    ping_ms: 1_000,
    offline_ms: 3_000,
};

/// Spawns an input/control link manager, the `mesh.rs` idiom.
async fn spawn_node(
    name: &'static str,
    identity: Arc<NodeIdentity>,
    listen: SocketAddr,
    trusted: &[(NodeId, SocketAddr)],
    dial: &[SocketAddr],
) -> TestNode {
    let id = identity.node_id();
    let mut trust = TrustStore::new();
    for (peer, address) in trusted {
        trust.trust(TrustedPeer::paired(
            *peer,
            "peer",
            Some(address.to_string()),
            now_unix_ms(),
        ));
    }
    let config = ManagerConfig {
        identity,
        trust,
        local_hello: hello_for(id, name),
        listen,
        peers: dial.to_vec(),
        max_members: 3,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
        pairing: Default::default(),
    };
    let (manager, events) = LinkManager::new(config).expect("manager");
    let manager = Arc::new(manager);
    let runner = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = manager.run().await;
        })
    };
    TestNode {
        manager,
        events,
        runner,
    }
}

async fn wait_for_members(node: &TestNode, wanted: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if node.manager.member_count().await == wanted {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
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

/// 1. A 300 KiB payload round-trips through `Outgoing` and `clipboard::Assembler`, chunked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transfer_round_trips_in_chunks() {
    let root = temp_root("roundtrip");
    let server = Arc::new(identity_at(&root, "server"));
    let client = identity_at(&root, "client");
    let (server_trust, client_trust) = paired(&server, &client);

    let listen = free_address().await;
    let mut listener = ClipboardListener::bind(Arc::clone(&server), server_trust.clone(), listen)
        .await
        .expect("clipboard listener");
    let address = listener.local_addr();

    let offer_id = [3u8; 16];
    let parent = [9u8; 16];
    let channel_id = [4u8; 16];
    let token = [5u8; 32];
    let payload = payload_of(300 * 1024);
    let expected_sha = clipboard::sha256(&payload);
    let payload_len = payload.len() as u64;
    let origin = *client.node_id().as_bytes();
    let expected_payload = payload.clone();
    let receiver_sha = expected_sha.clone();

    let (accept_result, dial_result) = within("the clipboard accept and dial", async {
        tokio::join!(
            listener.accept(),
            dial_clipboard(
                &client,
                &client_trust,
                server.node_id(),
                address,
                parent,
                channel_id,
                token,
            )
        )
    })
    .await;
    let (mut server_conn, bind) = accept_result.expect("accept");
    let mut client_conn = dial_result.expect("dial");

    assert_eq!(
        bind,
        ClipboardMessage::Bind {
            parent_connection_id: parent,
            channel_id,
            binding_token: token,
        },
        "accept() hands the caller the Bind it read"
    );
    assert_eq!(server_conn.channel_id, channel_id);
    assert_eq!(server_conn.peer, client.node_id());
    assert_eq!(client_conn.channel_id, channel_id);

    let sender = tokio::spawn(async move {
        client_conn
            .send(ClipboardMessage::Offer { offer_id,
            origin_node_id: origin,
            origin_version: 1,
            mime: clipboard::MIME.to_owned(),
            size_bytes: payload_len, secret_marked: true, sender_epoch: Vec::new(), relayed: false })
            .await
            .expect("offer");
        match client_conn.recv().await.expect("request") {
            ClipboardMessage::Request { offer_id: got } => {
                assert_eq!(got, offer_id, "the request names our offer");
            }
            other => panic!("unexpected message before Request: {other:?}"),
        }
        let mut outgoing =
            Outgoing::new(offer_id, payload, 1 << 40, payload_len, 0).expect("outgoing");
        let mut largest_chunk = 0usize;
        while let Some((message, _delay)) = outgoing.next(0) {
            if let ClipboardMessage::Data { data, .. } = &message {
                largest_chunk = largest_chunk.max(data.len());
            }
            client_conn.send(message).await.expect("chunk");
        }
        client_conn.close().await;
        largest_chunk
    });

    let receiver = tokio::spawn(async move {
        let mut assembler = clipboard::Assembler::new(offer_id, payload_len).expect("assembler");
        loop {
            match server_conn.recv().await.expect("clipboard recv") {
                ClipboardMessage::Offer {
                    size_bytes,
                    mime,
                    secret_marked,
                    ..
                } => {
                    assert_eq!(size_bytes, payload_len);
                    assert_eq!(mime, clipboard::MIME);
                    assert!(secret_marked, "secret classification survives the wire");
                    server_conn
                        .send(ClipboardMessage::Request { offer_id })
                        .await
                        .expect("request");
                }
                ClipboardMessage::Data {
                    offer_id: got,
                    chunk_index,
                    chunk_count,
                    data,
                } => {
                    assert_eq!(got, offer_id);
                    assert!(
                        data.len() <= 16384,
                        "every chunk is at most 16 KiB, got {}",
                        data.len()
                    );
                    assembler
                        .push(&offer_id, chunk_index, chunk_count, &data)
                        .expect("assembled chunk");
                    if chunk_index + 1 == chunk_count {
                        assert!(
                            chunk_count > 1,
                            "300 KiB must arrive in more than one chunk"
                        );
                    }
                }
                ClipboardMessage::Complete {
                    offer_id: got,
                    sha256,
                } => {
                    assert_eq!(got, offer_id);
                    assert_eq!(sha256, receiver_sha, "the hash ends the transfer");
                    let text = assembler.finish(&sha256).expect("UTF-8 and hash");
                    return text.into_bytes();
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    });

    let largest_chunk = within("the sender task", sender)
        .await
        .expect("sender task");
    let assembled = within("the receiver task", receiver)
        .await
        .expect("receiver task");
    assert_eq!(
        assembled, expected_payload,
        "the assembled payload is byte-equal"
    );
    assert_eq!(clipboard::sha256(&assembled), expected_sha);
    assert!(largest_chunk <= 16384);

    let _ = std::fs::remove_dir_all(&root);
}

/// 2. Every protocol limit is refused, and `Outgoing::new` honours the configured cap.
#[test]
fn every_limit_is_refused() {
    let id = [1u8; 16];
    let node = [2u8; 32];

    let too_large = ClipboardMessage::Offer { offer_id: id, origin_node_id: node, origin_version: 0, mime: clipboard::MIME.to_owned(), size_bytes: 1024 * 1024 + 1, secret_marked: false, sender_epoch: Vec::new(), relayed: false };
    assert_eq!(too_large.validate(), Err(ClipboardViolation::TooLarge));

    let big_chunk = ClipboardMessage::Data {
        offer_id: id,
        chunk_index: 0,
        chunk_count: 2,
        data: vec![0u8; 16 * 1024 + 1],
    };
    assert_eq!(big_chunk.validate(), Err(ClipboardViolation::BadChunk));

    let bad_count = ClipboardMessage::Data {
        offer_id: id,
        chunk_index: 0,
        chunk_count: 65,
        data: vec![0u8; 8],
    };
    assert_eq!(bad_count.validate(), Err(ClipboardViolation::BadCount));

    let bad_hash = ClipboardMessage::Complete {
        offer_id: id,
        sha256: vec![0u8; 31],
    };
    assert_eq!(bad_hash.validate(), Err(ClipboardViolation::BadHash));

    let bad_mime = ClipboardMessage::Offer { offer_id: id, origin_node_id: node, origin_version: 0, mime: "text/html".to_owned(), size_bytes: 10, secret_marked: false, sender_epoch: Vec::new(), relayed: false };
    assert_eq!(bad_mime.validate(), Err(ClipboardViolation::BadMime));

    // `BadId` is unreachable from `validate`: every identifier is a fixed-size array in the
    // `ClipboardMessage` enum, so only `from_envelope` (decoding arbitrary bytes) can see a
    // wrong length. This envelope has a 16-byte `offer_id` field carrying 15 bytes.
    let mut short = wire::Envelope {
        major: protocol::limits::PROTOCOL_MAJOR,
        minor: protocol::limits::PROTOCOL_MINOR,
        connection_id: vec![0u8; 16],
        seq: 1,
        lease_id: Vec::new(),
        payload: Some(wire::envelope::Payload::ClipboardRequest(
            wire::ClipboardRequest {
                offer_id: vec![0u8; 15],
            },
        )),
    };
    assert!(ClipboardMessage::from_envelope(&short).is_err());
    short.payload = None;
    assert!(ClipboardMessage::from_envelope(&short).is_err());

    // A well-formed message passes.
    assert_eq!(
        ClipboardMessage::Data {
            offer_id: id,
            chunk_index: 0,
            chunk_count: 1,
            data: vec![0u8; 8],
        }
        .validate(),
        Ok(())
    );

    // `Outgoing::new` refuses more than the configured `max_bytes`, and more than 1 MiB.
    assert_eq!(
        Outgoing::new(id, vec![0u8; 100], 1 << 20, 99, 0).err(),
        Some(ClipboardViolation::TooLarge)
    );
    assert_eq!(
        Outgoing::new(id, vec![0u8; 1024 * 1024 + 1], 1 << 20, u64::MAX, 0).err(),
        Some(ClipboardViolation::TooLarge)
    );
}

/// 3. `Grants` accepts a bind once, expires, checks parent and token, and drops its grants
///    when the parent link closes.
#[test]
fn grants_are_one_use_expiring_and_parent_scoped() {
    let parent = [7u8; 16];
    let other_parent = [8u8; 16];
    let mut grants = Grants::default();

    let (channel_id, token) = grants.issue(parent, 0);
    assert_eq!(grants.open(), 1);
    assert_eq!(grants.accept(&parent, &channel_id, &token, 0), Ok(()));
    assert_eq!(grants.open(), 0, "a bound grant is no longer open");
    assert_eq!(
        grants.accept(&parent, &channel_id, &token, 0),
        Err(ClipboardViolation::BadId),
        "the same token cannot bind twice"
    );

    // Expiry: `clipboard::CHANNEL_TTL_MS` after issue.
    let (channel_id, token) = grants.issue(parent, 0);
    assert_eq!(
        grants.accept(&parent, &channel_id, &token, clipboard::CHANNEL_TTL_MS + 1),
        Err(ClipboardViolation::BadId),
        "an expired grant is refused"
    );
    assert_eq!(grants.open(), 1, "a refused bind leaves the grant open");
    grants.close_parent(&parent);
    assert_eq!(grants.open(), 0);

    // A wrong parent or token is refused and the grant stays open until its parent closes.
    let (channel_id, token) = grants.issue(parent, 0);
    assert_eq!(
        grants.accept(&other_parent, &channel_id, &token, 0),
        Err(ClipboardViolation::BadId)
    );
    assert_eq!(
        grants.accept(&parent, &channel_id, &[0u8; 32], 0),
        Err(ClipboardViolation::BadId)
    );
    grants.close_parent(&other_parent);
    assert_eq!(grants.open(), 1, "closing another parent keeps this grant");
    grants.close_parent(&parent);
    assert_eq!(grants.open(), 0, "closing the parent drops its grants");
}

/// 4. A connection whose first frame is not a `Bind` is dropped; `accept()` keeps waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_frame_must_be_a_bind() {
    let root = temp_root("bind-first");
    let server = Arc::new(identity_at(&root, "server"));
    let client = identity_at(&root, "client");
    let (server_trust, client_trust) = paired(&server, &client);

    let listen = free_address().await;
    let mut listener = ClipboardListener::bind(Arc::clone(&server), server_trust.clone(), listen)
        .await
        .expect("clipboard listener");
    let address = listener.local_addr();
    let server_id = server.node_id();
    let client_id = client.node_id();

    // The offending dialer: a real TLS session whose first frame is an `Offer`, not a `Bind`.
    // It must be handshaken by `accept()` before the good dialer connects, so the two are
    // driven in one join: the bad task signals after writing its `Offer`, and the good task
    // waits for that signal before dialing.
    let bad_parent = [1u8; 16];
    let offer = ClipboardMessage::Offer { offer_id: [2u8; 16], origin_node_id: [3u8; 32], origin_version: 0, mime: clipboard::MIME.to_owned(), size_bytes: 10, secret_marked: false, sender_epoch: Vec::new(), relayed: false };
    let envelope = offer.to_envelope(1, &bad_parent).expect("envelope");
    let frame = encode_message(Channel::Clipboard, &envelope).expect("frame");
    let config = identity::clipboard_client_config(&client, &client_trust).expect("config");
    let connector = TlsConnector::from(Arc::new(config));
    let (offer_sent, offer_seen) = tokio::sync::oneshot::channel();

    let bad = tokio::spawn(async move {
        let tcp = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        let mut stream = connector
            .connect(identity::server_name(address.ip()), tcp)
            .await
            .expect("handshake");
        stream.write_all(&frame).await.expect("offer frame");
        let _ = offer_sent.send(());
        stream.shutdown().await.expect("shutdown");
    });

    let parent = [9u8; 16];
    let channel_id = [4u8; 16];
    let token = [5u8; 32];
    let good = tokio::spawn(async move {
        // Wait until the Offer-first session is on the wire, then dial with the `Bind`.
        offer_seen.await.expect("the bad dialer ran");
        dial_clipboard(
            &client,
            &client_trust,
            server_id,
            address,
            parent,
            channel_id,
            token,
        )
        .await
        .expect("dial")
    });

    let accepted = tokio::time::timeout(Duration::from_secs(15), async {
        let accept = listener.accept();
        let (accept_result, bad_result, good_result) = tokio::join!(accept, bad, good);
        bad_result.expect("bad dialer task");
        let client_conn = good_result.expect("good dialer task");
        (
            accept_result.expect("accept returns the Bind-first connection"),
            client_conn,
        )
    })
    .await
    .expect("accept() must skip the Offer-first connection and return the next one");

    let ((connection, bind), _client_conn) = accepted;
    assert_eq!(
        bind,
        ClipboardMessage::Bind {
            parent_connection_id: parent,
            channel_id,
            binding_token: token,
        },
        "the Offer-first connection was dropped and the next (Bind-first) one was returned"
    );
    assert_eq!(connection.channel_id, channel_id);
    assert_eq!(connection.peer, client_id);

    let _ = std::fs::remove_dir_all(&root);
}

/// 5. `KVM-042`: a 1 MiB clipboard transfer does not block key-up delivery on the input
/// link. The two channels are separate TLS sessions on separate ports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mebibyte_transfer_does_not_block_key_up() {
    let root = temp_root("kvm-042");
    let address_a = free_address().await;
    let address_b = free_address().await;
    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());
    let (trust_a, trust_b) = paired(&identity_a, &identity_b);

    // A normal input/control mesh of two nodes.
    let a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &[address_b],
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &[address_a],
    )
    .await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await, "a->b up");
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await, "b->a up");

    // A clipboard listener on A and a dialer from B: a separate session on its own port.
    let listen = free_address().await;
    let mut listener = ClipboardListener::bind(Arc::clone(&identity_a), trust_a.clone(), listen)
        .await
        .expect("clipboard listener");
    let clipboard_address = listener.local_addr();

    let offer_id = [11u8; 16];
    let parent = [12u8; 16];
    let channel_id = [13u8; 16];
    let token = [14u8; 32];
    let payload = payload_of(1024 * 1024);
    let expected_sha = clipboard::sha256(&payload);
    let payload_len = payload.len() as u64;

    let (accept_result, dial_result) = within("the clipboard accept and dial", async {
        tokio::join!(
            listener.accept(),
            dial_clipboard(
                &identity_b,
                &trust_b,
                id_a,
                clipboard_address,
                parent,
                channel_id,
                token,
            )
        )
    })
    .await;
    let (mut server_conn, _bind) = accept_result.expect("accept");
    let mut client_conn = dial_result.expect("dial");

    // The receiver: assemble 1 MiB while the sender paces it.
    let receiver = tokio::spawn(async move {
        let mut assembler = clipboard::Assembler::new(offer_id, payload_len).expect("assembler");
        loop {
            match server_conn.recv().await.expect("clipboard recv") {
                ClipboardMessage::Data {
                    offer_id,
                    chunk_index,
                    chunk_count,
                    data,
                } => {
                    assembler
                        .push(&offer_id, chunk_index, chunk_count, &data)
                        .expect("chunk");
                }
                ClipboardMessage::Complete { sha256, .. } => {
                    return assembler.finish(&sha256).expect("hash and UTF-8");
                }
                other => panic!("unexpected clipboard message: {other:?}"),
            }
        }
    });

    // 512 KiB/s: the bucket starts full, so ~1 MiB takes about a second.
    let sender = tokio::spawn(async move {
        let mut outgoing =
            Outgoing::new(offer_id, payload, 512 * 1024, u64::MAX, 0).expect("outgoing");
        let start = Instant::now();
        loop {
            let now_ms = start.elapsed().as_millis() as u64;
            let Some((message, delay)) = outgoing.next(now_ms) else {
                break;
            };
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if client_conn.send(message).await.is_err() {
                break;
            }
        }
        client_conn.close().await;
        start.elapsed()
    });

    // Let the transfer get in flight, then prove the input link is not blocked.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let key_up = focus_router::Message::InputBatch {
        lease_id: [21u8; 16],
        input_batch_seq: 1,
        events: vec![input_model::InputEvent::Key {
            key: input_model::HidKey::keyboard(0x04),
            state: input_model::KeyState::Up,
        }],
    };
    let handle = a.manager.handle(&id_b).await.expect("a handle to b");
    let sent = Instant::now();
    handle.send(key_up).await.expect("key-up queued");

    let delivered = wait_for_event(
        &mut b.events,
        |event| {
            matches!(
                event,
                LinkEvent::Message {
                    message: Inbound::Focus(focus_router::Message::InputBatch { .. }),
                    ..
                }
            )
        },
        Duration::from_secs(2),
    )
    .await;
    let latency = sent.elapsed();
    assert!(
        delivered.is_some(),
        "the key-up must arrive while the 1 MiB transfer is in flight"
    );
    assert!(
        latency < Duration::from_millis(500),
        "the input link must not be blocked by the clipboard transfer (took {latency:?})"
    );

    let received = within("the receiver task", receiver)
        .await
        .expect("receiver task");
    assert_eq!(
        received.len() as u64,
        payload_len,
        "the whole payload arrives"
    );
    assert_eq!(clipboard::sha256(received.as_bytes()), expected_sha);
    let transfer_time = within("the sender task", sender)
        .await
        .expect("sender task");
    assert!(
        transfer_time >= Duration::from_millis(400),
        "the transfer was actually paced, not sent instantly: {transfer_time:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Proves a dial was refused: either the TLS handshake failed, or the server dropped the
/// connection instead of serving it (the client may finish TLS 1.3 before the alert arrives).
async fn assert_refused(dial: Result<transport::ClipboardConnection, transport::TransportError>) {
    match dial {
        Err(_) => {}
        Ok(mut connection) => {
            let closed = tokio::time::timeout(Duration::from_secs(2), connection.recv()).await;
            assert!(
                matches!(closed, Ok(Err(_))),
                "a refused dial must close the connection, got {closed:?}"
            );
        }
    }
}

#[tokio::test]
async fn runtime_paired_peer_clipboard_works_and_revocation_refuses_handshake() {
    let root = temp_root("clipboard-pins");
    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let id_a = identity_a.node_id();
    let id_b = identity_b.node_id();

    // Node A starts with an EMPTY shared pin set (B is not yet paired).
    let a_pins = Arc::new(std::sync::RwLock::new(std::collections::BTreeSet::new()));
    let b_pins = Arc::new(std::sync::RwLock::new([id_a].into_iter().collect()));

    let address_a = free_address().await;
    let listener = ClipboardListener::bind(Arc::clone(&identity_a), Arc::clone(&a_pins), address_a)
        .await
        .expect("bind listener");
    let address_a = listener.local_addr();

    // `accept()` must be driven for any handshake to make progress (it hands each accepted
    // socket to its own task), so the test runs it in a task and collects the connections it
    // yields.
    let (accepted_tx, mut accepted_rx) = mpsc::channel(4);
    let acceptor = tokio::spawn(async move {
        let mut listener = listener;
        loop {
            match listener.accept().await {
                Ok(pair) => {
                    if accepted_tx.send(pair).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    // Before pairing: B dials A. A pins nobody, so its verifier refuses the handshake. TLS 1.3
    // lets the client finish its half before the server's rejection lands, so the proof is on
    // the server side: the connection is closed, never handed to the application.
    let before = within(
        "the pre-pairing dial",
        dial_clipboard(
            &identity_b,
            Arc::clone(&b_pins),
            id_a,
            address_a,
            [1; 16],
            [2; 16],
            [3; 32],
        ),
    )
    .await;
    assert_refused(before).await;
    assert!(
        accepted_rx.try_recv().is_err(),
        "a refused handshake must not be handed over"
    );

    // Runtime pairing: A adds B to its shared pins WITHOUT restarting the listener.
    a_pins.write().unwrap().insert(id_b);

    let conn_b = within(
        "the paired dial",
        dial_clipboard(
            &identity_b,
            Arc::clone(&b_pins),
            id_a,
            address_a,
            [1; 16],
            [2; 16],
            [3; 32],
        ),
    )
    .await
    .expect("the dial must succeed once B is pinned");
    assert_eq!(conn_b.peer, id_a);

    let (conn_a, bind_msg) = tokio::time::timeout(Duration::from_secs(5), accepted_rx.recv())
        .await
        .expect("the paired connection must be accepted without a restart")
        .expect("the accept task is alive");
    assert_eq!(conn_a.peer, id_b);
    assert!(matches!(bind_msg, ClipboardMessage::Bind { .. }));

    // Revoke: A removes B from its shared pins, and the handshake is refused again.
    a_pins.write().unwrap().remove(&id_b);
    let after = within(
        "the post-revocation dial",
        dial_clipboard(
            &identity_b,
            Arc::new(std::sync::RwLock::new([id_a].into_iter().collect())),
            id_a,
            address_a,
            [4; 16],
            [5; 16],
            [6; 32],
        ),
    )
    .await;
    assert_refused(after).await;
    assert!(
        accepted_rx.try_recv().is_err(),
        "a revoked peer's handshake must not be handed over"
    );

    acceptor.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// R6: a connection that stalls its handshake must not block the ones behind it.
///
/// `ClipboardListener::accept` used to TLS-accept and read the `Bind` inline, so one peer that
/// connected and said nothing held the listener until its socket died — a head-of-line DoS.
/// Each connection is handshaked in its own task now, so the valid dialer queued behind the
/// silent one is accepted at once.
#[tokio::test]
async fn a_silent_connection_does_not_block_a_valid_one() {
    let root = temp_root("clipboard-hol");
    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = identity_at(&root, "b");
    let id_a = identity_a.node_id();
    let id_b = identity_b.node_id();
    let address = free_address().await;

    let a_pins = Arc::new(std::sync::RwLock::new([id_b].into_iter().collect()));
    let b_pins = Arc::new(std::sync::RwLock::new([id_a].into_iter().collect()));
    let mut listener =
        ClipboardListener::bind(Arc::clone(&identity_a), Arc::clone(&a_pins), address)
            .await
            .expect("bind listener");

    // A socket that is accepted and then says nothing.
    let silent = tokio::net::TcpStream::connect(address)
        .await
        .expect("the silent connection");

    // A real dialer, queued behind it.
    let dial = tokio::spawn(async move {
        dial_clipboard(
            &identity_b,
            b_pins,
            id_a,
            address,
            [1; 16],
            [2; 16],
            [3; 32],
        )
        .await
    });

    let accepted = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await;
    let (connection, message) = accepted
        .expect("the valid dial must be accepted while a silent peer stalls")
        .expect("accept");
    assert_eq!(connection.peer, id_b);
    assert!(matches!(message, ClipboardMessage::Bind { .. }));

    let dialed = within("the queued dial", dial)
        .await
        .expect("join")
        .expect("dial");
    assert_eq!(dialed.peer, id_a);
    drop(silent);
    let _ = std::fs::remove_dir_all(&root);
}
