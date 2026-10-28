//! Revocation uses real isolated cores and real pairing, never desktop input.

mod common;

use common::{Cluster, Harness, Node};
use std::time::Duration;

/// The CLI revoke of a linked peer drops the link and keeps it down.
#[test]
fn a_cli_revoke_drops_the_live_link_and_refuses_the_redial() {
    let _serial = common::serial();
    let harness = input_free_pair();
    harness.wait_linked();

    // `status` names this node's own id, which is what the CLI resolves the reference to.
    let b_id = harness.b.status().expect("b answers").node;
    assert_eq!(b_id.len(), 64, "a node id is 32 bytes of hex");

    // The real CLI, against the running core (`Node::ctl` runs the binary).
    let reply = harness
        .a
        .ctl(&format!("peers --revoke {b_id}"))
        .expect("the revoke must reach the running core");
    assert!(reply.starts_with("OK revoked"), "{reply}");

    harness
        .a
        .wait_for(
            "the revoked peer is gone",
            Duration::from_secs(10),
            |status| status.members == 0,
        )
        .expect("the live link to a revoked peer must be dropped");
    harness
        .b
        .wait_for(
            "the remote side forgets the requester",
            Duration::from_secs(10),
            |_| !trusts(&harness.b, &harness.a.id),
        )
        .expect("remote trust must disappear, not merely go offline");
    let remote = gui_status(&harness.b);
    assert!(!remote
        .members
        .iter()
        .any(|peer| peer.node_id == harness.a.id));
    assert_eq!(remote.unpair_notice, "source");
    assert_eq!(remote.unpair_notice_id, 1);
    assert_layout_tombstone(&harness.b, &harness.a.id);
    print_unpair_evidence("online A", &harness.a);
    print_unpair_evidence("online B", &harness.b);

    // The peer's dialer keeps retrying on its backoff, and the pin is gone, so every attempt
    // is refused at TLS: the member count must stay at zero.
    std::thread::sleep(Duration::from_secs(3));
    let after = harness.a.status().expect("a answers");
    assert_eq!(
        after.members, 0,
        "a revoked peer must not come back through its own redial"
    );
}

fn trust_text(node: &Node) -> String {
    std::fs::read_to_string(node.root.join("config/trust.toml")).expect("trust state")
}

fn trusts(node: &Node, peer: &str) -> bool {
    identity::TrustStore::from_toml(&trust_text(node))
        .expect("valid trust state")
        .active()
        .any(|record| identity::fingerprint(&record.node_id) == peer)
}

fn input_free_pair() -> Harness {
    Harness::start_with(false, false, |a, b| {
        for root in [a, b] {
            std::fs::create_dir_all(root.join("run")).expect("isolated runtime");
            std::fs::write(root.join("run/no-input-events-dry"), b"").expect("no input");
        }
    })
    .0
}

fn gui_status(node: &Node) -> protocol::ipc::Status {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("IPC runtime");
    runtime.block_on(async {
        let endpoint = common::gui_endpoint_of(&node.root);
        let stream = ipc::transport::connect(&endpoint)
            .await
            .expect("GUI endpoint");
        let mut link = ipc::GuiLink::new(stream);
        link.send(
            1,
            protocol::ipc::gui_frame::Msg::GetStatus(protocol::ipc::GetStatus {}),
        )
        .await
        .expect("status request");
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), link.recv())
                .await
                .expect("GUI deadline")
                .expect("GUI frame");
            if frame.request_id == 1 {
                if let Some(protocol::ipc::gui_frame::Msg::Status(status)) = frame.msg {
                    return status;
                }
            }
        }
    })
}

fn assert_layout_tombstone(node: &Node, removed: &str) {
    let config =
        std::fs::read_to_string(node.root.join("config/config.toml")).expect("persisted layout");
    let pending =
        std::fs::read_to_string(node.root.join("config/layout.pending.toml")).unwrap_or_default();
    assert!(
        config
            .lines()
            .chain(pending.lines())
            .any(|line| { line.starts_with("forgotten_nodes") && line.contains(removed) }),
        "remote removal must persist the stale-report tombstone:\n{config}\n{pending}"
    );
}

fn print_unpair_evidence(label: &str, node: &Node) {
    for line in node.log().lines().filter(|line| {
        line.contains("unpair layout")
            || line.contains("peer revoked")
            || line.contains("peer removed pairing with this computer")
    }) {
        println!("{label}: {line}");
    }
}

