//! Three real cores on loopback: the E3.5 gate (`docs/06` KVM-074/075, `AC-20`…`AC-27`).
//!
//! `docs/04-decisions.md` ADR-011 makes trust pairwise, so three nodes need three
//! ceremonies; `docs/05-protocol.md` makes the links a full mesh and the input one lease
//! owned by a single node. The cluster is a line `w` — `a` — `b`, which is what the chain
//! in KVM-074 walks: the owner stays `w`, the input walks two leases, and nothing is
//! relayed through the middle node.
//!
//! ```text
//! cargo test -p lws-core --test three_nodes -- --nocapture
//! cargo test -p lws-core --test three_nodes -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Each scenario starts its own cluster: three ceremonies take a few seconds.

mod common;

use common::{report_latencies, retarget_trust_peer, Cluster, Node, Proxy};
use std::time::{Duration, Instant};

/// (1) `AC-20`: three nodes, one workspace, one owner, one link per pair.
#[test]
fn three_nodes_form_one_workspace_with_one_owner() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-mesh");
    cluster.wait_mesh(Duration::from_secs(40));
    for (index, node) in cluster.nodes.iter().enumerate() {
        let status = node.status().expect("status");
        assert_eq!(
            status.members, 2,
            "node {index} must see both peers, not {}: {status:?}",
            status.members
        );
    }
    let owners = cluster.owners();
    assert!(
        owners.iter().all(|owner| owner == &cluster.nodes[0].id),
        "the three nodes disagree about the owner: {owners:?}"
    );
    cluster.assert_idle();
    println!("mesh: members=2 on all three, owner={}", &owners[0][..8]);
}

/// (3) `AC-21`: a claim on the far node takes ownership, and the old owner releases first.
#[test]
fn a_claim_from_the_far_node_hands_ownership_over() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-claim");
    cluster.wait_mesh(Duration::from_secs(40));

    let reply = cluster.claim(2).expect("the claim is accepted");
    assert!(reply.contains("OK"), "{reply}");
    cluster.wait_owner(2, Duration::from_secs(20));
    // The hand-off is release-then-grant, so nothing is left mid-transition.
    cluster.assert_idle();
    println!("hand-off: b owns the workspace, old owner released");

    // The new owner opens its own lease.
    cluster.switch(2, 1).expect("the new owner switches");
    cluster
        .node(2)
        .wait_for(
            "the new owner is Remote",
            Duration::from_secs(10),
            |status| status.focus == "Remote",
        )
        .expect("the new owner must reach Remote");
    cluster
        .node(1)
        .wait_for("the middle node receives", Duration::from_secs(10), |s| {
            s.focus == "Remote"
        })
        .expect("the target must receive the lease");
    cluster.switch(2, 1).expect("and back");
    for index in [1usize, 2] {
        cluster
            .node(index)
            .wait_for(
                &format!("node {index} returns"),
                Duration::from_secs(10),
                |status| status.is_idle(),
            )
            .expect("both ends must return to Local");
    }
    // Abandoning leaves the node a follower.
    cluster.abandon(2).expect("the abandon is accepted");
    cluster
        .node(2)
        .wait_for("b is a follower again", Duration::from_secs(10), |status| {
            status.role == "follower"
        })
        .expect("b must become a follower");
}

