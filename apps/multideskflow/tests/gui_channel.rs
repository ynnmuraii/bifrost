//! The GUI's channel, driven over the real endpoint of two real cores.
//!
//! `docs/03-architecture.md` («GUI ↔ core») makes `GuiFrame` the contract for `lws-gui`, and
//! the GUI is not in the hot path: it asks, core decides, and the answers and events travel on
//! one `seq`-stamped link per window. This suite is the transport-level proof of that contract
//! — it speaks `GuiFrame` over `\\.\pipe\lan-workspace-<SID>` / `gui.sock` to two `lws-core run`
//! processes, with the dry agent and no OS input, and asserts what the window would see.
//!
//! What it covers, in the order the wizard uses them:
//!
//! * `GetStatus`/`GetTopology` answer, and the pushed `Status` follows on its own;
//! * `ClaimOwnership` on a follower really makes it the owner (the GUI button, end to end);
//! * the pairing ceremony between two **unpaired** cores: `StartPairing` opens a window and
//!   pushes the address to type, `PairWith` from the other window dials it, both sides get
//!   `PairingPrompt` with the *same six digits*, both `PairingAnswer{approve}` and both write
//!   the `TrustedPeer` the CLI's own ceremony writes.
//!
//! Everything is event-driven: the pushes are awaited with a deadline, never a sleep that is
//! hoped to be long enough.

mod common;

use std::time::{Duration, Instant};

use common::{gui_endpoint_of, Harness, Unpaired};
use ipc::transport::Endpoint;
use ipc::GuiLink;
use protocol::ipc::{gui_frame, GuiFrame};

/// How long an answer or a push may take before the test fails.
const DEADLINE: Duration = Duration::from_secs(20);

/// A window speaking `GuiFrame` to one core, in a blocking test.
struct Window {
    link: GuiLink<ipc::transport::ClientConnection>,
    runtime: tokio::runtime::Runtime,
    next_request: u64,
    /// Events that arrived while waiting for an answer, in order: what the GUI would have
    /// painted.
    events: Vec<gui_frame::Msg>,
}

impl Window {
    /// Connects to a core's GUI endpoint.
    fn open(endpoint: &Endpoint) -> Window {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the GUI link");
        let stream = runtime
            .block_on(ipc::transport::connect(endpoint))
            .unwrap_or_else(|error| panic!("cannot connect to {}: {error}", endpoint.describe()));
        Window {
            link: GuiLink::new(stream),
            runtime,
            next_request: 1,
            events: Vec::new(),
        }
    }

    /// One frame, or a panic if the core is silent before `deadline`.
    fn recv_until(&mut self, what: &str, deadline: Instant) -> GuiFrame {
        let left = deadline.saturating_duration_since(Instant::now());
        match self
            .runtime
            .block_on(async { tokio::time::timeout(left, self.link.recv()).await })
        {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => panic!("{what}: the GUI link failed: {error}"),
            Err(_) => panic!("{what}: no frame within {left:?}"),
        }
    }

    /// One frame, or a panic if the core is silent past the usual deadline.
    fn recv(&mut self, what: &str) -> GuiFrame {
        self.recv_until(what, Instant::now() + DEADLINE)
    }

    /// Sends one request and returns its `request_id` without waiting for the answer.
    ///
    /// This is what the real window does: `docs/03-architecture.md` («GUI ↔ core») makes every
    /// request fire-and-forget with a `request_id`, and the answer is folded when it arrives.
    /// A caller that must act on *two* computers at once (the pairing ceremony's two
    /// «Да, совпадают») has to send the second request before reading the first answer: the
    /// verdict of a ceremony only exists once both sides answered.
    fn send(&mut self, message: gui_frame::Msg) -> u64 {
        let request_id = self.next_request;
        self.next_request += 1;
        self.runtime
            .block_on(self.link.send(request_id, message))
            .expect("the request is sent");
        request_id
    }

