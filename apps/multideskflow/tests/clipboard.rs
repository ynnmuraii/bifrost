//! The clipboard end to end, on the real processes the harness starts.
//!
//! ADR-008 (`docs/04-decisions.md`), `docs/05-protocol.md` («Clipboard канал») and the
//! acceptance criteria this file pins:
//!
//! * **AC-28** — a copy on one node reaches every other member, once each;
//! * **AC-09** — the payload limit is exactly 1 MiB, and one byte more is refused without
//!   anything being applied;
//! * **AC-15** — content a password manager marked is not sent;
//! * **KVM-042** — «объём clipboard не блокирует key-up»: the clipboard has a channel of its
//!   own, and an input transition during a transfer is not delayed by it.
//!
//! # How the runner drives the OS layer
//!
//! A CI machine has no clipboard worth the name, so the agent runs `--dry-inject` and core
//! hands it a directory of files (`--dry-clipboard-dir`). Writing `in.txt` **is** a local user
//! copy, and `out.txt` is what that node's clipboard holds afterwards. The harness never fakes
//! the agent or core: the copy travels agent → IPC → core → TLS → core → IPC → agent, and the
//! only thing missing is the OS selection at either end.
//!
//! Every test takes `common::serial()`: the nodes bind fixed loopback ports and start real
//! processes, so the suite runs one scenario at a time (the same rule the other suites follow).
//!
//! Each node may run its clipboard listener on a port of its own: `ClipboardGrant` advertises
//! the sender's `clipboard_port` and the dialer takes the address from the live input link, so
//! the harness's per-node random ports are the honest case rather than an obstacle.

mod common;

use std::time::Duration;

use common::{enable_clipboard, write_clipboard_rate, Cluster, Harness};

/// How long a copy may take to settle. The default rate is 256 KiB/s, so a 1 MiB payload
/// legitimately needs about four seconds on top of the round trip.
const SETTLE: Duration = Duration::from_secs(60);

/// Two nodes, both with the clipboard enabled and linked.
fn two_nodes_with_clipboard() -> Harness {
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        enable_clipboard(a_root);
        enable_clipboard(b_root);
    });
    harness.wait_linked();
    harness
}

