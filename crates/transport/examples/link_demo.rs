//! Three in-process nodes on loopback: mesh formation, dedup, liveness, restart.
//!
//! What it shows (KVM-070/032):
//!
//! 1. three managers listening on loopback and dialling each other form a full mesh —
//!    each node reports two members, and only two, even though all three dialed at once
//!    (the simultaneous-dial rule keeps the link whose initiator has the larger `node_id`);
//! 2. an `Envelope` round trip: a focus/lease message sent on one link arrives, converted,
//!    on the other side;
//! 3. killing a node makes the others report it offline within ~3 s (`Ping` every 1 s, no
//!    `Pong` for 3 s) — with no `Shutdown`, so it is the timeout path;
//! 4. restarting that node with the *same* identity re-forms its links (the peer identity
//!    is pinned, so a new socket is enough).
//!
//! Run with: `cargo run -p transport --example link_demo`

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use focus_router::{ClaimReason, ErrorCode, Message};
use identity::{now_unix_ms, IdentityPaths, NodeIdentity, TrustStore, TrustedPeer};
use protocol::wire;
use tokio::task::JoinHandle;
use transport::{LinkEvent, LinkManager, ManagerConfig, NodeId};

struct Node {
    name: &'static str,
    id: NodeId,
    manager: Arc<LinkManager>,
    /// Drains this node's events so the mesh is visible while it forms.
    log: JoinHandle<()>,
    /// The manager's own loop (listener + dialers), so the demo can take a node down
    /// completely and later restart it with the same identity.
    runner: JoinHandle<()>,
}

fn hello_for(id: NodeId, name: &str) -> wire::Hello {
    wire::Hello {
        node_id: id.as_bytes().to_vec(),
        display_name: name.to_owned(),
        can_own: true,
        session_kind: wire::SessionKind::Wayland as i32,
        ..Default::default()
    }
}

async fn free_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .expect("bind");
    listener.local_addr().expect("addr")
}

async fn spawn_node(
    name: &'static str,
    identity: Arc<NodeIdentity>,
    listen: SocketAddr,
    others: Vec<(NodeId, SocketAddr)>,
) -> Node {
    let id = identity.node_id();
    println!("  {name}: {id} listening on {listen}");

    let mut trust = TrustStore::new();
    for (peer, address) in &others {
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
        peers: others.iter().map(|(_, address)| *address).collect(),
        max_members: 3,
        queue_capacity: 64,
    };
    let (manager, events) = LinkManager::new(config).expect("manager");
    let manager = Arc::new(manager);
    let manager_loop = Arc::clone(&manager);
    let runner = tokio::spawn(async move {
        let _ = manager_loop.run().await;
    });

    let mut log_events = events;
    let label = name;
    let log = tokio::spawn(async move {
        while let Some(event) = log_events.recv().await {
            match event {
                LinkEvent::Up { peer, hello } => {
                    println!(
                        "  [{label}] link up with {peer} (peer hello: {})",
                        hello.display_name
                    )
                }
                LinkEvent::Message { peer, message } => {
                    println!("  [{label}] <- {peer}: {message:?}")
                }
                LinkEvent::Down { peer, reason } => {
                    println!("  [{label}] link down with {peer}: {reason:?}")
                }
            }
        }
    });

    Node {
        name,
        id,
        manager,
        log,
        runner,
    }
}