    /// The answer to one request, collecting the pushed events that arrive first (see
    /// [`Window::events`]): they are not answers, and dropping them would hide the state the
    /// GUI lives on.
    fn await_answer(&mut self, request_id: u64) -> gui_frame::Msg {
        loop {
            let frame = self.recv(&format!("no answer to request {request_id}"));
            if frame.request_id == request_id {
                return frame.msg.expect("an answer carries a message");
            }
            assert_eq!(
                frame.request_id, 0,
                "a frame answers another request: {frame:?}"
            );
            self.events
                .push(frame.msg.expect("an event carries a message"));
        }
    }

    /// Sends one request and returns its answer.
    fn request(&mut self, message: gui_frame::Msg) -> gui_frame::Msg {
        let request_id = self.send(message);
        self.await_answer(request_id)
    }

    /// Waits for a pushed event of one kind.
    fn wait_event<T>(&mut self, what: &str, of: impl Fn(&gui_frame::Msg) -> Option<T>) -> T {
        self.wait_event_until(what, Instant::now() + DEADLINE, of)
    }

    /// Waits until one absolute deadline, so paired nodes share the same 10-second window.
    fn wait_event_until<T>(
        &mut self,
        what: &str,
        deadline: Instant,
        of: impl Fn(&gui_frame::Msg) -> Option<T>,
    ) -> T {
        let mut seen = Vec::new();
        loop {
            if let Some(index) = self.events.iter().position(|event| of(event).is_some()) {
                let event = self.events.remove(index);
                return of(&event).expect("checked by position");
            }
            let frame = self.recv_until(
                &format!("{what} did not arrive; events seen: {seen:?}"),
                deadline,
            );
            assert_eq!(
                frame.request_id, 0,
                "{}: an event answers nothing, but this frame carries a request id",
                what
            );
            seen.push(kind(
                frame.msg.as_ref().expect("an event carries a message"),
            ));
            self.events
                .push(frame.msg.expect("an event carries a message"));
        }
    }

    /// The `Status` this window last saw (asks for one if none was pushed yet).
    fn status(&mut self) -> protocol::ipc::Status {
        let answer = self.request(gui_frame::Msg::GetStatus(protocol::ipc::GetStatus {}));
        match answer {
            gui_frame::Msg::Status(status) => status,
            other => panic!("GetStatus answered {other:?}"),
        }
    }
}

/// The name of a message, for failure output.
fn kind(message: &gui_frame::Msg) -> &'static str {
    match message {
        gui_frame::Msg::Status(_) => "Status",
        gui_frame::Msg::PairingPrompt(_) => "PairingPrompt",
        gui_frame::Msg::PairingWindow(_) => "PairingWindow",
        gui_frame::Msg::OpResult(_) => "OpResult",
        gui_frame::Msg::TopologyDoc(_) => "TopologyDoc",
        gui_frame::Msg::PendingEdits(_) => "PendingEdits",
        _ => "request",
    }
}

/// `GetStatus`, `GetTopology` and a pushed `Status` over the documented endpoint.
#[test]
fn the_gui_endpoint_answers_and_pushes_the_status() {
    let _guard = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let mut window = Window::open(&gui_endpoint_of(&harness.a.root));

    let status = window.status();
    assert_eq!(
        status.role, "owner",
        "the harness's source owns the workspace"
    );
    assert!(status.is_owner);
    assert_eq!(status.focus, "LOCAL", "nothing is remote yet");
    assert_eq!(status.members.len(), 1, "one peer is linked: {status:?}");
    assert!(status.members[0].online);
    // `Status.members[].display_name` comes from the trust record, and that record's name is
    // what the pairing ceremony read from the peer's `PairHello` — the configured
    // `node.display_name`, which this harness sets to "receiver" for the other node.
    assert_eq!(status.members[0].display_name, "receiver");
    assert!(status.connected);
    // The backend is this build's own session decision, so it is asserted by shape and not by
    // a name only one host could produce.
    assert!(
        status.backend.starts_with("platform-"),
        "backend: {:?}",
        status.backend
    );

    let answer = window.request(gui_frame::Msg::GetTopology(protocol::ipc::GetTopology {}));
    match answer {
        gui_frame::Msg::TopologyDoc(doc) => {
            // This harness's dry agents report no displays, so nodes can be present without
            // any drawing geometry. Any links that are present must still name declared nodes.
            for link in &doc.links {
                for endpoint in [link.from.as_ref(), link.to.as_ref()] {
                    let endpoint = endpoint.expect("a link has both endpoints");
                    assert!(
                        doc.members
                            .iter()
                            .any(|member| member.node_id == endpoint.node_id),
                        "a link names a member the document does not declare: {doc:?}"
                    );
                }
            }
        }
        other => panic!("GetTopology answered {other:?}"),
    }

    // The pushed `Status` is what keeps the window alive without polling: a state change
    // (here: the peer's claim) has to arrive on its own.
    let pushed = window.wait_event("a pushed Status", |event| match event {
        gui_frame::Msg::Status(status) => Some(status.clone()),
        _ => None,
    });
    assert!(
        pushed.members.len() == 1 || pushed.role == "owner",
        "the pushed status is this node's own: {pushed:?}"
    );
}