/// Drive the real GUI command and require its unsolicited Status push.
fn toggle_clipboard(node: &common::Node, enabled: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("GUI runtime");
    runtime.block_on(async {
        let connection = ipc::transport::connect(&common::gui_endpoint_of(&node.root))
            .await
            .expect("GUI connection");
        let mut link = ipc::GuiLink::new(connection);
        link.send(
            1,
            protocol::ipc::gui_frame::Msg::SetClipboard(protocol::ipc::SetClipboard { enabled }),
        )
        .await
        .expect("send explicit opt-in");
        tokio::time::timeout(Duration::from_secs(15), async {
            let mut answered = false;
            let mut pushed = false;
            while !answered || !pushed {
                let frame = link.recv().await.expect("GUI frame");
                match frame.msg {
                    Some(protocol::ipc::gui_frame::Msg::OpResult(result))
                        if frame.request_id == 1 =>
                    {
                        assert!(result.ok, "toggle refused: {}", result.error_code);
                        answered = true;
                    }
                    Some(protocol::ipc::gui_frame::Msg::Status(status))
                        if frame.request_id == 0 && status.clipboard_enabled == enabled =>
                    {
                        pushed = true;
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("toggle answer and pushed Status");
    });
    let config = config::Config::load(&node.root.join("config/config.toml")).expect("saved config");
    assert_eq!(config.clipboard.enabled, enabled, "opt-in must survive restart");
}

fn clipboard_address(node: &common::Node) -> std::net::SocketAddr {
    let config = config::Config::load(&node.root.join("config/config.toml")).expect("config");
    std::net::SocketAddr::from(([127, 0, 0, 1], config.network.port_clipboard))
}

#[test]
fn gui_opt_in_starts_default_off_clipboards_and_off_closes_the_listener() {
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let address = clipboard_address(&harness.b);
    assert!(std::net::TcpStream::connect(address).is_err());
    toggle_clipboard(&harness.a, true);
    toggle_clipboard(&harness.b, true);
    let text = payload("gui-opt-in", 64);
    harness.a.copy_to_clipboard(&text);
    harness.b.wait_for_clipboard(&text, SETTLE).expect("GUI opt-in copy reaches B");
    toggle_clipboard(&harness.b, false);
    assert!(std::net::TcpStream::connect(address).is_err(), "disabled listener is closed");
    let listener = std::net::TcpListener::bind(address).expect("off releases the listening port");
    drop(listener);
    harness.a.copy_to_clipboard(&payload("while-off", 80));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(harness.b.clipboard_text().as_deref(), Some(text.as_str()));
}

#[test]
fn ten_gui_on_off_cycles_release_sockets_and_keep_peer_delivery_working() {
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    toggle_clipboard(&harness.a, true);
    let address = clipboard_address(&harness.b);
    for cycle in 0..10 {
        toggle_clipboard(&harness.b, true);
        let text = payload(&format!("cycle-{cycle}"), 64);
        harness.a.copy_to_clipboard(&text);
        harness.b.wait_for_clipboard(&text, SETTLE).expect("fresh runtime receives copy");
        toggle_clipboard(&harness.b, false);
        assert!(std::net::TcpStream::connect(address).is_err());
        let listener = std::net::TcpListener::bind(address).expect("no leaked listener");
        drop(listener);
    }
}

#[test]
fn toggling_the_sender_preserves_origin_versions_for_a_live_receiver() {
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    let first = payload("before-sender-toggle", 64);
    harness.a.copy_to_clipboard(&first);
    harness.b.wait_for_clipboard(&first, SETTLE).expect("initial copy");
    toggle_clipboard(&harness.a, false);
    toggle_clipboard(&harness.a, true);
    let second = payload("after-sender-toggle", 64);
    harness.a.copy_to_clipboard(&second);
    harness.b.wait_for_clipboard(&second, Duration::from_secs(10))
        .expect("new local copy must not reuse a version suppressed by the live receiver");
}

/// Distinct payloads, so a later assertion cannot pass on an earlier copy.
fn payload(tag: &str, len: usize) -> String {
    let mut text = format!("lws-{tag}-{}-", std::process::id());
    while text.len() < len {
        text.push_str(tag);
    }
    text.truncate(len);
    text
}

#[test]
fn a_copy_on_one_node_arrives_on_the_other() {
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    let text = payload("two-nodes", 64);

    harness.a.copy_to_clipboard(&text);
    harness
        .a
        .wait_for_clipboard(&text, SETTLE)
        .expect("the node that copied holds its own text");
    harness
        .b
        .wait_for_clipboard(&text, SETTLE)
        .expect("B applies what A copied");
    assert!(
        harness.b.log().contains("clipboard payload applied"),
        "B must have verified and applied a payload, not merely stored it"
    );
}

#[test]
fn a_copy_reaches_every_other_member_once() {
    // AC-28's shape: A copies, B and C each end up with it, and neither of them re-offers it.
    let _serial = common::serial();
    let (cluster, ()) = Cluster::start_with("clipboard-fanout", |roots, _, _| {
        for root in roots {
            enable_clipboard(root);
        }
    });
    cluster.wait_mesh(Duration::from_secs(40));

    let text = payload("three-nodes", 96);
    cluster.node(0).copy_to_clipboard(&text);
    for index in [1usize, 2] {
        cluster
            .node(index)
            .wait_for_clipboard(&text, SETTLE)
            .unwrap_or_else(|error| panic!("node {index} never got the copy: {error}"));
    }
    for index in [1usize, 2] {
        assert_eq!(
            cluster.node(index).log().matches("clipboard offer").count(),
            0,
            "node {index} applied a peer's payload: it must not mint an offer of its own"
        );
    }
    assert_eq!(
        cluster.node(0).log().matches("clipboard offer").count(),
        1,
        "the node that copied offers exactly once, whatever the member count"
    );
}

#[test]
fn one_mebibyte_travels_and_one_byte_more_is_refused() {
    // AC-09: the limit is exactly 1 MiB. The larger copy must produce `CLIPBOARD_TOO_LARGE`
    // *and* change nothing on the other side.
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();

    let allowed = payload("mib", 1024 * 1024);
    harness.a.copy_to_clipboard(&allowed);
    harness
        .b
        .wait_for_clipboard(&allowed, SETTLE)
        .expect("exactly 1 MiB must travel");

    let refused = payload("over", 1024 * 1024 + 1);
    harness.a.copy_to_clipboard(&refused);
    // Give the refused copy every chance to arrive (and to be wrongly applied).
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        harness.b.clipboard_text().as_deref(),
        Some(allowed.as_str()),
        "a payload over the limit must not be applied"
    );
    assert!(
        harness.a.log().contains("CLIPBOARD_TOO_LARGE"),
        "the refusal has to be reported with the documented code"
    );
}

#[test]
fn a_node_that_did_not_enable_the_clipboard_never_applies_a_copy() {
    // Capability advertises adapter support; opting out still forbids any payload application.
    let _serial = common::serial();
    let (harness, ()) = Harness::start_with(false, false, |a_root, _b_root| {
        enable_clipboard(a_root);
    });
    harness.wait_linked();

    let text = payload("disabled", 48);
    harness.a.copy_to_clipboard(&text);
    harness
        .a
        .wait_for_clipboard(&text, SETTLE)
        .expect("A still holds its own copy");
    std::thread::sleep(Duration::from_secs(2));

    assert_eq!(
        harness.b.clipboard_text().unwrap_or_default(),
        "",
        "a node with the clipboard off must never receive a payload (an empty dry clipboard is \
         what a node that was never sent anything holds)"
    );
}

#[test]
fn a_marked_secret_is_not_sent() {
    // AC-15: a copy a password manager marked as a secret never leaves the machine.
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();

    let secret = payload("secret", 40);
    harness.a.copy_to_clipboard_marked(&secret, &["kde_secret"]);
    std::thread::sleep(Duration::from_secs(3));

    assert!(
        harness.a.log().contains("CLIPBOARD_SECRET"),
        "the skip has to be reported with the documented code"
    );
    assert_eq!(
        harness.b.clipboard_text().unwrap_or_default(),
        "",
        "a secret-marked copy must not reach the peer"
    );

    // The very same text without the marker does travel, which is what makes the assertion
    // above about the marker and not about the payload.
    harness.a.copy_to_clipboard(&secret);
    harness
        .b
        .wait_for_clipboard(&secret, SETTLE)
        .expect("the unmarked copy travels");
    assert!(
        harness.b.log().contains("clipboard payload applied"),
        "B applies the unmarked copy"
    );
}

#[test]
fn a_received_copy_is_never_offered_back() {
    // The echo rule: B applies A's payload and mints no version of its own, so the fan-out
    // settles instead of bouncing. Counted rather than timed: an offer is logged once per
    // local copy, so a loop would show up as extra offers on both nodes.
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();

    let text = payload("echo", 80);
    harness.a.copy_to_clipboard(&text);
    harness
        .b
        .wait_for_clipboard(&text, SETTLE)
        .expect("B applies");
    let offers_on_a = harness.a.log().matches("clipboard offer").count();
    let offers_on_b = harness.b.log().matches("clipboard offer").count();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        harness.a.log().matches("clipboard offer").count(),
        offers_on_a,
        "the version counter must stop moving once the copy has been applied"
    );
    assert_eq!(
        harness.b.log().matches("clipboard offer").count(),
        offers_on_b,
        "B must not offer what it received"
    );
    assert_eq!(offers_on_b, 0, "B copied nothing locally");
    assert_eq!(offers_on_a, 1, "A copied once");
}

#[test]
fn a_transition_during_a_mebibyte_transfer_is_not_delayed() {
    // KVM-042. The clipboard is paced at 64 KiB/s, so a 256 KiB payload occupies its channel
    // for about four seconds — far longer than a transition may ever take. The transition runs
    // on the input link, which shares no socket, queue or task with the clipboard.
    let _serial = common::serial();
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        write_clipboard_rate(a_root, 64 * 1024);
        write_clipboard_rate(b_root, 64 * 1024);
    });
    harness.wait_linked();

    let text = payload("kvm042", 256 * 1024);
    harness.a.copy_to_clipboard(&text);
    // Wait until the channel is open and the paced transfer has started: the grant is logged
    // before the peer dials, so this is the last cheap signal that it is *about* to move.
    let deadline = std::time::Instant::now() + SETTLE;
    while !harness.a.log().contains("clipboard grant") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        harness.a.log().contains("clipboard grant"),
        "the grant never left A, so this test measured nothing"
    );
    // The bind, the offer and the request are one round trip; the paced data then takes about
    // four seconds (64 KiB/s), which is the window this measures in. Confirm the transfer has
    // not finished before the transition starts.
    std::thread::sleep(Duration::from_millis(300));
    assert_ne!(
        harness.b.clipboard_text().unwrap_or_default(),
        text,
        "the transfer finished before the transition started, so this test measured nothing"
    );

    let started = std::time::Instant::now();
    harness.cycle();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "a transition took {elapsed:?} while a 4 s clipboard transfer was running: the clipboard \
         is delaying the input link (KVM-042)"
    );

    harness
        .b
        .wait_for_clipboard(&text, SETTLE)
        .expect("the paced transfer still completes after the transition");
}
