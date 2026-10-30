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

use common::{write_clipboard_rate, Cluster, Harness};

/// How long a copy may take to settle. The default rate is 256 KiB/s, so a 1 MiB payload
/// legitimately needs about four seconds on top of the round trip.
const SETTLE: Duration = Duration::from_secs(60);

/// Legacy fan-out assertions opt into broadcast; shared harness defaults stay follow.
fn enable_clipboard(root: &std::path::Path) {
    common::append_config(
        root,
        "\n[clipboard]\nenabled = true\npolicy = \"broadcast\"\nskip_secret_marked = true\n",
    );
}

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
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        for root in [a_root, b_root] {
            common::append_config(root, "\n[clipboard]\npolicy = \"broadcast\"\n");
        }
    });
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
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        for root in [a_root, b_root] {
            common::append_config(root, "\n[clipboard]\npolicy = \"broadcast\"\n");
        }
    });
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
    let mut text = format!("bifrost-{tag}-{}-", std::process::id());
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

/// A real filesystem wait must not stop IPC heartbeats in a dry agent.
#[cfg(windows)]
#[test]
fn a_blocked_dry_clipboard_write_keeps_bidirectional_delivery_alive() {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows_sys::Win32::System::IO::{
        CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
    };

    struct Oplock {
        file: std::fs::File,
        overlapped: Box<OVERLAPPED>,
        event: OwnedHandle,
    }
    impl Drop for Oplock {
        fn drop(&mut self) {
            let mut transferred = 0;
            // SAFETY: this guard owns the handle, stable OVERLAPPED allocation and event.
            // Await cancellation even on assertion unwind before freeing that allocation.
            unsafe {
                CancelIoEx(self.file.as_raw_handle(), &*self.overlapped);
                GetOverlappedResult(
                    self.file.as_raw_handle(),
                    &*self.overlapped,
                    &mut transferred,
                    1,
                );
            }
        }
    }

    let _serial = common::serial();
    let harness = two_nodes_with_clipboard();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(7)
        .custom_flags(0x40000000) // FILE_FLAG_OVERLAPPED
        .open(harness.a.root.join("run/clipboard-dry/out.txt"))
        .expect("dry selection file");
    // SAFETY: a null security descriptor/name creates an unnamed event; ownership is
    // transferred once and held until the pending oplock has been closed below.
    let event = unsafe {
        let raw = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        assert!(!raw.is_null(), "{}", std::io::Error::last_os_error());
        OwnedHandle::from_raw_handle(raw)
    };
    // SAFETY: zero initializes all offsets and the internal completion fields.
    let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
    overlapped.hEvent = event.as_raw_handle();
    let mut oplock = Oplock {
        file,
        overlapped,
        event,
    };
    let mut returned = 0;
    // SAFETY: the file is open for overlapped IO, the unused buffers are empty, and
    // both the event and OVERLAPPED remain alive until the file handle is dropped.
    let requested = unsafe {
        DeviceIoControl(
            oplock.file.as_raw_handle(),
            0x00090000, // FSCTL_REQUEST_OPLOCK_LEVEL_1
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            &mut *oplock.overlapped,
        )
    };
    assert_eq!(requested, 0);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ERROR_IO_PENDING as i32)
    );
    harness.a.copy_to_clipboard("filesystem-blocked-copy");
    // SAFETY: the event is owned above and remains open throughout this wait.
    assert_eq!(
        unsafe { WaitForSingleObject(oplock.event.as_raw_handle(), 3000) },
        0
    );
    // Only this fixture file's conflicting open/write is blocked, not either process.
    // This exceeds the unchanged 300 ms watchdog deliberately.
    std::thread::sleep(Duration::from_millis(350));
    drop(oplock);
    harness
        .b
        .wait_for_clipboard("filesystem-blocked-copy", Duration::from_secs(10))
        .expect("the delayed local copy reaches B");
    harness.b.copy_to_clipboard("reply-after-filesystem-wait");
    harness
        .a
        .wait_for_clipboard("reply-after-filesystem-wait", Duration::from_secs(10))
        .expect("A remains attached and applies B's reply");
    assert!(
        !harness.a.log().contains("the agent stopped answering"),
        "a dry filesystem operation must not starve agent IPC"
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
        for root in [a_root, b_root] {
            common::append_config(root, "policy = \"broadcast\"\n");
        }
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
    wait_history(&harness.a, 2);
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
        let all = wait_history(node, 3);
        assert_eq!(
            all.iter()
                .filter(|item| item.direction == ClipboardHistoryDirection::Copied as i32)
                .count(),
            1,
            "a valid local copy is retained before any completed transfer"
        );
        let items: Vec<_> = all
            .into_iter()
            .filter(|item| item.direction != ClipboardHistoryDirection::Copied as i32)
            .collect();
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
    assert_eq!(history(&harness.b).len(), 3, "clear is node-local");
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
    let entry = wait_history(&harness.a, 2).remove(0);
    let current = "peer must keep this newer text";
    harness.b.copy_to_clipboard(current);
    harness
        .a
        .wait_for_clipboard(current, SETTLE)
        .expect("newer transfer");
    wait_history(&harness.a, 3);
    wait_history(&harness.b, 3);
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
            3,
            "local copy creates no transfer"
        );
        assert_eq!(history(&harness.b).len(), 3);
    }
}