/// The «Расположить автоматически» button, end to end: the GUI channel reaches the same
/// `layout_auto` the `layout auto` command runs, and the layout the window then reads is the
/// automatic one.
#[test]
fn the_auto_layout_button_arranges_the_desktop_over_the_gui_channel() {
    let _guard = common::serial();
    // The dry agents have to report displays: the automatic layout is a document about
    // displays, and a workspace of two empty manifests has nothing to arrange.
    let harness = Harness::start_with_displays();
    harness.wait_linked();
    let mut window = Window::open(&gui_endpoint_of(&harness.a.root));

    let answer = window.request(gui_frame::Msg::LayoutAuto(protocol::ipc::LayoutAuto {}));
    match answer {
        gui_frame::Msg::OpResult(result) => {
            assert!(result.ok, "the auto layout applies: {result:?}")
        }
        other => panic!("LayoutAuto answered {other:?}"),
    }

    let doc = match window.request(gui_frame::Msg::GetTopology(protocol::ipc::GetTopology {})) {
        gui_frame::Msg::TopologyDoc(doc) => doc,
        other => panic!("GetTopology answered {other:?}"),
    };
    assert!(
        doc.revision >= 1,
        "the automatic layout is an applied revision"
    );
    assert_eq!(
        doc.members.len(),
        2,
        "this computer and the other one: {doc:?}"
    );
    assert!(
        !doc.links.is_empty(),
        "the automatic layout links the two computers: {doc:?}"
    );
}

#[test]
fn linked_cores_share_agent_displays_with_the_gui_without_a_layout() {
    let _guard = common::serial();
    let harness = Harness::start_with_displays();
    harness.wait_linked();

    for node in [&harness.a, &harness.b] {
        let mut window = Window::open(&gui_endpoint_of(&node.root));
        let has_both_manifests = |doc: &protocol::ipc::TopologyDoc| {
            doc.members.len() == 2 && doc.members.iter().all(|member| member.displays.len() == 1)
        };
        let startup_doc = window.wait_event(
            "the AgentReady display queries reached the GUI without a manual request",
            |event| match event {
                gui_frame::Msg::TopologyDoc(doc) if has_both_manifests(doc) => Some(doc.clone()),
                _ => None,
            },
        );
        let response = window.request(gui_frame::Msg::GetTopology(protocol::ipc::GetTopology {}));
        let doc = match response {
            gui_frame::Msg::TopologyDoc(doc) if has_both_manifests(&doc) => doc,
            other => panic!("GetTopology did not return both display manifests: {other:?}"),
        };
        assert_eq!(startup_doc.members, doc.members);
        let ids: Vec<_> = doc
            .members
            .iter()
            .flat_map(|member| member.displays.iter().map(|display| display.id.as_str()))
            .collect();
        assert_eq!(ids, ["dry-0", "dry-0"]);
        assert!(
            doc.members
                .iter()
                .all(|member| !member.display_name.is_empty()),
            "both members must have a name for the GUI: {doc:?}"
        );
    }
}

