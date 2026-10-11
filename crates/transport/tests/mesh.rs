//! Loopback integration with real TLS: mesh formation and dedup, delivery, the liveness
//! timeout, the membership cap and an untrusted peer.
//!
//! Every identity lives in its own temp directory and every listener binds an ephemeral
//! loopback port, so nothing here touches the user's profile or a fixed port.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use focus_router::{ClaimReason, ErrorCode, Message};
use geometry::NodeId;
use identity::{now_unix_ms, IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::wire;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use transport::{BackoffBounds, LinkDownReason, LinkEvent, LinkLiveness, LinkManager, ManagerConfig};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous bounds: the tests wait for events, never for a fixed sleep.
const FORM_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(8);

/// The cadence every node in these tests runs with: the documented `1 s` / `3 s` that the
/// application passes in from `workspace.link_ping_ms` / `workspace.link_timeout_ms`.
const LIVENESS: LinkLiveness = LinkLiveness {
    ping_ms: 1_000,
    offline_ms: 3_000,
};

struct TestNode {
    name: &'static str,
    manager: Arc<LinkManager>,
    events: mpsc::Receiver<LinkEvent>,
    runner: JoinHandle<()>,
}

impl TestNode {
    /// Takes the node down the way a crash would: no `Shutdown`, sockets dropped.
    async fn kill(&self) {
        self.manager.kill_all().await;
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.runner.abort();
    }
}

fn temp_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lws-transport-tests-{}-{}-{label}",
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

async fn free_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind an ephemeral port");
    listener.local_addr().expect("local address")
}

