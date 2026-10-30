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
        "bifrost-transport-tests-{}-{}-{label}",
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
    spawn_node_with_liveness(name, identity, listen, trusted, dial, max_members, LIVENESS).await
}

/// The same with an explicit cadence, for the tests that need pings faster than production's.
async fn spawn_node_with_liveness(
    name: &'static str,
    identity: Arc<NodeIdentity>,
    listen: SocketAddr,
    trusted: &[(NodeId, SocketAddr)],
    dial: &[SocketAddr],
    max_members: u8,
    liveness: LinkLiveness,
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
        liveness,
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

#[tokio::test]
async fn capability_refresh_reaches_live_link_without_replacing_it() {
    let root = temp_root("capability-refresh");
    let a = Arc::new(identity_at(&root, "a"));
    let b = Arc::new(identity_at(&root, "b"));
    let a_id = a.node_id();
    let b_id = b.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node("a", a, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    settle(&mut a, Duration::from_millis(100)).await;
    settle(&mut b, Duration::from_millis(100)).await;

    let mut hello = hello_for(a_id, "a");
    hello.capabilities = vec![wire::Capability::TextClipboard as i32, 12345];
    a.manager.update_local_hello(hello.clone()).await;
    let expected = hello.capabilities;
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            match b.events.recv().await.expect("manager event") {
                LinkEvent::Message {
                    peer,
                    message: Inbound::CapabilitiesChanged(capabilities),
                } if peer == a_id => {
                    assert_eq!(capabilities, expected);
                    break;
                }
                LinkEvent::Up { .. } | LinkEvent::Down { .. } => {
                    panic!("a capability refresh replaced the established link");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("capability refresh");
    assert_eq!(a.manager.member_count().await, 1);
    assert_eq!(b.manager.member_count().await, 1);
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn inbound_only_membership_is_symmetric_and_capture_refusal_keeps_it() {
    let root = temp_root("inbound-refusal");
    let a = Arc::new(identity_at(&root, "a"));
    let b = Arc::new(identity_at(&root, "b"));
    let a_id = a.node_id();
    let b_id = b.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node("a", a, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    settle(&mut a, Duration::from_millis(100)).await;
    settle(&mut b, Duration::from_millis(100)).await;
    let refusal = Message::ProtocolError {
        code: focus_router::ErrorCode::CaptureFailed,
        related_seq: 0,
    };
    a.manager
        .handle(&b_id)
        .await
        .unwrap()
        .send(refusal.clone())
        .await
        .unwrap();
    assert!(wait_for_event(
        &mut b.events,
        |event| matches!(
            event, LinkEvent::Message { peer, message: Inbound::Focus(message) }
                if *peer == a_id && *message == refusal
        ),
        EVENT_TIMEOUT
    )
    .await
    .is_some());
    b.manager
        .handle(&a_id)
        .await
        .unwrap()
        .send(refusal.clone())
        .await
        .unwrap();
    assert!(wait_for_event(
        &mut a.events,
        |event| matches!(
            event, LinkEvent::Message { peer, message: Inbound::Focus(message) }
                if *peer == b_id && *message == refusal
        ),
        EVENT_TIMEOUT
    )
    .await
    .is_some());
    assert_eq!(a.manager.members().await, vec![b_id]);
    assert_eq!(b.manager.members().await, vec![a_id]);
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn capability_refresh_during_formation_converges_to_latest_set() {
    let root = temp_root("capability-formation");
    let a = Arc::new(identity_at(&root, "a"));
    let b = Arc::new(identity_at(&root, "b"));
    let a_id = a.node_id();
    let b_id = b.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let a = spawn_node("a", a, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    // Race cache changes against TLS/Hello/registration. The final set must arrive
    // whether Hello captured it, Up caught it up, or an established-link push sent it.
    let final_capabilities = vec![wire::Capability::TextClipboard as i32];
    for capabilities in [
        vec![],
        vec![wire::Capability::Capture as i32],
        final_capabilities.clone(),
    ] {
        let mut hello = hello_for(a_id, "a");
        hello.capabilities = capabilities;
        a.manager.update_local_hello(hello).await;
        tokio::task::yield_now().await;
    }
    let observed = wait_for_event(
        &mut b.events,
        |event| match event {
            LinkEvent::Up { peer, hello, .. } => *peer == a_id && hello.text_clipboard,
            LinkEvent::Message {
                peer,
                message: Inbound::CapabilitiesChanged(capabilities),
            } => *peer == a_id && capabilities == &final_capabilities,
            _ => false,
        },
        FORM_TIMEOUT,
    )
    .await;
    assert!(
        observed.is_some(),
        "latest capabilities were lost during formation"
    );
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
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

/// Whether an event is a `Down` for `peer` (any reason).
fn is_peer_down(peer: NodeId) -> impl Fn(&LinkEvent) -> bool {
    move |event: &LinkEvent| matches!(event, LinkEvent::Down { peer: event_peer, .. } if *event_peer == peer)
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
    // The dialer retries inside this window, so several links to the same peer can end here:
    // the test is about the *refusal* being delivered, not about which of them ends first.
    // Waiting for the first `Down` of any kind and then demanding the reason made the test
    // flaky (`Io(... 10054)` from a reset connection instead of the refusal, ~1 run in 100
    // under load), which said nothing about whether the refusal arrived — this collector
    // reports every reason it saw when the refusal is missing.
    let mut observed: Vec<LinkDownReason> = Vec::new();
    let refusal_seen = {
        let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break false;
            }
            match tokio::time::timeout(
                left,
                wait_for_event(&mut refused.events, is_peer_down(id_a), left),
            )
            .await
            {
                Ok(Some(LinkEvent::Down { reason, .. })) => {
                    observed.push(reason.clone());
                    if reason == LinkDownReason::MembershipFull {
                        break true;
                    }
                }
                Ok(Some(other)) => panic!("unexpected event {other:?}"),
                Ok(None) => break false,
                Err(_) => break false,
            }
        }
    };
    assert!(
        refusal_seen,
        "the refused member must learn that membership is full; reasons seen: {observed:?}"
    );

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

    // And nothing puts the dead peer back: a dialer that survived the kill kept dialling the
    // survivor, the survivor accepted it, and the pair linked again — a dead peer that never
    // looked dead, which is exactly what this test saw on Windows (`Down{LocalClosed}` + `Up`
    // and then fifteen silent seconds, because the replacement link was perfectly healthy).
    // `kill_all` takes the dialers with the links now, the way a crash does.
    let resurrected = {
        let window = Instant::now() + Duration::from_millis(2_000);
        let mut resurrected = false;
        while let Ok(event) = tokio::time::timeout(
            window.saturating_duration_since(Instant::now()),
            b.events.recv(),
        )
        .await
        {
            match event {
                Some(LinkEvent::Up { peer, .. }) if peer == id_a => resurrected = true,
                Some(LinkEvent::Down {
                    peer,
                    reason: LinkDownReason::LocalClosed,
                }) if peer == id_a => resurrected = true,
                Some(_) => continue,
                None => break,
            }
        }
        resurrected
    };
    assert!(
        !resurrected,
        "the killed peer must not be linked again by its own dial loop"
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

    // Graceful close needs one established link, not simultaneous-dial dedup.
    // Keep one initiator; the formation races have their own dedicated tests.
    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b)],
        &[],
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
    settle(&mut a, Duration::from_millis(300)).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_all_reports_local_close_for_each_member() {
    let root = temp_root("kill-all-down");
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

    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b), (id_c, address_c)],
        &[],
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
    let c = spawn_node(
        "c",
        Arc::clone(&identity_c),
        address_c,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;

    assert!(wait_for_members(&a, 2, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&c, 1, FORM_TIMEOUT).await);
    settle(&mut a, Duration::from_millis(300)).await;

    a.manager.kill_all().await;
    assert_eq!(a.manager.member_count().await, 0);

    let mut removed = BTreeSet::new();
    while removed.len() < 2 {
        let event = wait_for_event(
            &mut a.events,
            |event| matches!(event, LinkEvent::Down { .. }),
            EVENT_TIMEOUT,
        )
        .await
        .expect("kill_all reports each removed member");
        match event {
            LinkEvent::Down { peer, reason } => {
                assert_eq!(reason, LinkDownReason::LocalClosed);
                removed.insert(peer);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(removed, [id_b, id_c].into_iter().collect());

    a.kill().await;
    b.kill().await;
    c.kill().await;
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

/// A healthy link is never replaced by a late redial.
///
/// This is what CI kept hitting: two nodes linked, a lease `Remote`, and then both ends logged
/// `link down reason=LocalClosed` + `link up` in the same millisecond — the established link
/// replaced, the session torn down. The cause was that [`policy::prefer_incoming`] decided
/// *every* `Up` that met a live link, not only the simultaneous dial it exists for: a dial
/// that completes after formation settled — a staggered start, a loser's redial loop, a
/// reconnect that overlapped the old link's last moments — carries the larger initiator, and
/// that alone used to be enough to replace a healthy link.
///
/// The setup below is that shape, deterministically: only the *smaller* id dials at formation,
/// so the mesh settles on its link, and a second manager — still the larger, still trusted —
/// dials the smaller node's address long afterwards. Its initiator beats the live one in
/// `prefer_incoming`; before the fix that comparison replaced the link. Now the dial is late by
/// [`policy::formation_overlap`], so it is refused: no `Down`, no `Up`, no member loss — and
/// the live link keeps carrying traffic. (The only replacement left is a dial that was already
/// forming when the live link came up, which is what the doc's «одновременный взаимный дозвон»
/// means and what `prefer_incoming` still decides.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_healthy_link_is_never_replaced_by_a_late_redial() {
    let root = temp_root("late-redial");
    let address_a = free_address().await;
    let address_b = free_address().await;
    let identity_a = Arc::new(identity_at(&root, "a"));
    let identity_b = Arc::new(identity_at(&root, "b"));
    let (id_a, id_b) = (identity_a.node_id(), identity_b.node_id());

    // Only the *smaller* id dials at formation, so the live link is the one it dialled and its
    // initiator is the smaller of the two — the state every settled mesh is in, and the state a
    // late dial from the larger id used to overturn.
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

    // The late dial comes from the *larger* id, and it is the smaller id that watches: its live
    // link is the one the late dial used to replace.
    let (watch_a, second_identity, peer_id, peer_address, watched_peer_id) = if id_b > id_a {
        (true, Arc::clone(&identity_b), id_a, address_a, id_b)
    } else {
        (false, Arc::clone(&identity_a), id_b, address_b, id_a)
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
    // Its own events must be drained or its sends block. Every `Up` the second manager sees is a
    // late dial the watched node refused: it holds no link of its own to that peer, so each dial
    // comes up, dies, and is retried — the churn that must stay invisible to the watched node.
    let late_dials = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&late_dials);
    let drain = tokio::spawn(async move {
        while let Some(event) = second_events.recv().await {
            if matches!(event, LinkEvent::Up { .. }) {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    // Wait until the refusal path has run a few times, or a quiet machine proves nothing: the
    // watched node has to stay silent *while* those refusals are happening.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while late_dials.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        late_dials.load(Ordering::SeqCst) >= 3,
        "the late dialer must keep being retried by its own loop (it has no link to that peer \
         on its own side), so the watched node really is refusing repeatedly"
    );

    let events = if watch_a {
        &mut a.events
    } else {
        &mut b.events
    };
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(format!("{event:?}"));
    }
    assert!(
        seen.is_empty(),
        "a late redial must leave the live link's owner in silence, not replace it: {seen:?}"
    );
    let watched = if watch_a { &a } else { &b };
    assert_eq!(
        watched.manager.member_count().await,
        1,
        "the refused dial must not cost the live link its member"
    );

    let handle = watched
        .manager
        .handle(&watched_peer_id)
        .await
        .expect("a handle over the live link");
    handle
        .send(Message::InputBatch {
            lease_id: [7u8; 16],
            input_batch_seq: 1,
            events: vec![input_model::InputEvent::Button {
                button: input_model::MouseButton::Left,
                state: input_model::KeyState::Up,
            }],
        })
        .await
        .expect("the batch is queued on the live link");
    let peer_events = if watch_a {
        &mut b.events
    } else {
        &mut a.events
    };
    let received = wait_for_event(
        peer_events,
        |event| matches!(event, LinkEvent::Message { .. }),
        EVENT_TIMEOUT,
    )
    .await;
    assert!(
        received.is_some(),
        "the live link must still deliver after the refused redial"
    );

    second.kill_all().await;
    second_runner.abort();
    drain.abort();
    let _ = std::fs::remove_dir_all(&root);
}

/// A dialer to an already-linked peer stays idle until that link goes down.
///
/// The loser of a dial used to keep dialling: its link lost the dedup, the manager closed it,
/// and the dialer read that as "the peer went away" — redial every backoff, a full TLS handshake
/// each time, all of it refused at the `Up` by the very manager that keeps the live link. A
/// dialer now takes its turn from the manager, and the manager only hands it over when the peer
/// has no link left. The proxy counts the connections the losing side actually makes: after
/// formation that count must not move, and after the live link is closed it must move again —
/// the idleness is a wait, not a wedge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dialer_to_a_linked_peer_stays_idle() {
    let root = temp_root("idle-dialer");
    let via_proxy_address = free_address().await;
    let direct_address = free_address().await;
    let proxy_address = free_address().await;

    let identity_first = Arc::new(identity_at(&root, "first"));
    let identity_second = Arc::new(identity_at(&root, "second"));
    // The survivor of a simultaneous dial is the link with the larger initiator, so the node
    // that dials *through the proxy* must be the one with the smaller id — those are the
    // connections being counted. The ids are random, so the roles are assigned to fit.
    let (proxy_identity, direct_identity) = if identity_first.node_id() < identity_second.node_id()
    {
        (Arc::clone(&identity_first), Arc::clone(&identity_second))
    } else {
        (Arc::clone(&identity_second), Arc::clone(&identity_first))
    };
    let (id_via_proxy, id_direct) = (proxy_identity.node_id(), direct_identity.node_id());

    // The proxy: plain TCP in front of the direct node's real listener, so the pinned handshake
    // and the `Hello` travel through it unchanged — it only forwards and counts.
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&connections);
    let backend = direct_address;
    let proxy = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(proxy_address)
            .await
            .expect("bind the proxy");
        while let Ok((mut client, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut upstream) = tokio::net::TcpStream::connect(backend).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                }
            });
        }
    });

    // The smaller id dials through the proxy (and loses), the larger dials directly (and wins).
    let mut via_proxy = spawn_node(
        "via-proxy",
        proxy_identity,
        via_proxy_address,
        &[(id_direct, direct_address)],
        &[proxy_address],
        3,
    )
    .await;
    let mut direct = spawn_node(
        "direct",
        direct_identity,
        direct_address,
        &[(id_via_proxy, via_proxy_address)],
        &[via_proxy_address],
        3,
    )
    .await;
    assert!(wait_for_members(&via_proxy, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&direct, 1, FORM_TIMEOUT).await);
    settle(&mut via_proxy, Duration::from_millis(300)).await;
    settle(&mut direct, Duration::from_millis(300)).await;

    // Formation's losing dial may still be in flight when the member count is right, so wait for
    // the counter to hold still before measuring. A dialer that loops is not slowed by this: it
    // moves the counter well inside the quiet bound below.
    let quiet = Duration::from_millis(1_200);
    let settle_deadline = Instant::now() + FORM_TIMEOUT;
    let mut last_change = Instant::now();
    let mut settled_at = connections.load(Ordering::SeqCst);
    while last_change.elapsed() < quiet && Instant::now() < settle_deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = connections.load(Ordering::SeqCst);
        if now != settled_at {
            settled_at = now;
            last_change = Instant::now();
        }
    }

    // Several backoffs' worth of silence: the losing dialer used to handshake through the proxy
    // every ~500 ms for the life of the mesh.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        settled_at,
        "a dialer whose peer is linked must not dial again until that link goes down"
    );

    // The heal: stop the live endpoint's link and dialers, leaving its listener up. That
    // isolates the waiting dialer's Down notification from a competing reconnect on this side.
    direct.manager.kill_all().await;
    let reconnect_deadline = Instant::now() + FORM_TIMEOUT;
    while connections.load(Ordering::SeqCst) == settled_at && Instant::now() < reconnect_deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        connections.load(Ordering::SeqCst) > settled_at,
        "after the link is gone the dialer must dial again — the permit is a wait, not a wedge"
    );
    assert!(wait_for_members(&via_proxy, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&direct, 1, FORM_TIMEOUT).await);

    via_proxy.kill().await;
    direct.kill().await;
    proxy.abort();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_drops_only_target_peer_and_preserves_other_links() {
    let root = temp_root("revoke-mesh");
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

    let mut a = spawn_node(
        "a",
        Arc::clone(&identity_a),
        address_a,
        &[(id_b, address_b), (id_c, address_c)],
        &[address_b, address_c],
        3,
    )
    .await;
    let b = spawn_node(
        "b",
        Arc::clone(&identity_b),
        address_b,
        &[(id_a, address_a), (id_c, address_c)],
        &[address_a, address_c],
        3,
    )
    .await;
    let c = spawn_node(
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
    settle(&mut a, Duration::from_millis(200)).await;

    // Now A revokes B via remove_peer.
    a.manager.remove_peer(&id_b).await.expect("remove_peer");
    // The link to B must be dropped on A.
    let down_b = wait_for_event(
        &mut a.events,
        |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == id_b),
        EVENT_TIMEOUT,
    )
    .await;
    assert!(down_b.is_some(), "link to B must be dropped when revoked");

    // The link to C must stay UP on A!
    assert_eq!(
        a.manager.member_count().await,
        1,
        "A must still have C as a member"
    );
    assert!(
        a.manager.handle(&id_c).await.is_some(),
        "handle to C must still be live"
    );
    assert!(
        a.manager.handle(&id_b).await.is_none(),
        "handle to B must be gone"
    );

    // Now bounce C: drop C's node, then start a new C with the same identity and address.
    c.kill().await;

    // A sees C go down.
    let down_c = wait_for_event(
        &mut a.events,
        |event| matches!(event, LinkEvent::Down { peer, .. } if *peer == id_c),
        OFFLINE_TIMEOUT,
    )
    .await;
    assert!(down_c.is_some(), "A must observe C go down");

    // Respawn C.
    let _c2 = spawn_node(
        "c2",
        Arc::clone(&identity_c),
        address_c,
        &[(id_a, address_a)],
        &[address_a],
        3,
    )
    .await;

    // A must redial / reconnect to C!
    assert!(
        wait_for_members(&a, 1, FORM_TIMEOUT).await,
        "A must reconnect to C after C bounces"
    );
    assert!(
        a.manager.handle(&id_c).await.is_some(),
        "handle to C must be live again"
    );
}

/// A live link measures its own `Ping`/`Pong` round trip, and the numbers leave with the
/// socket.
///
/// The cadence is sped up to 50 ms so three samples (the minimum for a statistic) arrive in
/// well under a second; nothing else about the link changes. The integration point is the
/// manager's `rtt`, exactly what core reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_measures_its_ping_round_trip_and_forgets_it_with_the_connection() {
    const FAST: LinkLiveness = LinkLiveness {
        ping_ms: 50,
        offline_ms: 3_000,
    };

    let root = temp_root("rtt");
    let a = Arc::new(identity_at(&root, "a"));
    let b = Arc::new(identity_at(&root, "b"));
    let a_id = a.node_id();
    let b_id = b.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node_with_liveness("a", a, a_addr, &[(b_id, b_addr)], &[], 4, FAST).await;
    let mut b =
        spawn_node_with_liveness("b", b, b_addr, &[(a_id, a_addr)], &[a_addr], 4, FAST).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await, "a linked");
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await, "b linked");
    settle(&mut a, Duration::from_millis(50)).await;
    settle(&mut b, Duration::from_millis(50)).await;

    // Three answered pings at a 50 ms cadence arrive in ~150 ms; poll instead of sleeping the
    // worst case. Loopback RTT is small but never zero, which is exactly the «known» test.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rtt = a.manager.rtt(&b_id).await.expect("a's link to b is up");
        if rtt.is_known() {
            assert!(rtt.p95_us > 0, "a measured round trip is never zero");
            break;
        }
        assert!(Instant::now() < deadline, "a measured no latency in 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Both ends measure: the Pong is symmetric, so b's own window is populated too.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rtt = b.manager.rtt(&a_id).await.expect("b's link to a is up");
        if rtt.is_known() {
            assert!(rtt.p95_us > 0);
            break;
        }
        assert!(Instant::now() < deadline, "b measured no latency in 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Latency belongs to the connection: once it is gone there is nothing to report, rather
    // than the last window of a socket that no longer exists.
    a.kill().await;
    assert_eq!(a.manager.rtt(&b_id).await, None);
}

#[tokio::test]
async fn remote_update_request_and_progress_cross_the_live_control_link_in_order() {
    let root = temp_root("remote-update");
    let a_identity = Arc::new(identity_at(&root, "a"));
    let b_identity = Arc::new(identity_at(&root, "b"));
    let a_id = a_identity.node_id();
    let b_id = b_identity.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let mut a = spawn_node("a", a_identity, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b_identity, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);
    let handle = a.manager.handle(&b_id).await.unwrap();
    let request = wire::UpdateRequest {
        target_version: "1.2.3".into(),
    };
    handle.send_update_request(request.clone()).await.unwrap();
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            if let LinkEvent::Message {
                peer,
                message: Inbound::UpdateRequest(received),
            } = b.events.recv().await.expect("request event")
            {
                assert_eq!(peer, a_id);
                assert_eq!(received, request);
                break;
            }
        }
    })
    .await
    .expect("request delivered");
    let handle = b.manager.handle(&a_id).await.unwrap();
    for status in ["pending", "downloading", "restarting", "done"] {
        handle
            .send_update_result(wire::UpdateResult {
                target_version: "1.2.3".into(),
                status: status.into(),
                detail: String::new(),
            })
            .await
            .unwrap();
    }
    tokio::time::timeout(EVENT_TIMEOUT, async {
        for expected in ["pending", "downloading", "restarting", "done"] {
            loop {
                if let LinkEvent::Message {
                    peer,
                    message: Inbound::UpdateResult(result),
                } = a.events.recv().await.expect("progress event")
                {
                    assert_eq!(peer, b_id);
                    assert_eq!(result.target_version, "1.2.3");
                    assert_eq!(result.status, expected);
                    break;
                }
            }
        }
    })
    .await
    .expect("ordered progress delivered");
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn clipboard_routes_and_apply_acks_cross_the_live_control_link() {
    let root = temp_root("clipboard-routing");
    let a_identity = Arc::new(identity_at(&root, "a"));
    let b_identity = Arc::new(identity_at(&root, "b"));
    let a_id = a_identity.node_id();
    let b_id = b_identity.node_id();
    let a_addr = free_address().await;
    let b_addr = free_address().await;
    let a = spawn_node("a", a_identity, a_addr, &[(b_id, b_addr)], &[], 4).await;
    let mut b = spawn_node("b", b_identity, b_addr, &[(a_id, a_addr)], &[a_addr], 4).await;
    assert!(wait_for_members(&a, 1, FORM_TIMEOUT).await);
    assert!(wait_for_members(&b, 1, FORM_TIMEOUT).await);

    let route = wire::ClipboardRoute {
        action: "deliver".into(),
        origin_node_id: a_id.as_bytes().to_vec(),
        origin_version: 7,
        sha256: vec![2; 32],
        target_node_id: b_id.as_bytes().to_vec(),
    };
    let applied = wire::ClipboardApplied {
        origin_version: 7,
        sha256: vec![2; 32],
        ok: false,
        detail: "backend refused selection ownership".into(),
    };
    let handle = a.manager.handle(&b_id).await.unwrap();
    handle.send_clipboard_route(route.clone()).await.unwrap();
    handle
        .send_clipboard_applied(applied.clone())
        .await
        .unwrap();
    tokio::time::timeout(EVENT_TIMEOUT, async {
        for expected in [
            Inbound::ClipboardRoute(route),
            Inbound::ClipboardApplied(applied),
        ] {
            loop {
                if let LinkEvent::Message { peer, message } =
                    b.events.recv().await.expect("clipboard routing event")
                {
                    if !matches!(
                        &message,
                        Inbound::ClipboardRoute(_) | Inbound::ClipboardApplied(_)
                    ) {
                        continue;
                    }
                    assert_eq!(peer, a_id);
                    assert_eq!(message, expected);
                    break;
                }
            }
        }
    })
    .await
    .expect("ordered clipboard control messages delivered");
    a.kill().await;
    b.kill().await;
    let _ = std::fs::remove_dir_all(root);
}

#[path = "input_pipeline/loopback.rs"]
mod input_pipeline;