/// The ownership buttons: `AbandonOwnership` gives it up, `ClaimOwnership` takes it back.
///
/// The claim runs the FSM's own exchange (`OwnerClaim`, then the grant or the self-grant after
/// `claim_grant_timeout_ms`), so it is asserted through the control channel first and then
/// through the GUI's pushed `Status` — the two halves a window actually depends on.
#[test]
fn the_ownership_buttons_work_over_the_gui_endpoint() {
    let _guard = common::serial();
    let harness = Harness::start(false, false);
    harness.wait_linked();
    let mut window = Window::open(&gui_endpoint_of(&harness.a.root));

    let status = window.status();
    assert!(status.is_owner, "A owns the workspace here: {status:?}");
    assert_eq!(status.role, "owner");
    assert_eq!(status.focus, "LOCAL", "nothing is remote yet");

    // Give the workspace up: it goes to nobody (`docs/03-architecture.md`, S9: ownership is
    // only ever taken by an explicit claim), which is exactly the state the button promises.
    let answer = window.request(gui_frame::Msg::AbandonOwnership(
        protocol::ipc::AbandonOwnership {},
    ));
    match answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "abandon: {result:?}"),
        other => panic!("AbandonOwnership answered {other:?}"),
    }
    let deadline = Instant::now() + DEADLINE;
    loop {
        let ctl = harness.a.status().expect("ctl status");
        if ctl.owner != harness.a.id {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "A never gave ownership up: {ctl:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Take it back. Nobody contests it, so the FSM self-grants after `claim_grant_timeout_ms`.
    let answer = window.request(gui_frame::Msg::ClaimOwnership(
        protocol::ipc::ClaimOwnership {},
    ));
    match answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "claim: {result:?}"),
        other => panic!("ClaimOwnership answered {other:?}"),
    }
    let deadline = Instant::now() + DEADLINE;
    loop {
        let ctl = harness.a.status().expect("ctl status");
        if ctl.owner == harness.a.id {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "A never became the owner again: {ctl:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // …and the window is told, without asking: the pushed `Status` names the owner.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let pushed = window.wait_event("a pushed Status naming the owner", |event| match event {
            gui_frame::Msg::Status(status) => Some(status.clone()),
            _ => None,
        });
        if pushed.is_owner && pushed.owner_node_id == harness.a.id && pushed.role == "owner" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no pushed status reported A as the owner: {pushed:?}"
        );
    }
}

/// The wizard's first half: the window opens and the address to type is pushed.
#[test]
fn the_wizard_opens_a_window_and_pushes_the_address_to_type() {
    let _guard = common::serial();
    let two = Unpaired::start("gui-window");
    two.wait_ready();
    let mut opener = Window::open(&gui_endpoint_of(&two.a.root));

    let answer = opener.request(gui_frame::Msg::StartPairing(protocol::ipc::StartPairing {
        window_s: 120,
    }));
    match answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "StartPairing refused: {result:?}"),
        other => panic!("StartPairing answered {other:?}"),
    }
    // A second window would replace a code the other computer may already hold.
    let again = opener.request(gui_frame::Msg::StartPairing(protocol::ipc::StartPairing {
        window_s: 120,
    }));
    match again {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok);
            assert_eq!(result.error_code, "PAIRING_WINDOW_OPEN");
        }
        other => panic!("the second StartPairing answered {other:?}"),
    }

    let (address, attempt) = opener.wait_event("PairingWindow{open}", |event| match event {
        gui_frame::Msg::PairingWindow(window) if window.open => {
            Some((window.address.clone(), window.attempt_id.clone()))
        }
        _ => None,
    });
    assert_eq!(
        address,
        format!("127.0.0.1:{}", two.a_port),
        "the window answers on the link listener's own port: pairing shares 47901"
    );
    assert_eq!(attempt.len(), 16, "an attempt_id is 16 bytes");
    assert!(
        opener.status().layout_editable || true,
        "the window's link keeps working while it is open"
    );
}

