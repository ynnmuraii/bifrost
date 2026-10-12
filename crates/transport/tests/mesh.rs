//! Loopback integration with real TLS: mesh formation and dedup, delivery, the liveness
//! timeout, the membership cap and an untrusted peer.
//!
//! Every identity lives in its own temp directory and every listener binds an ephemeral
//! loopback port, so nothing here touches the user's profile or a fixed port.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use focus_router::{ClaimReason, ErrorCode, Message};
use geometry::NodeId;
use identity::{now_unix_ms, IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::wire;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use transport::convert::Inbound;
use transport::{
    BackoffBounds, LinkDownReason, LinkEvent, LinkLiveness, LinkManager, ManagerConfig,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Generous bounds: the tests wait for events, never for a fixed sleep.
const FORM_TIMEOUT: Duration = Duration::from_secs(20);
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

/// The cadence every node in these tests runs with: the documented `1 s` / `3 s` that the
/// application passes in from `workspace.link_ping_ms` / `workspace.link_timeout_ms`.
const LIVENESS: LinkLiveness = LinkLiveness {
    ping_ms: 1_000,
    offline_ms: 3_000,
};

/// What the test allows on top of [`LIVENESS`]'s window before it calls a report late.
///
/// The link decides on its *own* monotonic clock (`policy::liveness_action` compares
/// milliseconds since the last `Pong`), so a descheduled link task declares the peer dead the
/// instant it is scheduled again: the delay belongs to the machine, not to the product. The
/// product's deadline is therefore pinned where no scheduler can move it — `policy`'s own unit
/// tests assert the action flips at exactly `offline_ms` — and this integration bound only has
/// to be generous enough that a starved process still answers. Measured: the socket-death path
/// reports in well under a second on an idle machine, and a box running two full workspace
/// suites at once stretched it past the old fixed 8 s, which is how this bound was found.
const SCHEDULING_ALLOWANCE_MS: u64 = 12_000;

/// The slowest honest report a starved process may give: the documented offline window plus the
/// allowance above.
const OFFLINE_TIMEOUT: Duration =
    Duration::from_millis(LIVENESS.offline_ms + SCHEDULING_ALLOWANCE_MS);

/// How long the member bookkeeping may lag the link's `Down` report.
///
/// A second, separately bounded wait: removing the member is the manager's step after the
/// link's report, so it may not take longer than the report itself was allowed.
const MEMBER_DROP_TIMEOUT: Duration = OFFLINE_TIMEOUT;

struct TestNode {
    name: &'static str,
    manager: Arc<LinkManager>,
    events: mpsc::Receiver<LinkEvent>,
    runner: JoinHandle<()>,
}

impl TestNode {
    /// Takes the node down the way a crash would: no `Shutdown`, the listener and every link
    /// gone with the process.
    ///
    /// [`LinkManager::kill_all`] alone is *not* a crash: the manager's own task keeps the
    /// listener, so a surviving peer's redial is accepted, the pair is linked again, and the
    /// dead peer never looks dead. That is how this suite failed under load — the test saw an
    /// `Up` (the reconnect) and waited for a `Down` that a linked peer does not produce.
    /// Aborting the runner closes the listener, so the redial is refused and the survivor has
    /// only the socket death and the liveness clock to go on, which is the subject here.
    async fn kill(&self) {
        self.manager.kill_all().await;
        self.runner.abort();
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

/// Waits until a node's own link events go quiet: formation is two dials that race, and the
/// loser's `Down` — or a late winner's `Up` — can arrive *after* the member count is right. A
/// test that starts timing before that confuses formation's churn with the event it is about.
async fn settle(node: &mut TestNode, quiet: Duration) {
    let deadline = Instant::now() + FORM_TIMEOUT;
    let mut last = Instant::now();
    while Instant::now() < deadline && last.elapsed() < quiet {
        match tokio::time::timeout(quiet, node.events.recv()).await {
            Ok(Some(_)) => last = Instant::now(),
            Ok(None) | Err(_) => break,
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
            let Inbound::Focus(Message::InputBatch {
                lease_id,
                input_batch_seq,
                events,
            }) = &message
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
                    message: Inbound::Focus(Message::OwnerClaim { .. }),
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
            let Inbound::Focus(message) = message else {
                panic!("the second message is a focus message, got {message:?}");
            };
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
    // the detection is collected — and every event is kept for the message below, because
    // "nothing was reported" and "something else was reported" are different failures.
    a.kill().await;
    let started = Instant::now();
    let deadline = Instant::now() + OFFLINE_TIMEOUT;
    let mut detected: Option<LinkDownReason> = None;
    let mut graceful = false;
    let mut seen: Vec<String> = Vec::new();
    while detected.is_none() && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, b.events.recv()).await {
            // Nothing at all in the window: the manager's own channel closed.
            Ok(None) => break,
            // The window is out.
            Err(_) => break,
            Ok(Some(event)) => {
                match &event {
                    LinkEvent::Down { peer, reason } if *peer == id_a => match reason {
                        LinkDownReason::Timeout | LinkDownReason::Io(_) => {
                            detected = Some(reason.clone());
                        }
                        LinkDownReason::PeerShutdown => graceful = true,
                        _ => {}
                    },
                    _ => {}
                }
                seen.push(format!("{event:?}"));
            }
        }
    }

    // The waited-out time is the evidence for the bounds below, printed whichever way the run
    // went: on an idle machine the socket death reports in well under a second, a loaded one
    // stretches it, and the product's own deadline is not what moved (the `policy` tests pin
    // that at exactly `offline_ms`).
    println!(
        "b waited {:?} for the dead peer (liveness window {:?}); detected {detected:?}, graceful {graceful}, events {seen:?}",
        started.elapsed(),
        Duration::from_millis(LIVENESS.offline_ms)
    );
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

    // The member list no longer contains the dead peer. This is a second, separately bounded
    // wait: the link's `Down` report and the manager's member bookkeeping are different steps,
    // and a loaded machine stretches both.
    let deadline = Instant::now() + MEMBER_DROP_TIMEOUT;
    while b.manager.member_count().await != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        b.manager.member_count().await,
        0,
        "the dead peer must leave the member list within {MEMBER_DROP_TIMEOUT:?}"
    );

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

/// A dial must end: a peer that accepts the connection and then says nothing must be retried.
///
/// `TcpStream::connect` and the TLS handshake have no deadline of their own, so a blackholed
/// route (or a suspended core, or a half-open path) used to park the dialer for ever — and with
/// it the whole reconnect schedule: the node never dialed that peer again while it lived, so a
/// partition never healed. The layout suite's heal waits were the messenger for it on the
/// two-CPU CI runner («the follower adopts the coordinator's revision did not happen»); this
/// test is the mechanism, with no proxy or core in the way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_peer_is_retried_instead_of_parking_the_dialer() {
    // The peer: accepts, holds the connection, never answers. The dialer's handshake waits for
    // a `ServerHello` that never comes, which is what a blackholed route looks like.
    let blackhole = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("a blackhole listener");
    let address = blackhole.local_addr().expect("its address");
    let attempts = Arc::new(AtomicUsize::new(0));
    let held: Arc<std::sync::Mutex<Vec<tokio::net::TcpStream>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = Arc::clone(&attempts);
    let keep = Arc::clone(&held);
    tokio::spawn(async move {
        while let Ok((stream, _)) = blackhole.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            // Holding the stream is the point: closing it would end the handshake and let the
            // dial fail fast instead of hanging.
            keep.lock().expect("held streams").push(stream);
        }
    });

    let root = temp_root("silent-peer");
    let identity = Arc::new(identity_at(&root, "dialer"));
    let id = identity.node_id();
    let silent = identity_at(&root, "silent").node_id();
    let mut trust = TrustStore::new();
    trust.trust(TrustedPeer::paired(
        silent,
        "silent",
        Some(address.to_string()),
        now_unix_ms(),
    ));
    let (manager, _events) = LinkManager::new(ManagerConfig {
        identity,
        trust,
        local_hello: hello_for(id, "dialer"),
        listen: "127.0.0.1:0".parse().expect("a listen address"),
        peers: vec![address],
        max_members: 3,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
        pairing: transport::PairingGate::closed(),
    })
    .expect("manager");
    let manager = Arc::new(manager);
    let runner = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            let _ = manager.run().await;
        })
    };

    // One attempt may last `LIVENESS.offline_ms` (the dial deadline in `dial_loop`) and a retry
    // adds the first backoff step; three deadlines plus two seconds is far past a second
    // attempt, and a dialer that parks on the first one never gets there.
    let bound = Duration::from_millis(LIVENESS.offline_ms * 3 + 2_000);
    let deadline = Instant::now() + bound;
    while attempts.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let seen = attempts.load(Ordering::SeqCst);
    runner.abort();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        seen >= 2,
        "a silent peer parked the dialer: {seen} attempt(s) in {bound:?} — with no deadline on \
         `TcpStream::connect` plus the TLS handshake the reconnect schedule never runs again"
    );
}

