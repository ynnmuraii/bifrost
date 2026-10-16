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

#[test]
fn a_clipboard_capability_reported_after_link_up_reaches_the_peer() {
    let _serial = common::serial();
    let (harness, gate) = Harness::start_with(false, false, |a_root, b_root| {
        enable_clipboard(a_root);
        enable_clipboard(b_root);
        let gate = b_root.join("run/agent-ready-gate");
        std::fs::create_dir_all(gate.parent().expect("gate directory")).expect("run directory");
        std::fs::write(&gate, b"hold AgentReady").expect("hold B's actual agent report");
        gate
    });
    for node in [&harness.a, &harness.b] {
        node.wait_for(
            "input link before B's AgentReady",
            Duration::from_secs(20),
            |s| s.members == 1,
        )
        .expect("the real input link forms while B's agent is starting");
    }
    assert_ne!(harness.b.status().expect("B status").agent, "ready");
    std::fs::remove_file(gate).expect("release the actual AgentReady");
    harness.wait_linked();
    let text = payload("late-agent-capability", 64);
    harness.a.copy_to_clipboard(&text);
    harness
        .b
        .wait_for_clipboard(&text, Duration::from_secs(10))
        .expect("late TEXT_CLIPBOARD enables delivery on the linked pair");
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
    let config = config::Config::load(node.root.join("config/config.toml")).expect("saved config");
    assert_eq!(
        config.clipboard.enabled, enabled,
        "opt-in must survive restart"
    );
}