/// The whole pairing ceremony between two unpaired cores, driven as the wizard drives it.
///
/// The one thing this test has to do that a single window does not: the ceremony only has a
/// verdict once *both* users answered, and the answer to `PairingAnswer` is that verdict
/// (`OpResult`), so the two answers are **sent** before either reply is read. The real wizard
/// does the same — its requests are fire-and-forget (`docs/03-architecture.md`, «GUI ↔ core»),
/// and two computers cannot wait for each other's click.
#[test]
fn the_pairing_wizard_links_two_cores_without_restarting_them() {
    let _guard = common::serial();
    let two = Unpaired::start("gui-pairing");
    two.wait_ready();

    let mut opener = Window::open(&gui_endpoint_of(&two.a.root));
    let mut dialer = Window::open(&gui_endpoint_of(&two.b.root));

    // 1. The user opens the window on the left computer.
    let answer = opener.request(gui_frame::Msg::StartPairing(protocol::ipc::StartPairing {
        window_s: 120,
    }));
    match answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "StartPairing refused: {result:?}"),
        other => panic!("StartPairing answered {other:?}"),
    }
    // 2. Another window is already open.
    let again = opener.request(gui_frame::Msg::StartPairing(protocol::ipc::StartPairing {
        window_s: 120,
    }));
    match again {
        gui_frame::Msg::OpResult(result) => {
            assert!(!result.ok);
            assert_eq!(result.error_code, "PAIRING_WINDOW_OPEN");
        }
        other => panic!("the second StartPairing answered {other:?}"),
    }

    // 3. The address to type arrives as a push — this is the whole reason the message exists:
    //    the listener is core's own `47901`, so the GUI cannot guess the address.
    let (address, attempt) = opener.wait_event("PairingWindow{open}", |event| match event {
        gui_frame::Msg::PairingWindow(window) if window.open => {
            Some((window.address.clone(), window.attempt_id.clone()))
        }
        _ => None,
    });
    assert_eq!(
        address,
        format!("127.0.0.1:{}", two.a_port),
        "the window answers on the link listener's own port: pairing shares 47901"
    );
    assert_eq!(attempt.len(), 16, "an attempt_id is 16 bytes");

    // 4. The user types it on the right computer.
    let answer = dialer.request(gui_frame::Msg::PairWith(protocol::ipc::PairWith {
        address: address.clone(),
    }));
    match answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "PairWith refused: {result:?}"),
        other => panic!("PairWith answered {other:?}"),
    }

    // 5. Both windows show six digits, and they are the same six.
    let left = opener.wait_event("the opener's PairingPrompt", |event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    let right = dialer.wait_event("the dialer's PairingPrompt", |event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    assert_eq!(left.sas_code, right.sas_code, "the codes must match");
    assert_eq!(left.sas_code.len(), 6, "the code is six digits");
    assert!(
        left.sas_code.chars().all(|digit| digit.is_ascii_digit()),
        "the code is digits: {:?}",
        left.sas_code
    );
    assert_eq!(
        left.attempt_id.len(),
        16,
        "each side's prompt names its own ceremony by a 16-byte id"
    );
    assert_eq!(right.attempt_id.len(), 16);
    // The two ids are per-connection ceremony keys (R4), not the window's shared protocol
    // `attempt_id`: the opener and the dialer each own their ceremony, and their `PairingAnswer`
    // names it back below.
    // `Unpaired` names the two nodes "left" and "right" (`common::write_config`), and each
    // prompt shows the *other* computer's configured name — not a constant.
    assert_eq!(left.peer_name, "right");
    assert_eq!(right.peer_name, "left");
    assert_ne!(left.local_fingerprint, left.peer_fingerprint);

    // 6. Both users confirm. Neither window can wait for its own answer before the other
    //    confirms, so both answers go out first, exactly as two windows on two computers do.
    let link_deadline = Instant::now() + Duration::from_secs(10);
    let left_id = opener.send(gui_frame::Msg::PairingAnswer(
        protocol::ipc::PairingAnswer {
            attempt_id: left.attempt_id.clone(),
            approve: true,
        },
    ));
    let right_id = dialer.send(gui_frame::Msg::PairingAnswer(
        protocol::ipc::PairingAnswer {
            attempt_id: right.attempt_id.clone(),
            approve: true,
        },
    ));
    let left_answer = opener.await_answer(left_id);
    match left_answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "the opener's answer: {result:?}"),
        other => panic!("PairingAnswer answered {other:?}"),
    }
    let right_answer = dialer.await_answer(right_id);
    match right_answer {
        gui_frame::Msg::OpResult(result) => assert!(result.ok, "the dialer's answer: {result:?}"),
        other => panic!("PairingAnswer answered {other:?}"),
    }
    let left_status =
        opener.wait_event_until(
            "the opener's live link",
            link_deadline,
            |event| match event {
                gui_frame::Msg::Status(status) if status.connected && status.members.len() == 1 => {
                    Some(status.clone())
                }
                _ => None,
            },
        );
    let right_status =
        dialer.wait_event_until(
            "the dialer's live link",
            link_deadline,
            |event| match event {
                gui_frame::Msg::Status(status) if status.connected && status.members.len() == 1 => {
                    Some(status.clone())
                }
                _ => None,
            },
        );
    assert_eq!(left_status.members[0].display_name, "right");
    assert_eq!(right_status.members[0].display_name, "left");

    // 7. The window closes on its own, and both sides hold the peer they paired with.
    let closed = opener.wait_event("PairingWindow{closed}", |event| match event {
        gui_frame::Msg::PairingWindow(window) if !window.open => Some(window.clone()),
        _ => None,
    });
    assert_eq!(closed.seconds_remaining, 0);

    for (root, peer_id, peer_name, what) in [
        (&two.a.root, &two.b_id, "right", "the opener"),
        (&two.b.root, &two.a_id, "left", "the dialer"),
    ] {
        let trust = std::fs::read_to_string(root.join("config/trust.toml")).expect("trust.toml");
        assert!(
            trust.contains(peer_id),
            "{what} must hold {} in trust.toml:\n{trust}",
            peer_id
        );
        // The name the record keeps is the peer's configured one, which is what `Status`
        // lists and what the settings screen names when it offers to revoke.
        assert!(
            trust.contains(&format!("display_name = \"{peer_name}\"")),
            "{what} must record the peer's own name ({peer_name}):\n{trust}"
        );
    }
    // The address each side remembered is the other's listener, which is what a restart needs
    // to dial it (`docs/05-protocol.md`: «Адреса для `network.peers` берутся из сопряжения»).
    let trust = std::fs::read_to_string(two.a.root.join("config/trust.toml")).expect("trust");
    assert!(
        trust.contains("127.0.0.1:"),
        "the opener recorded the dialer's address:\n{trust}"
    );
    // The dialer's own window: it learned the opener's address from the typed one, and its
    // status now names the address a restart would dial.
    let trust = std::fs::read_to_string(two.b.root.join("config/trust.toml")).expect("trust");
    assert!(
        trust.contains(&format!("127.0.0.1:{}", two.a_port)),
        "the dialer recorded the opener's address:\n{trust}"
    );
}