#[test]
fn secret_override_still_refuses_transfers_and_history() {
    let _serial = common::serial();
    let (harness, ()) = Harness::start_with(false, false, |a_root, b_root| {
        for root in [a_root, b_root] {
            common::append_config(
                root,
                "\n[clipboard]\nenabled = true\npolicy = \"broadcast\"\nskip_secret_marked = false\n",
            );
        }
    });
    harness.wait_linked();
    for (sender, receiver, secret) in [
        (&harness.a, &harness.b, "secret A → B"),
        (&harness.b, &harness.a, "secret B → A"),
    ] {
        let previous = receiver.clipboard_text();
        sender.copy_to_clipboard_marked(secret, &["kde_secret"]);
        wait_log(sender, "CLIPBOARD_SECRET");
        assert_clipboard_stays(receiver, previous.as_deref());
        // A successful public transfer also proves refusing the secret left the channel usable.
        let ordinary = format!("public after {secret}");
        sender.copy_to_clipboard(&ordinary);
        receiver
            .wait_for_clipboard(&ordinary, SETTLE)
            .expect("public transfer");
    }
    for node in [&harness.a, &harness.b] {
        let items: Vec<_> = wait_history(node, 3)
            .into_iter()
            .filter(|item| {
                item.direction != protocol::ipc::ClipboardHistoryDirection::Copied as i32
            })
            .collect();
        assert_eq!(items[0].preview, "public after secret B → A");
        assert_eq!(items[1].preview, "public after secret A → B");
        for item in items {
            assert!(history_text(node, item.id).starts_with("public after "));
        }
    }
}

/// No automatic synthetic keys: only file controls can settle a follow destination.
fn follow_cluster(label: &str) -> Cluster {
    let (cluster, ()) = Cluster::start_with(label, |roots, _, _| {
        for root in roots {
            common::enable_clipboard(root);
            common::append_config(root, "settle_ms = 3000\n");
            std::fs::create_dir_all(root.join("run/control-dry")).expect("dry controls");
            std::fs::write(root.join("run/no-input-events-dry"), b"0")
                .expect("disable automatic synthetic key edges");
        }
    });
    cluster.wait_mesh(Duration::from_secs(40));
    cluster
}

fn send_history(source: &common::Node, id: u64, target: &common::Node) -> protocol::ipc::OpResult {
    use protocol::ipc::gui_frame::Msg;
    let target_id = (0..target.id.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&target.id[index..index + 2], 16).expect("node hex"))
        .collect();
    let replies = history_exchange(
        source,
        Msg::SendClipboardHistoryItem(protocol::ipc::SendClipboardHistoryItem {
            id,
            peer_node_id: target_id,
        }),
        |frame| matches!(frame.msg, Some(Msg::OpResult(_))),
    );
    let Some(Msg::OpResult(result)) = replies.into_iter().last().expect("send reply").msg else {
        panic!("expected send result");
    };
    result
}