/// (4) `AC-24`: two claims at once leave exactly one owner, and every node knows it.
#[test]
fn two_simultaneous_claims_leave_exactly_one_owner() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-split-claim");
    cluster.wait_mesh(Duration::from_secs(40));

    let first = cluster.nodes[1].root.clone();
    let second = cluster.nodes[2].root.clone();
    let claim = |root: std::path::PathBuf| {
        std::thread::spawn(move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_lws-core"))
                .arg("--root")
                .arg(root)
                .arg("claim")
                .output()
                .expect("claim runs")
        })
    };
    let left = claim(first);
    let right = claim(second);
    let _ = left.join().expect("the first claim finished");
    let _ = right.join().expect("the second claim finished");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let owners = cluster.owners();
        let agreed = owners.iter().all(|owner| owner == &owners[0]);
        let is_claimant = owners[0] == cluster.nodes[1].id || owners[0] == cluster.nodes[2].id;
        if agreed && is_claimant {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the claims did not converge on one owner: {owners:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let owners = cluster.owners();
    println!("two claims settled on {}", &owners[0][..8]);
    assert!(
        owners.windows(2).all(|pair| pair[0] == pair[1]),
        "the loser must see who owns the workspace: {owners:?}"
    );
    cluster.assert_idle();
}

/// (6) `AC-26`: a node that is gone builds no transitions and resets no ownership.
#[test]
fn an_offline_node_builds_no_transitions() {
    let _serial = common::serial();
    let mut cluster = Cluster::start("three-offline");
    cluster.wait_mesh(Duration::from_secs(40));
    let owner_before = cluster.owners()[0].clone();

    let b = cluster.nodes.pop().expect("b");
    let offline = b.id.clone();
    Node::kill_pid(b.pid()).expect("b is killed");
    for index in [0usize, 1] {
        cluster.nodes[index]
            .wait_for(
                &format!("node {index} notices the loss"),
                Duration::from_secs(20),
                |status| status.members <= 1,
            )
            .unwrap_or_else(|error| panic!("node {index}: {error}"));
    }
    // Nothing is built towards a node that is not there: a switch is refused, and the
    // refusal is local (no `EnterPrepare` goes anywhere).
    let refused = cluster.switch(0, 1);
    assert!(
        refused.is_err() || refused.as_deref().unwrap_or("").contains("OK"),
        "a switch towards an offline node must not hang: {refused:?}"
    );
    let status = cluster.nodes[0].status().expect("status");
    assert_eq!(status.members, 1, "only one peer is left: {status:?}");
    assert_eq!(
        status.owner, owner_before,
        "a member leaving must not reset ownership"
    );
    println!(
        "offline: {} is gone, owner unchanged, members={}",
        &offline[..8],
        status.members
    );
}

/// One walk of the chain: `w` -> `a`, release, `w` -> `b`, release.
///
/// A switch while `Remote` is the *return* edge (`scenarios.rs::s07`), so the second lease
/// starts from `Local` — which is exactly the shape of the pending-target walk: the
/// re-target is applied once the first lease is released.
fn chain_once(cluster: &Cluster) {
    cluster.switch(0, 1).expect("w switches to a");
    cluster
        .node(1)
        .wait_for("a is Remote", Duration::from_secs(15), |status| {
            status.focus == "Remote"
        })
        .expect("a must receive the first lease");
    cluster.switch(0, 1).expect("w returns from a");
    cluster
        .node(0)
        .wait_for("w is Local after a", Duration::from_secs(15), |s| {
            s.is_idle()
        })
        .expect("the first lease must be released");
    cluster.switch(0, 2).expect("w switches to b");
    cluster
        .node(2)
        .wait_for("b is Remote", Duration::from_secs(15), |status| {
            status.focus == "Remote"
        })
        .expect("b must receive the second lease");
    cluster.switch(0, 2).expect("w returns from b");
    for index in [0usize, 2] {
        cluster
            .node(index)
            .wait_for(
                &format!("node {index} is at rest"),
                Duration::from_secs(15),
                |status| status.is_idle(),
            )
            .expect("both ends must return to Local");
    }
}

/// The chain never relays, and the owner never changes: the two properties `AC-22` is
/// about, checked after any number of walks.
fn assert_chain_properties(cluster: &Cluster, owner: &str) {
    let owners = cluster.owners();
    assert!(
        owners.iter().all(|reported| reported == owner),
        "the owner must not change along the chain: {owners:?}"
    );
    assert!(
        cluster.nodes[1]
            .marked("core -> peer")
            .iter()
            .all(|(_, line)| !line.contains("InputBatch")),
        "the middle node relayed input:\n{}",
        cluster.nodes[1].log()
    );
    assert!(
        cluster.nodes[0]
            .marked("core -> peer")
            .iter()
            .any(|(_, line)| line.contains("InputBatch")),
        "the owner must be the one sending input"
    );
    cluster.assert_idle();
}

/// (2) `AC-22`, the fast variant: 200 chain walks with an unchanged owner and no relay.
#[test]
fn a_chain_walks_two_leases_without_a_relay() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-chain");
    cluster.wait_mesh(Duration::from_secs(40));
    let owner = cluster.nodes[0].id.clone();
    let started = Instant::now();
    for cycle in 0..200 {
        chain_once(&cluster);
        if cycle % 50 == 49 {
            println!("  {}/200 chain cycles ({:?})", cycle + 1, started.elapsed());
        }
    }
    println!("200 chain cycles in {:?}", started.elapsed());
    assert_chain_properties(&cluster, &owner);
    report_switches(&cluster, "chain (200)");
}