async fn wait_for_members(node: &Node, wanted: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if node.manager.member_count().await == wanted {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join("lan-workspace-link-demo");
    std::fs::remove_dir_all(&root).ok();
    println!("== identities and managers ==");

    // Reserve three addresses first: the managers bind them, so the ports must be known
    // before the peers lists are built.
    let address_a = free_address().await;
    let address_b = free_address().await;
    let address_c = free_address().await;

    // Identities first: each node's trust store needs the other two ids, and creating them
    // up front also keeps them alive for the restart at the end of the demo.
    let (identity_a, load_a) =
        NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join("node-a")))?;
    let (identity_b, load_b) =
        NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join("node-b")))?;
    let (identity_c, load_c) =
        NodeIdentity::load_or_create(&IdentityPaths::rooted_at(root.join("node-c")))?;
    let (id_a, id_b, id_c) = (
        identity_a.node_id(),
        identity_b.node_id(),
        identity_c.node_id(),
    );
    println!("  identity load state: A {load_a:?}, B {load_b:?}, C {load_c:?}");
    let (identity_a, identity_b, identity_c) = (
        Arc::new(identity_a),
        Arc::new(identity_b),
        Arc::new(identity_c),
    );

    // Every node dials the other two, so the simultaneous-dial dedup is exercised.
    let node_a = spawn_node(
        "A",
        Arc::clone(&identity_a),
        address_a,
        vec![(id_b, address_b), (id_c, address_c)],
    )
    .await;
    let node_b = spawn_node(
        "B",
        Arc::clone(&identity_b),
        address_b,
        vec![(id_a, address_a), (id_c, address_c)],
    )
    .await;
    let node_c = spawn_node(
        "C",
        Arc::clone(&identity_c),
        address_c,
        vec![(id_a, address_a), (id_b, address_b)],
    )
    .await;
    let nodes = [node_a, node_b, node_c];

    println!("\n== mesh formation (2 members each expected) ==");
    for node in &nodes {
        let ok = wait_for_members(node, 2, Duration::from_secs(10)).await;
        let members = node.manager.members().await;
        println!("  {}: {} members {:?}", node.name, members.len(), members);
        assert!(ok, "{} did not reach 2 members", node.name);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for node in &nodes {
        let members = node.manager.members().await;
        assert_eq!(
            members.len(),
            2,
            "{} has duplicate links: {members:?}",
            node.name
        );
    }
    println!("  no duplicates: each node holds exactly one link per peer");

    println!("\n== Envelope round trip (A -> B) ==");
    let handle = nodes[0].manager.handle(&id_b).await.expect("a handle to B");
    handle
        .send(Message::OwnerClaim {
            serial: 7,
            reason: ClaimReason::User,
        })
        .await?;
    // Motion is droppable and counted, key/button traffic is not: show the difference.
    let motion_queued = handle.send_motion(Vec::new());
    println!("  A -> B: OwnerClaim serial 7 sent (empty motion accepted: {motion_queued})");
    tokio::time::sleep(Duration::from_millis(300)).await;

    println!("\n== kill C (no Shutdown): offline within ~3 s ==");
    // A real kill drops both the sockets and the listener/dialers: `kill_all` closes the
    // links without a `Shutdown`, and aborting the manager's loop frees the port so the
    // restart below can bind the same address.
    let killed = Instant::now();
    nodes[2].manager.kill_all().await;
    nodes[2].log.abort();
    nodes[2].runner.abort();
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut both_noticed = false;
    while Instant::now() < deadline {
        let a = nodes[0].manager.member_count().await;
        let b = nodes[1].manager.member_count().await;
        if a == 1 && b == 1 {
            both_noticed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elapsed = killed.elapsed();
    println!(
        "  A and B dropped the link to C after {:.1} s ({})",
        elapsed.as_secs_f64(),
        if both_noticed {
            "both noticed"
        } else {
            "TIMEOUT"
        }
    );
    assert!(both_noticed, "peers did not notice a dead node");
    assert!(
        elapsed < Duration::from_secs(6),
        "offline detection took longer than the documented 3 s plus slack: {elapsed:?}"
    );

    println!("\n== restart C with the same identity: links re-form ==");
    let node_c2 = spawn_node(
        "C",
        Arc::clone(&identity_c),
        address_c,
        vec![(id_a, address_a), (id_b, address_b)],
    )
    .await;
    assert_eq!(node_c2.id, id_c, "C must keep its node_id");
    let re_formed = wait_for_members(&node_c2, 2, Duration::from_secs(10)).await;
    println!("  C is back with 2 members: {}", re_formed);
    assert!(re_formed, "restarted C did not re-form its links");
    let a_back = wait_for_members(&nodes[0], 2, Duration::from_secs(10)).await;
    println!("  A sees C again: {a_back}");
    assert!(a_back);

    println!("\n== sanity: the failure vocabulary is the documented one ==");
    assert_eq!(ErrorCode::MembershipFull.as_u32(), 23);
    println!("  ERROR_CODE_MEMBERSHIP_FULL = 23 (the code a fifth member is refused with)");
    nodes[1].manager.kill_all().await;
    nodes[0].manager.kill_all().await;
    let _ = node_c2.manager.kill_all().await;
    for node in [&nodes[0], &nodes[1], &node_c2] {
        node.runner.abort();
        node.log.abort();
    }
    nodes[2].log.abort();
    std::fs::remove_dir_all(&root).ok();
    println!("\nLINK DEMO OK");
    Ok(())
}