/// A link that replaces a live one must be reported as a replacement, not swapped in silence.
///
/// Both sides dial at formation and [`policy::prefer_incoming`] keeps one of the two; the
/// loser's dialer redials after its link ends, and when that redial *wins* the comparison it
/// replaces the link the application is using. That replacement used to be silent — the manager
/// closed the old link and sent only the new `Up` — so the application kept reading a link that
/// was already gone. In a three-node CI run an in-flight `LeaveAck` died with it and the leaving
/// node sat in `Releasing` with nothing left to read. The replaced link is now reported
/// `Down { LocalClosed }`, which is what that reason documents, before the new `Up`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_that_replaces_a_live_one_is_reported_as_a_replacement() {
    let root = temp_root("replacement");
    let address_a = free_address().await;
    let address_b = free_address().await;
    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());

    // Only the *smaller* id dials at formation, so the live link is the one it dialled and its
    // initiator is the smaller of the two. The larger node then dials second, and its initiator
    // beats the live one — that comparison is what replaces a link. (With both sides dialing at
    // formation the larger id's link wins on both ends and nothing can beat it, so no
    // replacement could happen at all.)
    let dial_a: Vec<SocketAddr> = if id_a < id_b {
        vec![address_b]
    } else {
        Vec::new()
    };
    let dial_b: Vec<SocketAddr> = if id_a < id_b {
        Vec::new()
    } else {
        vec![address_a]
    };
    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &dial_a,
        3,
    )
    .await;
    let mut b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a)],
        &dial_b,
        3,
    )
    .await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    // Formation's own events are not this test's subject — and they are not over when the
    // member count is right: wait for the churn to stop.
    settle(&mut a, Duration::from_millis(600)).await;
    settle(&mut b, Duration::from_millis(600)).await;

    // The second dial comes from the *larger* id, and it is the smaller id that watches: its
    // live link is the one that gets replaced.
    let dial_second_from_b = id_b > id_a;
    let (watch_a, second_identity, peer_id, peer_address) = if dial_second_from_b {
        (true, Arc::clone(&identity_b), id_a, address_a)
    } else {
        (false, Arc::clone(&identity_a), id_b, address_b)
    };

    let mut trust = TrustStore::new();
    trust.trust(TrustedPeer::paired(
        peer_id,
        "peer",
        Some(peer_address.to_string()),
        now_unix_ms(),
    ));
    let second_id = second_identity.node_id();
    let (second, mut second_events) = LinkManager::new(ManagerConfig {
        identity: second_identity,
        trust,
        local_hello: hello_for(second_id, "second"),
        listen: free_address().await,
        peers: vec![peer_address],
        max_members: 3,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
        pairing: transport::PairingGate::closed(),
    })
    .expect("the second manager");
    let second = Arc::new(second);
    let second_runner = {
        let second = Arc::clone(&second);
        tokio::spawn(async move {
            let _ = second.run().await;
        })
    };
    // Its own events must be drained or its sends block.
    let drain = tokio::spawn(async move { while second_events.recv().await.is_some() {} });

    let events = if watch_a {
        &mut a.events
    } else {
        &mut b.events
    };
    let replacement = wait_for_event(
        events,
        |event| {
            matches!(
                event,
                LinkEvent::Down {
                    reason: LinkDownReason::LocalClosed,
                    ..
                }
            )
        },
        EVENT_TIMEOUT,
    )
    .await;
    assert!(
        replacement.is_some(),
        "the replaced link must be reported down, not swapped in silence"
    );
    let up = wait_for_event(
        events,
        |event| matches!(event, LinkEvent::Up { .. }),
        EVENT_TIMEOUT,
    )
    .await;
    assert!(
        up.is_some(),
        "the replacement must come up after the report"
    );

    second_runner.abort();
    drain.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// A second link in the *same* direction must not evict the live one.