fn clipboard_address(node: &common::Node) -> std::net::SocketAddr {
    let config = config::Config::load(node.root.join("config/config.toml")).expect("config");
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
    toggle_clipboard(&harness.a, true);
    toggle_clipboard(&harness.b, true);
    let text = payload("gui-opt-in", 64);
    harness.a.copy_to_clipboard(&text);
    harness
        .b
        .wait_for_clipboard(&text, SETTLE)
        .expect("GUI opt-in copy reaches B");
    toggle_clipboard(&harness.b, false);
    toggle_clipboard(&harness.b, false);
    assert!(
        std::net::TcpStream::connect(address).is_err(),
        "disabled listener is closed"
    );
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
        harness
            .b
            .wait_for_clipboard(&text, SETTLE)
            .expect("fresh runtime receives copy");
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
    harness
        .b
        .wait_for_clipboard(&first, SETTLE)
        .expect("initial copy");
    toggle_clipboard(&harness.a, false);
    toggle_clipboard(&harness.a, true);
    let second = payload("after-sender-toggle", 64);
    harness.a.copy_to_clipboard(&second);
    harness
        .b
        .wait_for_clipboard(&second, Duration::from_secs(10))
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
fn repeated_copies_reuse_the_channel_in_both_directions() {
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    for (sender, receiver, direction) in [
        (&harness.a, &harness.b, "a-to-b"),
        (&harness.b, &harness.a, "b-to-a"),
    ] {
        for copy in 0..5 {
            let text = payload(&format!("{direction}-{copy}"), 64);
            sender.copy_to_clipboard(&text);
            receiver
                .wait_for_clipboard(&text, Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("{direction} copy {copy}: {error}"));
        }
    }
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

/// Use the same authenticated GUI IPC endpoint as the desktop, ignoring status pushes.
fn history_exchange(
    node: &common::Node,
    request: protocol::ipc::gui_frame::Msg,
    mut complete: impl FnMut(&protocol::ipc::GuiFrame) -> bool,
) -> Vec<protocol::ipc::GuiFrame> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("history GUI runtime");
    runtime.block_on(async {
        let connection = ipc::transport::connect(&common::gui_endpoint_of(&node.root))
            .await
            .expect("history GUI connection");
        let mut link = ipc::GuiLink::new(connection);
        link.send(1, request).await.expect("history request");
        tokio::time::timeout(SETTLE, async {
            let mut replies = Vec::new();
            loop {
                let frame = link.recv().await.expect("history response");
                if frame.request_id != 1 {
                    continue;
                }
                let done = complete(&frame);
                replies.push(frame);
                if done {
                    return replies;
                }
            }
        })
        .await
        .expect("history response deadline")
    })
}

fn history(node: &common::Node) -> Vec<protocol::ipc::ClipboardHistoryEntry> {
    use protocol::ipc::gui_frame::Msg;
    let replies = history_exchange(
        node,
        Msg::GetClipboardHistory(protocol::ipc::GetClipboardHistory {}),
        |frame| matches!(frame.msg, Some(Msg::ClipboardHistory(_))),
    );
    match replies.into_iter().last().expect("history reply").msg {
        Some(Msg::ClipboardHistory(history)) => history.items,
        other => panic!("unexpected history reply: {other:?}"),
    }
}

fn wait_history(node: &common::Node, count: usize) -> Vec<protocol::ipc::ClipboardHistoryEntry> {
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        let items = history(node);
        if items.len() == count {
            return items;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected {count} history entries, got {items:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn history_text(node: &common::Node, id: u64) -> String {
    use protocol::ipc::gui_frame::Msg;
    let replies = history_exchange(
        node,
        Msg::GetClipboardHistoryItem(protocol::ipc::GetClipboardHistoryItem {
            id,
            copy_to_local: false,
        }),
        |frame| match &frame.msg {
            Some(Msg::ClipboardHistoryItem(item)) => item.chunk_index + 1 == item.chunk_count,
            _ => false,
        },
    );
    let count = replies.len();
    let mut text = String::new();
    for (index, frame) in replies.into_iter().enumerate() {
        let Some(Msg::ClipboardHistoryItem(item)) = frame.msg else {
            panic!("expected full-text chunk");
        };
        assert_eq!(item.id, id);
        assert_eq!(item.chunk_index as usize, index);
        assert_eq!(item.chunk_count as usize, count);
        text.push_str(&item.text);
    }
    if text.len() == 1024 * 1024 {
        assert!(count > 1, "a full MiB must use bounded IPC chunks");
    }
    text
}

fn history_operation(node: &common::Node, request: protocol::ipc::gui_frame::Msg) {
    use protocol::ipc::gui_frame::Msg;
    let replies = history_exchange(node, request, |frame| {
        matches!(frame.msg, Some(Msg::OpResult(_)))
    });
    let Some(Msg::OpResult(result)) = &replies.last().expect("operation reply").msg else {
        panic!("expected operation result");
    };
    assert!(result.ok, "history operation failed: {}", result.error_code);
}

#[test]
fn history_lists_both_directions_reassembles_unicode_mebibyte_and_clears() {
    use protocol::ipc::{gui_frame::Msg, ClipboardHistoryDirection};
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    // Seven UTF-8 bytes per repetition puts multibyte scalars across chunk boundaries.
    let mut large = "界🙂".repeat((1024 * 1024) / 7);
    large.push_str(&"x".repeat(1024 * 1024 - large.len()));
    assert_eq!(large.len(), 1024 * 1024);
    harness.a.copy_to_clipboard(&large);
    harness
        .b
        .wait_for_clipboard(&large, SETTLE)
        .expect("large transfer");
    wait_history(&harness.a, 1);
    wait_history(&harness.b, 1);
    let reply = "Ответ обратно — café 🙂";
    harness.b.copy_to_clipboard(reply);
    harness
        .a
        .wait_for_clipboard(reply, SETTLE)
        .expect("reverse transfer");
    for (node, peer, directions) in [
        (
            &harness.a,
            "receiver",
            [
                ClipboardHistoryDirection::Received,
                ClipboardHistoryDirection::Sent,
            ],
        ),
        (
            &harness.b,
            "source",
            [
                ClipboardHistoryDirection::Sent,
                ClipboardHistoryDirection::Received,
            ],
        ),
    ] {
        let items = wait_history(node, 2);
        for (item, direction) in items.iter().zip(directions) {
            assert_eq!(item.direction, direction as i32);
            assert_eq!(item.peer_name, peer);
            assert!(item.time_unix_ms > 0);
        }
        assert_ne!(items[0].id, items[1].id);
        assert_eq!(items[0].size_bytes, reply.len() as u64);
        assert_eq!(items[0].preview, reply);
        assert_eq!(items[1].size_bytes, large.len() as u64);
        assert_eq!(
            items[1].preview,
            large.chars().take(120).collect::<String>()
        );
        assert_eq!(history_text(node, items[0].id), reply);
        assert_eq!(history_text(node, items[1].id), large);
    }
    history_operation(
        &harness.a,
        Msg::ClearClipboardHistory(protocol::ipc::ClearClipboardHistory {}),
    );
    assert!(history(&harness.a).is_empty());
    assert_eq!(history(&harness.b).len(), 2, "clear is node-local");
    assert_eq!(harness.a.clipboard_text().as_deref(), Some(reply));
}

#[test]
fn copying_history_is_local_only_with_sharing_enabled_or_disabled() {
    use protocol::ipc::gui_frame::Msg;
    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    let old = "history local copy — старый 🙂";
    harness.a.copy_to_clipboard(old);
    harness
        .b
        .wait_for_clipboard(old, SETTLE)
        .expect("first transfer");
    let entry = wait_history(&harness.a, 1).remove(0);
    let current = "peer must keep this newer text";
    harness.b.copy_to_clipboard(current);
    harness
        .a
        .wait_for_clipboard(current, SETTLE)
        .expect("newer transfer");
    wait_history(&harness.a, 2);
    wait_history(&harness.b, 2);
    for enabled in [true, false] {
        if !enabled {
            toggle_clipboard(&harness.a, false);
            let disabled_local = "local text changed while sharing is disabled";
            harness.a.copy_to_clipboard(disabled_local);
            harness
                .a
                .wait_for_clipboard(disabled_local, SETTLE)
                .expect("disabled sharing still observes the local clipboard");
        }
        let offers = [
            harness.a.log().matches("clipboard offer").count(),
            harness.b.log().matches("clipboard offer").count(),
        ];
        history_operation(
            &harness.a,
            Msg::GetClipboardHistoryItem(protocol::ipc::GetClipboardHistoryItem {
                id: entry.id,
                copy_to_local: true,
            }),
        );
        harness
            .a
            .wait_for_clipboard(old, SETTLE)
            .expect("local history copy");
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(harness.b.clipboard_text().as_deref(), Some(current));
        assert_eq!(
            harness.a.log().matches("clipboard offer").count(),
            offers[0]
        );
        assert_eq!(
            harness.b.log().matches("clipboard offer").count(),
            offers[1]
        );
        assert_eq!(
            history(&harness.a).len(),
            2,
            "local copy creates no transfer"
        );
        assert_eq!(history(&harness.b).len(), 2);
    }
}

#[test]
fn transferred_secrets_are_excluded_from_sent_and_received_history() {
    let _serial = common::serial();
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        for root in [a_root, b_root] {
            common::append_config(
                root,
                "\n[clipboard]\nenabled = true\nskip_secret_marked = false\n",
            );
        }
    });
    harness.wait_linked();
    for (sender, receiver, secret) in [
        (&harness.a, &harness.b, "secret A → B"),
        (&harness.b, &harness.a, "secret B → A"),
    ] {
        sender.copy_to_clipboard_marked(secret, &["kde_secret"]);
        receiver
            .wait_for_clipboard(secret, SETTLE)
            .expect("configured secret transfer");
        // A subsequent successful transfer is a barrier for the sender's completion event.
        let ordinary = format!("public after {secret}");
        sender.copy_to_clipboard(&ordinary);
        receiver
            .wait_for_clipboard(&ordinary, SETTLE)
            .expect("public transfer");
    }
    for node in [&harness.a, &harness.b] {
        let items = wait_history(node, 2);
        assert_eq!(items[0].preview, "public after secret B → A");
        assert_eq!(items[1].preview, "public after secret A → B");
        for item in items {
            assert!(history_text(node, item.id).starts_with("public after "));
        }
    }
}