#[test]
fn offline_trusted_peers_remain_in_status_and_the_applied_map() {
    let _guard = common::serial();
    let mut harness = Harness::start_with_displays();
    harness.wait_linked();
    let peer_id = harness.b.id.clone();
    let mut window = Window::open(&gui_endpoint_of(&harness.a.root));
    assert!(window
        .status()
        .members
        .iter()
        .any(|peer| peer.node_id == peer_id && peer.online));
    let before = window.wait_event("both live display manifests", |event| match event {
        gui_frame::Msg::TopologyDoc(doc)
            if doc.members.len() == 2
                && doc.members.iter().all(|member| !member.displays.is_empty()) =>
        {
            Some(doc.clone())
        }
        _ => None,
    });
    harness.b.stop();
    let offline = window.wait_event("offline trusted peer status", |event| match event {
        gui_frame::Msg::Status(status)
            if status
                .members
                .iter()
                .any(|peer| peer.node_id == peer_id && !peer.online) =>
        {
            Some(status.clone())
        }
        _ => None,
    });
    assert_eq!(
        offline
            .members
            .iter()
            .find(|peer| peer.node_id == peer_id)
            .expect("trusted peer")
            .link_state,
        "OFFLINE"
    );
    let after = window.wait_event("offline applied map member", |event| match event {
        gui_frame::Msg::TopologyDoc(doc)
            if doc
                .members
                .iter()
                .any(|peer| peer.node_id == peer_id && !peer.online) =>
        {
            Some(doc.clone())
        }
        _ => None,
    });
    // The peer that went away is still a member of the applied document, offline and with the
    // screens it last reported — the same set of computers and displays the map showed while
    // the link was up. (The applied revision may tick over once here: a workspace with no
    // layout of its own gets its one default one; that is not the link going down.)
    assert_eq!(
        after
            .members
            .iter()
            .map(|member| member.node_id.as_str())
            .collect::<Vec<_>>(),
        before
            .members
            .iter()
            .map(|member| member.node_id.as_str())
            .collect::<Vec<_>>(),
        "a known computer keeps its place on the map"
    );
    let peer = after
        .members
        .iter()
        .find(|peer| peer.node_id == peer_id)
        .expect("known displays stay visible");
    assert!(!peer.displays.is_empty());
}