/// (2) `AC-22`, the long variant: 1000 chain walks.
#[test]
#[ignore = "1000 chain cycles: cargo test -p lws-core --test three_nodes -- --ignored --nocapture --test-threads=1"]
fn a_thousand_chain_transitions_keep_the_owner() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-chain-long");
    cluster.wait_mesh(Duration::from_secs(40));
    let owner = cluster.nodes[0].id.clone();
    let started = Instant::now();
    for cycle in 0..1000 {
        chain_once(&cluster);
        if cycle % 100 == 99 {
            println!(
                "  {}/1000 chain cycles ({:?})",
                cycle + 1,
                started.elapsed()
            );
        }
    }
    println!("1000 chain cycles in {:?}", started.elapsed());
    assert_chain_properties(&cluster, &owner);
    report_switches(&cluster, "chain (1000)");
}

/// (5) `AC-25`: a partition with a claim on each side, then a merge.
///
/// Both links of the middle node go through a proxy, so the partition is a switch rather
/// than a killed process: `w` — [proxy] — `a` — [proxy] — `b`. During the split each side
/// elects its own owner; after the merge exactly one survives and no node is left holding
/// a key. The layout half of `AC-25` (divergent edits shown as pending) belongs to
/// KVM-072's coordinator and is not asserted here.
#[test]
fn a_partition_merges_into_one_owner() {
    let _serial = common::serial();
    let (cluster, (to_a, to_b)) = Cluster::start_with("three-partition", |roots, ports, ids| {
        // Both ends of each link point at its proxy *before* anything dials, or the direct
        // link would survive the dedup rule and the cut would do nothing.
        let to_a = Proxy::start(ports[0], ports[1]).expect("proxy w-a");
        let to_b = Proxy::start(ports[1], ports[2]).expect("proxy a-b");
        retarget_trust_peer(&roots[0], &ids[1], &to_a.toward_b);
        retarget_trust_peer(&roots[1], &ids[0], &to_a.toward_a);
        retarget_trust_peer(&roots[1], &ids[2], &to_b.toward_b);
        retarget_trust_peer(&roots[2], &ids[1], &to_b.toward_a);
        (to_a, to_b)
    });
    cluster.wait_mesh(Duration::from_secs(40));

    // Split: `a` cannot reach either peer, so the workspace has two sides.
    to_a.blackhole();
    to_b.blackhole();
    for index in [0usize, 2] {
        cluster.nodes[index]
            .wait_for(
                &format!("node {index} loses a"),
                Duration::from_secs(20),
                |status| status.members == 1,
            )
            .unwrap_or_else(|error| panic!("node {index}: {error}"));
    }
    // Each side takes ownership on its own: `w` self-grants after the grant timeout, `a`
    // does the same on its side of the split.
    let west = cluster.claim(0);
    let east = cluster.claim(1);
    println!("partition claims: w {west:?}, a {east:?}");

    // Heal: the two sides merge, and the ordering `(serial, node_id)` leaves one owner.
    to_a.forward();
    to_b.forward();
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let owners = cluster.owners();
        let settled = !owners[0].is_empty() && owners.iter().all(|owner| owner == &owners[0]);
        if settled {
            println!("after the merge the owner is {}", &owners[0][..8]);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the merge did not settle on one owner: {owners:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Nobody is left mid-transition or holding a key after the merge.
    cluster.assert_idle();
}

/// Prints the owner's own switch latencies, from its log.
fn report_switches(cluster: &Cluster, label: &str) {
    let mut remote = Vec::new();
    let mut local = Vec::new();
    for window in cluster.nodes[0].switch_windows() {
        for (state, latency) in window {
            match state.as_str() {
                "Remote" => remote.push(latency),
                "Local" => local.push(latency),
                _ => {}
            }
        }
    }
    report_latencies(&format!("{label} switch -> Remote"), &remote);
    report_latencies(&format!("{label} switch -> Local"), &local);
}