/// Spawns a manager. `trusted` supplies both the pin set and the dial list — a node can
/// trust a peer it never dials, which is what a window-opener-style node looks like.
async fn spawn_node(
    name: &'static str,
    identity: Arc<NodeIdentity>,
    listen: SocketAddr,
    trusted: &[(NodeId, SocketAddr)],
    dial: &[SocketAddr],
    max_members: u8,
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
        max_members,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
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
        name,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_nodes_form_a_full_mesh_with_one_link_per_pair() {
    let root = temp_root("mesh");
    let address_a = free_address().await;
    let address_b = free_address().await;
    let address_c = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let identity_c = Arc::new(identity_at(&root, "c"));
    let (id_a, id_b, id_c) = (
        identity_a.node_id(),
        identity_b.node_id(),
        identity_c.node_id(),
    );

    // Everyone dials everyone: the simultaneous-dial dedup is the interesting path.
    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b), (id_c, address_c)],
        &[address_b, address_c],
        3,
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a), (id_c, address_c)],
        &[address_a, address_c],
        3,
    )
    .await;
    let mut c = spawn_node(
        "c",
        Arc::clone(&identity_c),
        address_c,
        &[(id_a, address_a), (id_b, address_b)],
        &[address_a, address_b],
        3,
    )
    .await;

    for node in [&a, &b, &c] {
        assert!(
            wait_for_members(node, 2, FORM_TIMEOUT).await,
            "{} did not reach two members",
            node.name
        );
    }

    // `members` must not contain a peer twice: one link per pair, whichever side won.
    for node in [&a, &b, &c] {
        let members = node.manager.members().await;
        let unique: BTreeSet<NodeId> = members.iter().copied().collect();
        assert_eq!(members.len(), 2, "{} members: {members:?}", node.name);
        assert_eq!(
            unique.len(),
            2,
            "{} has a duplicate link: {members:?}",
            node.name
        );
    }

    // The member set is exactly the two peers this node knows.
    let members_a: BTreeSet<NodeId> = a.manager.members().await.into_iter().collect();
    assert_eq!(members_a, [id_b, id_c].into_iter().collect());

    // Dedup invariant over a window: never more than one link per peer, and the mesh is
    // genuinely there (each node reaches two members again after any transient re-dial).
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reached_two = [false; 3];
    while Instant::now() < deadline {
        for (index, node) in [&a, &b, &c].into_iter().enumerate() {
            let members = node.manager.members().await;
            let unique: BTreeSet<NodeId> = members.iter().copied().collect();
            assert!(
                members.len() <= 2,
                "{} held {} links: {members:?}",
                node.name,
                members.len()
            );
            assert_eq!(
                members.len(),
                unique.len(),
                "{} had a duplicate link: {members:?}",
                node.name
            );
            if members.len() == 2 {
                reached_two[index] = true;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reached_two.iter().all(|seen| *seen),
        "every node must hold two members: {reached_two:?}"
    );

    for node in [&a, &b, &c] {
        node.kill().await;
    }
    for node in [&mut a, &mut b, &mut c] {
        while node.events.try_recv().is_ok() {}
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_untrusted_node_never_becomes_a_member() {
    let root = temp_root("untrusted");
    let address_a = free_address().await;
    let address_x = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_x = Arc::new(identity_at(&root, "x"));
    let id_a = identity_a.node_id();

    // Neither side has a trust record for the other: the pinned handshake cannot complete,
    // so no `Hello` is ever exchanged, on either the incoming or the outgoing side.
    let a = spawn_node("a", Arc::clone(&identity_a), address_a, &[], &[], 3).await;
    let x = spawn_node(
        "x",
        Arc::clone(&identity_x),
        address_x,
        &[],
        &[address_a],
        3,
    )
    .await;

    // Bounded observation window: the dialer keeps retrying with backoff, so the absence of
    // a link over several seconds is the assertion.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(x.manager.member_count().await, 0);
    assert_eq!(a.manager.member_count().await, 0);
    assert!(x.manager.handle(&id_a).await.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_membership_cap_refuses_the_extra_member() {
    let root = temp_root("membership");
    let address_a = free_address().await;
    let address_b = free_address().await;
    let address_c = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let identity_c = Arc::new(identity_at(&root, "c"));
    let (id_a, id_b, id_c) = (
        identity_a.node_id(),
        identity_b.node_id(),
        identity_c.node_id(),
    );

    // A accepts one member only and dials nobody; B and C both dial it, so one is refused.
    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b), (id_c, address_c)],
        &[],
        1,
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;
    let mut c = spawn_node(
        "c",
        Arc::clone(&identity_c),
        address_c,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;

    assert!(
        wait_for_members(&a, 1, FORM_TIMEOUT).await,
        "a admits one member"
    );
    let admitted = a.manager.members().await;
    assert_eq!(admitted.len(), 1);

    // The admitting side names the refusal with the documented reason.
    let refused_peer = if admitted[0] == id_b { id_c } else { id_b };
    let refusal = wait_for_event(
        &mut a.events,
        |event| matches!(event, LinkEvent::Down { peer, reason: LinkDownReason::MembershipFull } if *peer == refused_peer),
        EVENT_TIMEOUT,
    )
    .await;
    assert!(
        refusal.is_some(),
        "the admitting node must report MEMBERSHIP_FULL for the extra peer"
    );

    // «пятый член получает MEMBERSHIP_FULL»: the refused member has to be able to tell a
    // full workspace apart from a protocol failure, so its own link-down reason must be the
    // membership refusal too.
    let refused = if refused_peer == id_b { &mut b } else { &mut c };
    let observed = wait_for_event(
        &mut refused.events,
        |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == id_a),
        EVENT_TIMEOUT,
    )
    .await
    .expect("the refused member notices that it was not admitted");
    match observed {
        LinkEvent::Down { reason, .. } => assert_eq!(
            reason,
            LinkDownReason::MembershipFull,
            "the refused member must learn that membership is full"
        ),
        other => panic!("unexpected event {other:?}"),
    }

    // The admitted member keeps its link.
    assert_eq!(a.manager.member_count().await, 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_focus_message_is_delivered_after_the_link_has_been_alive() {
    let root = temp_root("delivery");
    let address_a = free_address().await;
    let address_b = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());

    let a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &[address_b],
        3,
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;

    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);

    // Let the link live past its first `Ping` (1 s period): an established link must keep
    // delivering messages, not only during the first second of its life.
    tokio::time::sleep(Duration::from_millis(LIVENESS.ping_ms * 2)).await;

    // A key-up must arrive as a key-up: the whole product promise rests on that.
    let batch_events = vec![input_model::InputEvent::Button {
        button: input_model::MouseButton::Left,
        state: input_model::KeyState::Up,
    }];
    let batch = Message::InputBatch {
        lease_id: [7u8; 16],
        // Deliberately not the envelope sequence: `InputBatch.input_batch_seq` **is** the
        // envelope `seq` on the wire («Sender's envelope sequence of this batch»), so the
        // transport stamps it and this field never travels (`to_envelope` documents it and
        // `tests/convert.rs` pins it). The assertion below is that the stamping happened.
        input_batch_seq: 1,
        events: batch_events.clone(),
    };
    let handle = a.manager.handle(&id_b).await.expect("a handle to b");
    handle
        .send(batch.clone())
        .await
        .expect("the batch is queued");

    let claim = Message::OwnerClaim {
        serial: 42,
        reason: ClaimReason::Takeover,
    };
    handle
        .send(claim.clone())
        .await
        .expect("the claim is queued");

    let received = wait_for_event(
        &mut b.events,
        |event| matches!(event, LinkEvent::Message { .. }),
        EVENT_TIMEOUT,
    )
    .await;
    match received {
        Some(LinkEvent::Message { peer, message }) => {
            assert_eq!(peer, id_a);
            let Message::InputBatch {
                lease_id,
                input_batch_seq,
                events,
            } = &message
            else {
                panic!("the first message must be the batch, got {message:?}");
            };
            assert_eq!(lease_id, &[7u8; 16], "the lease id travels on the envelope");
            assert_eq!(events, &batch_events, "the events must arrive unchanged");
            // The link spent `seq = 1` on `Hello` and more on liveness frames, so a batch
            // numbered `1` would mean the sender's field had been trusted and the peer's
            // `SeqValidator` skipped. It must carry the envelope sequence instead.
            assert!(
                *input_batch_seq > 1,
                "the batch carries the envelope sequence, not the caller's field: \
                 {input_batch_seq}"
            );
        }
        Some(other) => panic!("unexpected event {other:?}"),
        None => panic!(
            "no message arrived within {EVENT_TIMEOUT:?}; b saw {:?}",
            b.events.try_recv()
        ),
    }

    let second = wait_for_event(
        &mut b.events,
        |event| {
            matches!(
                event,
                LinkEvent::Message {
                    message: Message::OwnerClaim { .. },
                    ..
                }
            )
        },
        EVENT_TIMEOUT,
    )
    .await;
    match second {
        Some(LinkEvent::Message { peer, message }) => {
            assert_eq!(peer, id_a);
            assert_eq!(message, claim, "the second message must arrive unchanged");
        }
        Some(other) => panic!("unexpected event {other:?}"),
        None => panic!("the second message never arrived"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abrupt_kill_is_reported_as_a_liveness_timeout() {
    let root = temp_root("kill");
    let address_a = free_address().await;
    let address_b = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());

    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &[address_b],
        3,
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);

    // Let the simultaneous-dial dedup settle and drop its redundant-link noise, so the only
    // `Down` left to observe is the one the kill causes.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while a.events.try_recv().is_ok() {}
    while b.events.try_recv().is_ok() {}

    // A crash: no `Shutdown`, sockets dropped. B can only notice through the socket dying or
    // through the liveness clock («офлайн через 3 s без Pong»). The simultaneous-dial dedup
    // may still surface a `LocalClosed` for the redundant link, so those are ignored while
    // the detection is collected.
    a.kill().await;
    let started = Instant::now();
    let deadline = Instant::now() + OFFLINE_TIMEOUT;
    let mut detected: Option<LinkDownReason> = None;
    let mut graceful = false;
    while detected.is_none() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = wait_for_event(
            &mut b.events,
            |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == id_a),
            remaining,
        )
        .await;
        match event {
            Some(LinkEvent::Down { reason, .. }) => match reason {
                LinkDownReason::Timeout | LinkDownReason::Io(_) => detected = Some(reason),
                LinkDownReason::PeerShutdown => graceful = true,
                _ => {}
            },
            _ => break,
        }
    }

    assert!(
        detected.is_some(),
        "b must notice the dead peer through the socket or the liveness clock"
    );
    assert!(
        !graceful,
        "a killed process cannot produce a graceful shutdown"
    );
    assert!(
        started.elapsed() < OFFLINE_TIMEOUT,
        "a dead socket must not take longer than the liveness timeout to show up"
    );

    // The member list no longer contains the dead peer.
    let deadline = Instant::now() + Duration::from_secs(5);
    while b.manager.member_count().await != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(b.manager.member_count().await, 0);

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deliberate_shutdown_is_reported_as_such() {
    let root = temp_root("shutdown");
    let address_a = free_address().await;
    let address_b = free_address().await;

    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());

    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &[address_b],
        3,
    )
    .await;
    let b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);

    // The graceful path is a `Shutdown`, which the peer can tell apart from a crash.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while a.events.try_recv().is_ok() {}
    let handle = b.manager.handle(&id_a).await.expect("a handle to a");
    handle.close();

    let event = wait_for_event(
        &mut a.events,
        |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == id_b),
        EVENT_TIMEOUT,
    )
    .await
    .expect("a notices the shutdown");
    match event {
        LinkEvent::Down { reason, .. } => assert_eq!(reason, LinkDownReason::PeerShutdown),
        other => panic!("unexpected event {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_membership_refusal_can_be_read_back_from_the_wire() {
    // The manager refuses an extra member with `ProtocolError { code: MembershipFull }`;
    // the reader has to classify number 23, or the refused member cannot tell a full
    // workspace apart from a protocol failure. (This test used to pin the opposite, the
    // asymmetry where 23 fell through to `UnknownEnum`.)
    assert_eq!(ErrorCode::MembershipFull.as_u32(), 23);
    assert_eq!(
        transport::convert::error_code_from_i32(ErrorCode::MembershipFull.as_u32() as i32),
        Ok(ErrorCode::MembershipFull)
    );
    // An undefined number is still rejected, so the check did not become "anything goes".
    assert!(transport::convert::error_code_from_i32(23_000).is_err());
}