///
/// The manager keyed its link maps by `(peer, direction)`, and that pair is not unique: a peer
/// that dials twice is two links in one direction — the late-redial case, and the
/// simultaneous-dial case when one side's dial arrives after the other's. The newcomer loses
/// [`policy::prefer_incoming`] (two dials from the same node tie, and a tie goes to the existing
/// link) and the manager closed it; its `Down` then matched the *live* link's direction and
/// evicted the live entry from the member list, so the application was told that a link which
/// was still up had gone down. Links are keyed by the key the manager hands each one now, so a
/// report can only ever change the state of the link that sent it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_link_in_the_same_direction_cannot_evict_the_live_one() {
    let root = temp_root("same-direction");
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
    // Formation's own events are not this test's subject — and they are not over when the
    // member count is right: wait for the churn to stop, or a late `Up`/`Down` from the race
    // lands in the window this test measures.
    settle(&mut a, Duration::from_millis(600)).await;
    settle(&mut b, Duration::from_millis(600)).await;

    // The link that survives a simultaneous dial is the one the *larger* node id dialled
    // (`prefer_incoming` keeps the larger initiator), so a second dial from that same node ties
    // and loses — deterministically, whichever ids the test happened to generate. Dial again
    // from the larger id and watch the other node: its live link is the one the newcomer's
    // `Down` must not touch.
    let (second_identity, watch_b) = if id_a > id_b {
        (Arc::clone(&identity_a), true)
    } else {
        (Arc::clone(&identity_b), false)
    };
    let (watched_id, watched_address) = if watch_b {
        (id_b, address_b)
    } else {
        (id_a, address_a)
    };

    let mut trust = TrustStore::new();
    trust.trust(TrustedPeer::paired(
        watched_id,
        "peer",
        Some(watched_address.to_string()),
        now_unix_ms(),
    ));
    let second_id = second_identity.node_id();
    let (second, mut second_events) = LinkManager::new(ManagerConfig {
        identity: second_identity,
        trust,
        local_hello: hello_for(second_id, "second"),
        listen: free_address().await,
        peers: vec![watched_address],
        max_members: 3,
        queue_capacity: 64,
        liveness: LIVENESS,
        backoff: BackoffBounds::default(),
        pairing: transport::PairingGate::closed(),
    })
    .expect("the second manager");
    let second = Arc::new(second);
    let second_runner = {
        let second = Arc::clone(&second);
        tokio::spawn(async move {
            let _ = second.run().await;
        })
    };
    // Its own events must be drained or its sends block.
    let drain = tokio::spawn(async move { while second_events.recv().await.is_some() {} });

    // The newcomer is closed as soon as its `Up` loses, and its `Down` arrives right after —
    // that is the report that used to be attributed to the live link. The window is short on
    // purpose: far longer than that round trip, far shorter than the 3 s liveness window, so a
    // loaded box cannot make the *live* link fail inside it and confuse the two.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let downs: Vec<LinkDownReason> = {
        let events = if watch_b {
            &mut b.events
        } else {
            &mut a.events
        };
        let mut reasons = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let LinkEvent::Down { reason, .. } = event {
                reasons.push(reason);
            }
        }
        reasons
    };
    let watched = if watch_b { &b } else { &a };
    let still_a_member = wait_for_members(watched, 1, FORM_TIMEOUT).await;

    second_runner.abort();
    drain.abort();
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        downs.is_empty(),
        "a closed newcomer in the live link's direction must not be reported as its `Down`: \
         {downs:?}"
    );
    assert!(
        still_a_member,
        "the live link must stay a member while a same-direction newcomer is refused"
    );
}