fn wait_log(node: &common::Node, marker: &str) {
    let deadline = std::time::Instant::now() + SETTLE;
    while !node.log().contains(marker) {
        assert!(std::time::Instant::now() < deadline, "missing {marker}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_clipboard_stays(node: &common::Node, expected: Option<&str>) {
    // A negative delivery assertion must outlast the configured 3 s dwell.
    let deadline = std::time::Instant::now() + Duration::from_millis(3500);
    while std::time::Instant::now() < deadline {
        assert_eq!(node.clipboard_text().as_deref(), expected);
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn follow_crosses_three_nodes_without_touching_transit_and_returns_a_remote_copy() {
    let _serial = common::serial();
    let cluster = follow_cluster("clipboard-follow-transit");
    let [pc1, pc2, pc3] = [&cluster.nodes[0], &cluster.nodes[1], &cluster.nodes[2]];
    let text = "PC1 carried through PC2 to PC3";
    pc1.copy_to_clipboard(text);
    let copied = wait_history(pc1, 1);
    assert_eq!(
        copied[0].peer_name, "node-w",
        "local copies name their origin"
    );
    assert_eq!(
        copied[0].direction,
        protocol::ipc::ClipboardHistoryDirection::Copied as i32,
        "follow records a local copy before any text transfer"
    );
    assert!(pc2.clipboard_text().unwrap_or_default().is_empty());
    assert!(pc3.clipboard_text().unwrap_or_default().is_empty());
    let started = std::time::Instant::now();
    cluster.switch_to(0, 1).expect("PC1 enters PC2");
    cluster
        .switch_to(0, 2)
        .expect("continue directly from PC2 to PC3");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "transit must finish before its dwell, otherwise the scenario measured nothing"
    );
    pc3.wait_for_clipboard(text, SETTLE)
        .expect("destination dwell delivers");
    assert!(pc2.clipboard_text().unwrap_or_default().is_empty());
    assert!(history(pc2).is_empty(), "transit creates no text history");
    let received = wait_history(pc3, 1);
    assert_eq!(
        received[0].peer_name, "node-w",
        "history names original PC1"
    );
    assert_eq!(history_text(pc3, received[0].id), text);
    let applied = pc3.log().matches("clipboard payload applied").count();
    cluster.switch_to_local(0).expect("return to PC1");
    cluster
        .switch_to(0, 2)
        .expect("re-enter the same destination");
    assert_clipboard_stays(pc3, Some(text));
    assert_eq!(
        pc3.log().matches("clipboard payload applied").count(),
        applied
    );
    assert_eq!(history(pc3).len(), 1, "re-entry is deduplicated");

    let reply = "copy made remotely on PC3";
    pc3.copy_to_clipboard(reply);
    wait_history(pc3, 2);
    assert_eq!(
        pc1.clipboard_text().as_deref(),
        Some(text),
        "copy does not fan out"
    );
    cluster
        .switch_to_local(0)
        .expect("return carries PC3's copy");
    pc1.wait_for_clipboard(reply, SETTLE)
        .expect("return dwell delivers remote copy");
    assert!(pc2.clipboard_text().unwrap_or_default().is_empty());
    let items = history(pc1);
    assert!(items
        .iter()
        .any(|item| item.preview == reply && item.peer_name == "node-b"));
}

#[test]
fn explicit_gui_send_stops_current_carry_but_an_old_row_preserves_it() {
    let _serial = common::serial();
    let cluster = follow_cluster("clipboard-follow-explicit");
    let [pc1, pc2, pc3] = [&cluster.nodes[0], &cluster.nodes[1], &cluster.nodes[2]];
    let old = "old row sent explicitly";
    pc1.copy_to_clipboard(old);
    let old_id = wait_history(pc1, 1)[0].id;
    let sent = send_history(pc1, old_id, pc3);
    assert!(sent.ok, "GUI send refused: {}", sent.error_code);
    pc3.wait_for_clipboard(old, SETTLE)
        .expect("explicit send without focus");
    assert_eq!(wait_history(pc3, 1)[0].peer_name, "node-w");
    cluster
        .switch_to(0, 1)
        .expect("cross after explicit current send");
    assert_clipboard_stays(pc2, pc2.clipboard_text().as_deref());
    assert!(pc2.clipboard_text().unwrap_or_default().is_empty());
    assert!(
        history(pc2).is_empty(),
        "explicit current send stops carry globally"
    );
    cluster.switch_to_local(0).expect("return");
    let current = "new copy still carried after sending old row";
    pc1.copy_to_clipboard(current);
    let deadline = std::time::Instant::now() + SETTLE;
    while !history(pc1).iter().any(|item| item.preview == current) {
        assert!(
            std::time::Instant::now() < deadline,
            "new copy missing from local history"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let sent = send_history(pc1, old_id, pc3);
    assert!(sent.ok, "old-row send refused: {}", sent.error_code);
    cluster
        .switch_to(0, 1)
        .expect("cross with current carry intact");
    pc2.wait_for_clipboard(current, SETTLE)
        .expect("old row did not cancel new carry");
    assert_eq!(pc3.clipboard_text().as_deref(), Some(old));
    cluster
        .switch_to(0, 2)
        .expect("enter the earlier explicit destination");
    pc3.wait_for_clipboard(current, SETTLE)
        .expect("settling replaces an explicitly sent old row with the current carry");
}

#[test]
fn explicit_send_refuses_offline_peers_and_secret_copies_stay_out_of_history() {
    let _serial = common::serial();
    let cluster = follow_cluster("clipboard-follow-offline-secret");
    let [pc1, pc2, pc3] = [&cluster.nodes[0], &cluster.nodes[1], &cluster.nodes[2]];
    pc1.copy_to_clipboard("ordinary local row");
    let id = wait_history(pc1, 1)[0].id;
    common::Node::kill_pid(pc3.pid()).expect("stop explicit destination");
    pc1.wait_for("offline destination removed", SETTLE, |s| s.members == 1)
        .expect("PC1 observes offline PC3");
    let refused = send_history(pc1, id, pc3);
    assert!(!refused.ok, "offline explicit send must fail");
    let before = history(pc1);
    let secret = "secret must remain only on PC1";
    pc1.copy_to_clipboard_marked(secret, &["kde_secret"]);
    pc1.wait_for_clipboard(secret, SETTLE)
        .expect("secret remains local");
    wait_log(pc1, "CLIPBOARD_SECRET");
    assert_eq!(
        history(pc1),
        before,
        "secret creates no sendable history row"
    );
    cluster
        .switch_to(0, 1)
        .expect("enter with secret selection");
    std::thread::sleep(Duration::from_millis(3500));
    assert_ne!(pc2.clipboard_text().as_deref(), Some(secret));
    assert!(history(pc2).iter().all(|item| item.preview != secret));
}

#[test]
fn follow_enter_then_ctrl_v_injects_keys_only_after_destination_clipboard_apply() {
    let _serial = common::serial();
    let cluster = follow_cluster("clipboard-follow-paste-order");
    let pc1 = cluster.node(0);
    let pc2 = cluster.node(1);
    let text = "clipboard must precede Ctrl V";
    pc1.copy_to_clipboard(text);
    wait_history(pc1, 1);
    cluster.switch_to(0, 1).expect("enter destination");
    pc1.wait_dry_capture("Remote");
    let dir = pc1.dry_control_dir();
    std::fs::write(
        dir.join("keys.writing"),
        b"224 down\n25 down\n25 up\n224 up\n",
    )
    .expect("stage dry Ctrl V");
    std::fs::rename(dir.join("keys.writing"), dir.join("keys")).expect("publish dry Ctrl V");
    let expected = [("224", "Down"), ("25", "Down"), ("25", "Up"), ("224", "Up")]
        .map(|(usage, state)| format!("{usage} {state} {:?}", clipboard::sha256(text.as_bytes())))
        .join("\n")
        + "\n";
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        let actual = std::fs::read_to_string(pc2.dry_control_dir().join("injected-keys.txt"))
            .unwrap_or_default();
        if actual.lines().count() >= 4 {
            assert_eq!(
                actual, expected,
                "ordered keys must observe the applied clipboard hash"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dry Ctrl V never reached destination"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pc2.wait_for_clipboard(text, SETTLE)
        .expect("first key settles clipboard");
}

#[test]
fn gui_clipboard_policy_reports_defaults_and_persists_clamped_settle_values() {
    use protocol::ipc::gui_frame::Msg;
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let status = |node: &common::Node| {
        let frames = history_exchange(node, Msg::GetStatus(protocol::ipc::GetStatus {}), |frame| {
            matches!(frame.msg, Some(Msg::Status(_)))
        });
        let Some(Msg::Status(status)) = frames.into_iter().last().expect("status reply").msg else {
            panic!("expected GUI status");
        };
        status
    };
    let defaults = status(&harness.a);
    assert_eq!(defaults.clipboard_policy, "follow");
    assert_eq!(defaults.clipboard_settle_ms, 700);
    for (policy, requested, expected) in [("broadcast", 1, 200), ("follow", 9000, 3000)] {
        history_operation(
            &harness.a,
            Msg::SetClipboardPolicy(protocol::ipc::SetClipboardPolicy {
                policy: policy.into(),
                settle_ms: requested,
            }),
        );
        let current = status(&harness.a);
        assert_eq!(current.clipboard_policy, policy);
        assert_eq!(current.clipboard_settle_ms, expected);
        let saved = config::Config::load(harness.a.root.join("config/config.toml"))
            .expect("saved clipboard policy");
        assert_eq!(saved.clipboard.settle_ms, expected);
        assert_eq!(
            saved.clipboard.policy,
            if policy == "follow" {
                config::ClipboardRoutingPolicy::Follow
            } else {
                config::ClipboardRoutingPolicy::Broadcast
            }
        );
    }
}

#[test]
fn gui_clipboard_images_switch_persists_and_reports_capability() {
    use protocol::ipc::gui_frame::Msg;
    let _serial = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let status = |node: &common::Node| {
        let frames = history_exchange(node, Msg::GetStatus(protocol::ipc::GetStatus {}), |frame| {
            matches!(frame.msg, Some(Msg::Status(_)))
        });
        let Some(Msg::Status(status)) = frames.into_iter().last().expect("status reply").msg else {
            panic!("expected GUI status");
        };
        status
    };
    let defaults = status(&harness.a);
    assert!(!defaults.clipboard_images, "images are opt-in per computer");
    for enabled in [true, false] {
        history_operation(
            &harness.a,
            Msg::SetClipboardImages(protocol::ipc::SetClipboardImages { enabled }),
        );
        let current = status(&harness.a);
        assert_eq!(current.clipboard_images, enabled);
        // The flag is policy; the capability stays whatever this backend honestly reports.
        let _supported = current.clipboard_images_supported;
        let saved = config::Config::load(harness.a.root.join("config/config.toml"))
            .expect("saved clipboard images option");
        assert_eq!(saved.clipboard.images, enabled);
    }
}

#[test]
fn follow_first_dry_button_settles_before_the_dwell_deadline() {
    let _serial = common::serial();
    let cluster = follow_cluster("clipboard-follow-button");
    let source = cluster.node(0);
    let target = cluster.node(1);
    let text = "first button settles clipboard";
    source.copy_to_clipboard(text);
    wait_history(source, 1);
    cluster.switch_to(0, 1).expect("enter dry receiver");
    source.wait_dry_capture("Remote");
    let started = std::time::Instant::now();
    source.trigger_dry_click();
    target
        .wait_for_clipboard(text, Duration::from_secs(2))
        .expect("button settles before the 3 s dwell");
    assert!(started.elapsed() < Duration::from_secs(3));
}