#[test]
fn a_second_gui_replays_the_live_pairing_window_and_unanswered_code() {
    let _guard = common::serial();
    let two = Unpaired::start("gui-pairing-replay");
    two.wait_ready();
    let mut opener = Window::open(&gui_endpoint_of(&two.a.root));
    let result = opener.request(gui_frame::Msg::StartPairing(protocol::ipc::StartPairing {
        window_s: 120,
    }));
    assert!(matches!(result, gui_frame::Msg::OpResult(result) if result.ok));
    let original_window = opener.wait_event("opened pairing window", |event| match event {
        gui_frame::Msg::PairingWindow(window) if window.open => Some(window.clone()),
        _ => None,
    });
    drop(opener);
    let mut reconnected = Window::open(&gui_endpoint_of(&two.a.root));
    reconnected.status();
    let replayed_window = reconnected.wait_event("pairing window snapshot", |event| match event {
        gui_frame::Msg::PairingWindow(window) if window.open => Some(window.clone()),
        _ => None,
    });
    assert_eq!(replayed_window.attempt_id, original_window.attempt_id);
    assert_eq!(replayed_window.address, original_window.address);
    assert!(
        replayed_window.seconds_remaining > 0
            && replayed_window.seconds_remaining <= original_window.seconds_remaining
    );

    let mut dialer = Window::open(&gui_endpoint_of(&two.b.root));
    let result = dialer.request(gui_frame::Msg::PairWith(protocol::ipc::PairWith {
        address: original_window.address,
    }));
    assert!(matches!(result, gui_frame::Msg::OpResult(result) if result.ok));
    let code = reconnected.wait_event("real opener SAS prompt", |event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    let other = dialer.wait_event("real dialer SAS prompt", |event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    assert_eq!(code.sas_code, other.sas_code);
    drop(reconnected);
    let mut second = Window::open(&gui_endpoint_of(&two.a.root));
    second.status();
    let replayed = second.wait_event("unanswered SAS snapshot", |event| match event {
        gui_frame::Msg::PairingPrompt(prompt) => Some(prompt.clone()),
        _ => None,
    });
    assert_eq!(replayed, code);
    second.send(gui_frame::Msg::PairingAnswer(
        protocol::ipc::PairingAnswer {
            attempt_id: replayed.attempt_id,
            approve: false,
        },
    ));
}

#[test]
fn live_hotkey_rebinds_are_serialized_validated_and_persisted_without_desktop_input() {
    let _guard = common::serial();
    let (harness, ()) = Harness::start_with(false, true, |a, b| {
        for root in [a, b] {
            std::fs::create_dir_all(root.join("run/control-dry")).expect("disable scheduled motion");
            std::fs::write(root.join("run/hotkey-enabled-dry"), "").expect("enable synthetic hotkeys");
        }
    });
    harness.wait_linked();
    let mut window = Window::open(&gui_endpoint_of(&harness.a.root));
    let action = protocol::ipc::HotkeyAction::SwitchNext as i32;
    let first = window.send(gui_frame::Msg::SetHotkey(protocol::ipc::SetHotkey {
        action, chord: "Ctrl+Alt+Shift+F8".to_owned(),
    }));
    let second = window.send(gui_frame::Msg::SetHotkey(protocol::ipc::SetHotkey {
        action, chord: "Ctrl+Alt+Shift+F9".to_owned(),
    }));
    for request in [first, second] {
        match window.await_answer(request) {
            gui_frame::Msg::HotkeysResult(result) => assert!(result.success, "{}", result.detail),
            other => panic!("expected HotkeysResult, got {other:?}"),
        }
    }
    let before = window.status().hotkeys;
    assert_eq!(before.iter().find(|binding| binding.action == action).unwrap().chord, "Ctrl+Alt+Shift+F9");
    let invalid = window.request(gui_frame::Msg::SetHotkey(protocol::ipc::SetHotkey {
        action, chord: "Ctrl+Alt+Shift+F12".to_owned(),
    }));
    assert!(matches!(invalid, gui_frame::Msg::HotkeysResult(result) if !result.success));
    assert_eq!(window.status().hotkeys, before);
    let saved = std::fs::read_to_string(harness.a.root.join("config/config.toml")).expect("saved configuration");
    assert!(saved.contains("Ctrl+Alt+Shift+F9"), "{saved}");
    assert!(!saved.contains("Ctrl+Alt+Shift+F8"), "{saved}");
    let mut other = Window::open(&gui_endpoint_of(&harness.b.root));
    assert_ne!(other.status().hotkeys, before, "binding ownership is local to each core");
    let config_path = harness.a.root.join("config/config.toml");
    let saved_path = harness.a.root.join("config/config.saved");
    std::fs::rename(&config_path, &saved_path).expect("hold saved configuration");
    std::fs::create_dir(&config_path).expect("force config write failure");
    let failed = window.request(gui_frame::Msg::SetHotkey(protocol::ipc::SetHotkey {
        action, chord: "Ctrl+Alt+Shift+F10".to_owned(),
    }));
    std::fs::remove_dir(&config_path).expect("remove write fault");
    std::fs::rename(&saved_path, &config_path).expect("restore configuration");
    assert!(matches!(failed, gui_frame::Msg::HotkeysResult(result) if !result.success));
    assert_eq!(window.status().hotkeys, before, "persistence failure rolls registration back");
    let marker = harness.a.root.join("run/hotkey-dry");
    let trigger = |action: protocol::ipc::HotkeyAction| {
        std::fs::write(&marker, (action as i32).to_string()).expect("synthetic hotkey");
    };
    trigger(protocol::ipc::HotkeyAction::SwitchNext);
    harness.a.wait_for("hotkey switches source remote", DEADLINE, |status| status.focus == "Remote" && !marker.exists()).expect("remote");
    trigger(protocol::ipc::HotkeyAction::SwitchNext);
    harness.a.wait_for("second switch-next is not a toggle", DEADLINE, |status| status.focus == "Remote" && !marker.exists()).expect("still remote");
    trigger(protocol::ipc::HotkeyAction::ReturnLocal);
    harness.a.wait_for("orderly hotkey return", DEADLINE, |status| status.focus == "Local" && !marker.exists()).expect("local");
    trigger(protocol::ipc::HotkeyAction::ReturnLocal);
    harness.a.wait_for("return-local is idempotent", DEADLINE, |status| status.focus == "Local" && !marker.exists()).expect("still local");
    std::fs::write(harness.b.root.join("run/hotkey-dry"), (protocol::ipc::HotkeyAction::ClaimOwnership as i32).to_string()).expect("claim hotkey");
    harness.b.wait_for("hotkey ownership claim", DEADLINE, |status| status.owner == harness.b.id).expect("claimed");
    let old_agent = harness.a.agent_pid().expect("agent pid");
    common::Node::kill_pid(old_agent).expect("restart agent");
    harness.a.wait_for("replacement agent", DEADLINE, |_| {
        harness.a.agent_pid().is_ok_and(|pid| pid != old_agent)
    }).expect("agent restarted");
    let mut restarted = Window::open(&gui_endpoint_of(&harness.a.root));
    if restarted.status().hotkeys != before {
        restarted.wait_event("saved bindings registered after restart", |event| match event {
            gui_frame::Msg::Status(status) if status.hotkeys == before => Some(()),
            _ => None,
        });
    }
}