#[test]
fn offline_revoke_is_delivered_after_both_cores_restart_without_input() {
    let _serial = common::serial();
    let mut harness = input_free_pair();
    harness.wait_linked();
    harness.b.stop();
    harness
        .a
        .wait_for("B offline", Duration::from_secs(10), |status| {
            status.members == 0
        })
        .expect("B disconnects");
    let reply = harness
        .a
        .ctl(&format!("peers --revoke {}", harness.b.id))
        .expect("offline revoke");
    assert!(reply.starts_with("OK revoked"), "{reply}");
    let pending = gui_status(&harness.a).pending_unpairs;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].node_id, harness.b.id);
    assert!(trusts(&harness.b, &harness.a.id));
    harness.a.restart_without_input();
    harness
        .a
        .wait_for("A restarted", Duration::from_secs(10), |_| true)
        .expect("A running");
    assert_eq!(
        gui_status(&harness.a).pending_unpairs.len(),
        1,
        "queue survives restart"
    );
    harness.b.restart_without_input();
    harness
        .b
        .wait_for("offline notice applied", Duration::from_secs(20), |_| {
            !trusts(&harness.b, &harness.a.id)
        })
        .expect("B must forget A after notify-only reconnect");
    harness
        .a
        .wait_for("notice consumed", Duration::from_secs(10), |_| {
            gui_status(&harness.a).pending_unpairs.is_empty()
        })
        .expect("successful send removes pending pin");
    assert_eq!(harness.a.status().expect("A status").members, 0);
    assert_eq!(harness.b.status().expect("B status").members, 0);
    assert_layout_tombstone(&harness.b, &harness.a.id);
    print_unpair_evidence("offline A", &harness.a);
    print_unpair_evidence("offline B", &harness.b);
}

#[test]
fn unpair_changes_only_one_pair_in_a_three_node_mesh() {
    let _serial = common::serial();
    let cluster = Cluster::start("three-unpair");
    cluster.wait_mesh(Duration::from_secs(40));
    let a = &cluster.nodes[0];
    let b = &cluster.nodes[1];
    let c = &cluster.nodes[2];
    let reply = a
        .ctl(&format!("peers --revoke {}", b.id))
        .expect("pair revoke");
    assert!(reply.starts_with("OK revoked"), "{reply}");
    b.wait_for("B forgets only A", Duration::from_secs(10), |_| {
        !trusts(b, &a.id)
    })
    .expect("remote pair removed");
    assert!(!trusts(a, &b.id));
    assert!(trusts(a, &c.id));
    assert!(trusts(b, &c.id));
    assert!(trusts(c, &a.id));
    assert!(trusts(c, &b.id));
    c.wait_for("C keeps both links", Duration::from_secs(10), |status| {
        status.members == 2
    })
    .expect("third-party trust and connectivity stay intact");
}

#[test]
fn failed_trust_save_is_retried_and_revocation_survives_restart() {
    let _serial = common::serial();
    let mut harness = input_free_pair();
    harness.wait_linked();
    harness.b.stop();
    harness.a.wait_for("peer offline", Duration::from_secs(10), |status| {
        status.members == 0
    }).expect("peer disconnects");
    let before = trust_text(&harness.a);
    // A directory at the temporary-file path deterministically rejects File::create
    // on both OSes, including privileged test runners. The primary stays readable.
    let blocker = harness.a.root.join("config/trust.toml.tmp");
    std::fs::create_dir(&blocker).expect("block only trust persistence");
    let revoke_once = || {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_bifrost"))
            .arg("--root").arg(&harness.a.root)
            .args(["peers", "--revoke", &harness.b.id])
            .output().expect("single revoke request");
        format!("{}{}", String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr))
    };
    let first = revoke_once();
    assert!(first.contains("TRUST_WRITE"), "{first}");
    assert_eq!(trust_text(&harness.a), before);
    assert!(trusts(&harness.a, &harness.b.id));
    let retry = revoke_once();
    assert!(retry.contains("TRUST_WRITE"), "retry must attempt persistence: {retry}");
    assert_eq!(trust_text(&harness.a), before);
    assert!(trusts(&harness.a, &harness.b.id));
    std::fs::remove_dir(&blocker).expect("restore trust persistence");
    let saved = revoke_once();
    assert!(saved.starts_with("OK revoked"), "{saved}");
    assert!(!trusts(&harness.a, &harness.b.id));
    harness.a.restart_without_input();
    harness.a.wait_for("core restarted", Duration::from_secs(10), |_| true)
        .expect("core answers after restart");
    assert!(!trusts(&harness.a, &harness.b.id), "revocation must survive restart");
    assert!(gui_status(&harness.a).members.iter().all(|peer| peer.node_id != harness.b.id));
}
