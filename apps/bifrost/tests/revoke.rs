//! Audit F16: `lws-core revoke <peer>` against a **running** core.
//!
//! The CLI used to tombstone the trust record in `trust.toml` and return. A running core
//! keeps its own copy of that store, the pin set its TLS verifiers read for every handshake
//! and the peer's live links, so the peer stayed trusted in memory, its link stayed up (and
//! could still inject input) and only a restart made the revocation real. The GUI's revoke
//! button was already complete (tombstone → save → `remove_peer` → `kill_all`); the CLI now
//! takes the same path through the control channel, with the file edit left as the
//! no-core-running fallback.

mod common;

use common::Harness;
use std::time::Duration;

/// The CLI revoke of a linked peer drops the link and keeps it down.
#[test]
fn a_cli_revoke_drops_the_live_link_and_refuses_the_redial() {
    let harness = Harness::start(false, false);
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

    // The peer's dialer keeps retrying on its backoff, and the pin is gone, so every attempt
    // is refused at TLS: the member count must stay at zero.
    std::thread::sleep(Duration::from_secs(3));
    let after = harness.a.status().expect("a answers");
    assert_eq!(
        after.members, 0,
        "a revoked peer must not come back through its own redial"
    );
}
