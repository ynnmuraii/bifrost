//! The core event loop: one `select!` over link events, agent frames, timers and the
//! control channel, with the sans-IO [`fr::FocusRouter`] in the middle.
//!
//! `docs/03-architecture.md` («Процессы и границы ответственности»): core owns TLS,
//! the protocol, the router, the focus and ownership FSMs, the link manager, the
//! layout coordinator and agent supervision, and it «не исполняется в input hook
//! callback». Everything the FSM decides comes back as [`fr::Output`] and is
//! translated by [`crate::routing`]; this module only performs the side effects and
//! owns the clock.
//!
//! # The clock
//!
//! `focus_router` is sans-IO and every deadline it returns is on the caller's
//! millisecond scale, so the loop keeps one monotonic origin ([`std::time::Instant`])
//! and passes `elapsed().as_millis()` as `now_ms` everywhere. Timers are armed from
//! the same scale, which is what makes `ArmDeadline` and the lease watchdog line up
//! with the agent's own monotonic clock.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use focus_router as fr;
use geometry::NodeId;
use input_model::InputEvent;
use protocol::ipc::agent_frame::Msg;
use tokio::sync::{mpsc, oneshot};
use transport::{LinkEvent, LinkManager};

use crate::agent::Supervisor;
use crate::command::PeerRef;
use crate::ctl;
use crate::ownership_store::OwnershipStore;
use crate::routing::{self, AgentSignal, Effect};

/// The GUI's half of the session: `GuiFrame` requests and the events pushed to the window.
mod gui;
/// The pairing window and its ceremonies (ALPN `pair/1` on the link listener).
mod pairing;

/// How long the loop sleeps when nothing else has a deadline.
///
/// It bounds how long a dead agent child can go unnoticed; the IPC heartbeat and the
/// FSM timers are all shorter than this in the default configuration.
const IDLE_TICK: Duration = Duration::from_millis(50);
const RESTART_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(8);
const RESTART_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const RESTART_CIRCUIT_FAILURES: usize = 3;
fn record_restart_failure(failures: &mut VecDeque<Instant>, now: Instant) -> bool {
    while failures
        .front()
        .is_some_and(|failed| now.duration_since(*failed) > RESTART_FAILURE_WINDOW)
    {
        failures.pop_front();
    }
    failures.push_back(now);
    failures.len() >= RESTART_CIRCUIT_FAILURES
}
fn next_restart_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_RESTART_BACKOFF)
}

/// How long a `layout edit`/`layout apply` waits for the coordinator's verdict before its
/// connection is told the verdict has not arrived (`PENDING_VERDICT`).
///
/// The link round trip is milliseconds; two seconds is long enough that only a real partition
/// or a stopped coordinator can exhaust it, and short enough that the CLI (`ctl`) and the GUI
/// do not look hung.
const LAYOUT_VERDICT_TIMEOUT: Duration = Duration::from_secs(2);

fn is_agent_ready(event: &fr::AgentEvent) -> bool {
    matches!(event, fr::AgentEvent::AgentReady)
}

/// Everything one `lws-core` process owns while it runs.
pub struct Session {
    router: crate::ids::Router,
    manager: Arc<LinkManager>,
    link_events: mpsc::Receiver<LinkEvent>,
    agent: Supervisor,
    ctl: ctl::Server,
    timers: Timers,
    context: routing::AgentContext,
    started: Instant,
    ping_nonce: u64,
    node_id: NodeId,
    local_hello: protocol::wire::Hello,
    peer_displays: BTreeMap<NodeId, (u64, Vec<protocol::wire::Display>)>,
    backend: String,
    trust: identity::TrustStore,
    /// Persists the ownership pair across restarts (`config.toml`).
    ownership_store: OwnershipStore,
    /// The layout document's revision discipline (KVM-072): CAS, floor, pending.
    layout_state: crate::layout::Layout,
    /// Persists the layout pair into `[topology]`.
    layout_store: crate::layout_store::LayoutStore,
    /// The document of the *applied* revision: what every send path publishes (KVM-072).
    ///
    /// One field, one source: `publish`, the link-up replication, the `TopologyRequest` reply
    /// and the rejoin all send this, so they cannot disagree about what the revision contains.
    /// The live manifests are folded in when it is built (see [`crate::topology::wire_from_live`]).
    applied_wire: Option<protocol::wire::Topology>,
    /// Set when a higher stored revision is known but its author is unreachable: layout
    /// editing is disabled until that resolves (`docs/05-protocol.md`: «Координатор
    /// недостижим: редактирование layout отключено»).
    layout_editing_disabled: Option<String>,
    /// The last refusal this node saw or sent, as `(code, detail)`, reported by `layout show`
    /// until a revision moves (KVM-072).
    last_layout_error: Option<(String, String)>,
    /// The `layout edit`/`layout apply` requests the coordinator has not answered yet, keyed by
    /// the request id the frame carries (E3.5).
    ///
    /// The key is this node's own nonce and not the peer: the answer travels back with that id,
    /// so two concurrent requests from one node cannot take each other's verdict, and a verdict
    /// for a request whose connection already gave up is recognised as nobody's.
    layout_requests: HashMap<u64, LayoutRequest>,
    /// The next `LayoutEdit.request_nonce`; 0 is reserved for "no request".
    next_layout_nonce: u64,
    /// Set by `quit` on the control channel.
    stopping: bool,
    /// The agent reported `AgentLost` for the current crash and is being restarted.
    agent_restart_due: Option<Instant>,
    /// Recent automatic restart failures, used by the circuit breaker.
    agent_restart_failures: VecDeque<Instant>,
    /// Exponential delay after unsuccessful restart attempts.
    agent_restart_backoff: Duration,
    /// Automatic restarts stop here until the user explicitly retries.
    agent_restart_circuit_open: bool,
    /// First typed portal/backend error, preserved across automatic restarts.
    agent_degraded_reason: String,
    /// Once a portal fails, automatic launches must not open another portal session.
    portal_failure: bool,
    /// Whether core told the FSM about the current agent loss.
    agent_lost_reported: bool,
    /// The last agent `AgentError`/portal hint, shown by `status`
    /// (`routing::agent_note`). Cleared when a fresh `AgentReady` arrives.
    refusal: Option<String>,
    /// The assembled layout document. `None` means edge-triggered transitions are
    /// disabled: a document that breaks «Правила целостности» is never half-applied.
    topology: Option<geometry::Topology>,
    /// `[topology]` from `config.toml`: the links and the display snapshot the document
    /// is assembled from.
    layout: config::TopologyConfig,
    /// `input.pointer_gain`, applied to captured deltas while a source lease is remote.
    pointer_gain: f64,
    /// The agent's last display report; it replaces the snapshot for this node.
    agent_displays: Vec<protocol::ipc::DisplayInfo>,
    agent_displays_received: bool,
    /// The last captured local position: the input of an edge crossing in `Local`.
    local_pointer: Option<geometry::Position>,
    /// While a source lease is remote: the virtual position on the peer's display.
    remote_pointer: Option<geometry::Position>,
    /// The node that virtual position lives on (the lease peer, or the peer of a chain).
    remote_node: Option<NodeId>,
    /// A link the pointer left through *while remote*; applied once the lease is
    /// released, which is how a chain A→B→C is walked as two leases and no relay.
    pending_target: Option<(NodeId, geometry::Position)>,
    /// Whether the agent was asked to observe the local pointer (`BeginCapture(OBSERVE)`).
    ///
    /// The FSM only asks for capture when a transition starts, but an edge has to be
    /// *noticed* before there is any transition: «OBSERVE: Local: watch edges, no
    /// suppression», so core arms the observation itself for the node that owns input.
    observe_armed: bool,
    /// The clipboard endpoint, when `[clipboard].enabled`: the policy, the offers and the
    /// handle to the runtime that listens on `47902` (ADR-008). `None` keeps every clipboard
    /// path closed, which is the default.
    clipboard: Option<crate::clipboard::Clipboard>,
    /// Events from the clipboard runtime: a bound channel, a message, or a lost channel.
    clipboard_events: mpsc::Receiver<crate::clipboard::Inbound>,
    /// The GUI's endpoint and the fan-out of pushed events.
    gui: crate::gui::Server,
    /// Requests from the connected GUIs, one per turn of the loop.
    gui_requests: mpsc::Receiver<crate::gui::Request>,
    /// Keeps `gui_requests` alive when no GUI is connected, so `recv` never returns `None`
    /// and the loop never spins on a closed channel.
    gui_requests_tx: mpsc::Sender<crate::gui::Request>,
    /// Where every pushed event goes ([`crate::gui::Server::publish`]).
    gui_events: tokio::sync::broadcast::Sender<crate::gui::Outgoing>,
    /// The last `Status` published, so an unchanged state is not re-sent every 50 ms.
    gui_status: Option<protocol::ipc::Status>,
    /// The last topology document sent to the GUI, including same-revision display changes.
    gui_topology: Option<protocol::ipc::TopologyDoc>,
    /// This node's identity: the pairing ceremony needs the key and the certificate.
    identity: Arc<identity::NodeIdentity>,
    /// The resolved directories: where the trust record a committed pairing is written.
    paths: config::Paths,
    /// The pairing window the link listener serves by ALPN (docs/05-protocol.md).
    pairing: pairing::Pairing,
    /// Reports from the ceremony tasks.
    pairing_events: mpsc::Receiver<pairing::Event>,
    /// A second handle on the same channel, so a spawned ceremony can report back.
    pairing_events_tx: mpsc::Sender<pairing::Event>,
    /// Where this node listens: the address a pairing window shows, and the port it
    /// advertises in `PairHello.listen_port`.
    listen: std::net::SocketAddr,
    /// `node.display_name`: what a pairing peer's prompt and `TRUSTED_PEER` record show for
    /// this node. A label, never an identity.
    display_name: String,
    /// `input.emergency_hotkey`, shown by `Status` and the settings screen.
    emergency_hotkey: String,
    /// This node's declared capabilities: what `Status.capabilities_unavailable` subtracts.
    capabilities: platform_probe::Capabilities,
    /// The last agent portal progress report (`PortalPermissionRequired` or restoring).
    portal_state: String,
    /// The receiver's active keyboard layout while a lease is remote (`ActiveLayout`).
    active_layout: String,
    /// The last pairing refusal, for `Status.last_error_code`.
    pairing_error: Option<String>,
}

impl Session {
    /// Builds the loop from its parts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: crate::ids::Router,
        manager: Arc<LinkManager>,
        link_events: mpsc::Receiver<LinkEvent>,
        agent: Supervisor,
        ctl: ctl::Server,
        node_id: NodeId,
        local_hello: protocol::wire::Hello,
        backend: String,
        trust: identity::TrustStore,
        layout: config::TopologyConfig,
        pointer_gain: f64,
        ownership_store: OwnershipStore,
        layout_store: crate::layout_store::LayoutStore,
        clipboard: Option<crate::clipboard::Clipboard>,
        clipboard_events: mpsc::Receiver<crate::clipboard::Inbound>,
        gui_server: crate::gui::Server,
        identity: Arc<identity::NodeIdentity>,
        paths: config::Paths,
        listen: std::net::SocketAddr,
        display_name: String,
        emergency_hotkey: String,
        capabilities: platform_probe::Capabilities,
        pairing_gate: transport::PairingGate,
    ) -> Self {
        let layout_state = layout_store.restored();
        let (gui_requests_tx, gui_requests) = mpsc::channel(64);
        let (pairing_events_tx, pairing_events) = mpsc::channel(16);
        let gui_events = gui_server.publisher();
        let mut session = Session {
            router,
            manager,
            link_events,
            agent,
            ctl,
            timers: Timers::default(),
            refusal: None,
            context: routing::AgentContext::default(),
            started: Instant::now(),
            ping_nonce: 0,
            node_id,
            backend,
            trust,
            ownership_store,
            layout_state,
            layout_store,
            applied_wire: None,
            layout_editing_disabled: None,
            last_layout_error: None,
            layout_requests: HashMap::new(),
            next_layout_nonce: 0,
            stopping: false,
            agent_restart_due: None,
            agent_restart_failures: VecDeque::new(),
            agent_restart_backoff: RESTART_BACKOFF,
            agent_restart_circuit_open: false,
            agent_degraded_reason: String::new(),
            portal_failure: false,
            agent_lost_reported: false,
            topology: None,
            layout,
            pointer_gain,
            agent_displays: Vec::new(),
            agent_displays_received: false,
            local_hello,
            peer_displays: BTreeMap::new(),
            local_pointer: None,
            remote_pointer: None,
            remote_node: None,
            pending_target: None,
            observe_armed: false,
            clipboard,
            clipboard_events,
            gui: gui_server,
            gui_requests,
            gui_requests_tx,
            gui_events,
            gui_status: None,
            gui_topology: None,
            identity,
            paths,
            pairing: pairing::Pairing::new(pairing_gate),
            pairing_events,
            pairing_events_tx,
            listen,
            display_name,
            emergency_hotkey,
            capabilities,
            portal_state: String::new(),
            active_layout: String::new(),
            pairing_error: None,
        };
        session.rebuild_topology();
        session
    }

    /// Assembles the layout from local and peer manifests, retaining configured display
    /// snapshots until a member's live report arrives.
    fn rebuild_topology(&mut self) {
        let snapshot = crate::topology::snapshot_displays(&self.layout, self.node_id.as_bytes());
        let displays = if self.agent_displays_received {
            self.agent_displays.clone()
        } else {
            snapshot
        };
        let mut peers: BTreeMap<_, _> =
            crate::topology::snapshot_peers(&self.layout, self.node_id.as_bytes())
                .into_iter()
                .map(|(node, displays)| (node, (0, displays)))
                .collect();
        peers.extend(
            self.peer_displays
                .iter()
                .map(|(node, (revision, displays))| (*node, (*revision, displays.clone()))),
        );
        let peers: Vec<_> = peers
            .into_iter()
            .map(|(node, (_, displays))| (node, displays))
            .collect();
        let documents = crate::topology::Documents {
            local_node_id: *self.node_id.as_bytes(),
            topology: &self.layout,
            local_displays: &displays,
            peer_displays: &peers,
        };
        match crate::topology::build(&documents) {
            Ok(document) => {
                self.router.set_topology_revision(document.revision);
                tracing::info!(
                    revision = document.revision,
                    links = document.links.len(),
                    nodes = document.nodes.len(),
                    "layout document assembled"
                );
                if self.applied_wire.is_some() {
                    let applied = self.layout_state.applied();
                    self.applied_wire = Some(crate::topology::wire_from_live(
                        &document,
                        &self.layout,
                        applied.revision,
                        applied.author.unwrap_or(self.node_id),
                        self.node_id,
                    ));
                }
                self.topology = Some(document);
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "layout document unusable; edge-triggered transitions are disabled"
                );
                self.topology = None;
            }
        }
    }

    /// The monotonic millisecond clock shared with the FSM.
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Runs until `quit`, a signal, or a fatal link error.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let outputs = self.router.on_start(self.now_ms());
        self.apply(outputs).await;
        tracing::info!(
            node = crate::command::node_id_hex(&self.node_id).as_str(),
            owner = self.router.is_owner(),
            "core running"
        );

        loop {
            self.service_clock().await;
            if self.stopping {
                self.shutdown_agent().await;
                return Ok(());
            }

            let now = self.now_ms();
            let deadline = self.instant_of(self.next_deadline(now));
            let attached = self.agent.is_attached();
            // The clipboard runtime owns the sender of `clipboard_events`; with the clipboard
            // off there is no runtime, and the arm must not fire on a closed channel.
            let clipboard_enabled = self.clipboard.is_some();
            let step = {
                tokio::select! {
                    event = self.link_events.recv() => Step::Link(event),
                    frame = self.agent.recv(), if attached => Step::Agent(frame),
                    event = self.clipboard_events.recv(), if clipboard_enabled => Step::Clipboard(event),
                    client = self.ctl.accept() => Step::Control(client),
                    client = self.gui.accept() => Step::GuiClient(client),
                    request = self.gui_requests.recv() => Step::Gui(request),
                    event = self.pairing_events.recv() => Step::Pairing(event),
                    () = tokio::time::sleep_until(deadline) => Step::Tick,
                }
            };

            match step {
                Step::Link(None) => {
                    tracing::warn!("the link manager stopped");
                    return Ok(());
                }
                Step::Link(Some(event)) => self.on_link_event(event).await,
                Step::Agent(Ok(frame)) => self.on_agent_frame(frame).await?,
                Step::Agent(Err(error)) => {
                    tracing::warn!(%error, "agent channel ended");
                    self.on_agent_lost().await;
                }
                Step::Control(Ok(stream)) => self.on_control(stream).await,
                Step::Control(Err(error)) => {
                    tracing::warn!(%error, "control channel error");
                }
                Step::GuiClient(Ok(stream)) => {
                    // The connection is served by its own task: core answers requests as they
                    // arrive through `gui_requests`, and events reach the window on the same
                    // `seq`-stamped link.
                    self.gui.spawn(stream, self.gui_requests_tx.clone());
                }
                Step::GuiClient(Err(error)) => {
                    tracing::warn!(%error, "GUI channel error");
                }
                Step::Gui(Some(request)) => self.on_gui_request(request).await,
                Step::Gui(None) => {
                    // Unreachable while `gui_requests_tx` lives here; if it ever closes, the
                    // loop must not spin on it.
                    tracing::warn!("the GUI request channel stopped");
                }
                Step::Pairing(Some(event)) => self.on_pairing_event(event).await,
                Step::Pairing(None) => {
                    tracing::warn!("the pairing channel stopped");
                }
                Step::Clipboard(Some(event)) => self.on_clipboard(event).await,
                Step::Clipboard(None) => {
                    // The runtime never closes its channel while core runs; if it did, every
                    // clipboard feature would silently stop, so say so.
                    tracing::warn!("the clipboard channel stopped");
                }
                Step::Tick => {
                    self.arm_observation().await;
                    self.clipboard_on_focus().await;
                    self.expire_pairing_window();
                }
            }
            // The liveness decision comes *after* the turn's work: whatever the agent
            // sent during this turn was processed by the step above, and a loop that
            // woke late (a starved worker on a loaded machine) must not condemn an
            // agent whose answer is already sitting in the link's buffer.
            if self.agent.is_attached()
                && (self.agent.child_exited() || self.agent.silence_exceeded())
            {
                tracing::warn!(
                    silence_ms = self.agent.silence_ms(),
                    "the agent stopped answering"
                );
                self.on_agent_lost().await;
            }
            // Once per turn, and only what changed: the GUI's `Status` and, on a new revision,
            // the document its editor works on.
            self.publish_gui_state().await;
        }
    }

    /// Stops the agent before this core exits, releasing capture and forwarded input.
    pub(crate) async fn shutdown_agent(&mut self) {
        self.stopping = true;
        self.agent.shutdown().await;
    }

    /// Timers, the IPC heartbeat, and the agent restart backoff.
    async fn service_clock(&mut self) {
        let now = self.now_ms();

        for timer in self.timers.due(now) {
            tracing::debug!(?timer, "timer expired");
            self.feed(fr::Input::TimerExpired(timer)).await;
        }

        if self.agent.is_attached() {
            // Rule 11: `Ping` every `core_silence_ms / 4`, on its own clock.
            if self.agent.heartbeat_due() {
                self.ping_nonce = self.ping_nonce.wrapping_add(1);
                let ping = Msg::Ping(protocol::ipc::Ping {
                    nonce: self.ping_nonce,
                });
                if let Err(error) = self.agent.send(ping).await {
                    tracing::warn!(%error, "cannot ping the agent");
                    self.on_agent_lost().await;
                } else {
                    self.agent.on_heartbeat();
                }
            }
        } else if let Some(due) = self.agent_restart_due {
            if Instant::now() >= due {
                self.agent_restart_due = None;
                self.restart_agent().await;
            }
        }
    }

    /// When the loop must wake next, in `now_ms` millis.
    fn next_deadline(&self, now_ms: u64) -> u64 {
        let mut deadline = now_ms.saturating_add(IDLE_TICK.as_millis() as u64);
        if let Some((_, timer_deadline)) = self.timers.next() {
            deadline = deadline.min(timer_deadline);
        }
        if self.agent.is_attached() {
            // Rule 11's ping period, and the moment rule 6's limit (or the launch
            // timeout of a starting agent) makes the next liveness decision due.
            let heartbeat = now_ms.saturating_add(self.agent.next_heartbeat_in_ms());
            deadline = deadline.min(heartbeat);
            let silence = now_ms.saturating_add(self.agent.next_silence_check_in_ms());
            deadline = deadline.min(silence);
        }
        if let Some(due) = self.agent_restart_due {
            let millis = due.saturating_duration_since(Instant::now()).as_millis() as u64;
            deadline = deadline.min(now_ms.saturating_add(millis));
        }
        deadline
    }

    /// An `Instant` for a `now_ms`-scale deadline.
    fn instant_of(&self, deadline_ms: u64) -> tokio::time::Instant {
        tokio::time::Instant::from_std(self.started + Duration::from_millis(deadline_ms))
    }

    /// Feeds the FSM and performs what it decided.
    async fn feed(&mut self, input: fr::Input) {
        let now = self.now_ms();
        let outputs = self.router.handle(now, input);
        // Boxed: `apply` reaches `feed` again through the agent-loss path.
        Box::pin(self.apply(outputs)).await;
    }

    /// Performs the side effects of one decision batch, in order.
    async fn apply(&mut self, outputs: Vec<fr::Output>) {
        for effect in routing::effects(&outputs) {
            match effect {
                Effect::Send { peer, message } => self.send_to(peer, message).await,
                Effect::Broadcast { message } => {
                    for peer in self.manager.members().await {
                        self.send_to(peer, message.clone()).await;
                    }
                }
                Effect::Agent(command) => {
                    self.context.on_command(&command);
                    let frame = routing::agent_command(&command);
                    tracing::info!(frame = routing::agent_frame_name(&frame), "core -> agent");
                    if let Err(error) = self.agent.send(frame).await {
                        tracing::warn!(%error, "cannot reach the agent");
                        self.on_agent_lost().await;
                    }
                }
                Effect::Arm { timer, deadline_ms } => {
                    self.timers.arm(timer, deadline_ms);
                    // Rule 4: the agent enforces the receiver's lease deadline on its own
                    // clock, and its `ArmDeadline` from `EnterCommit` is a one-shot. Every
                    // re-arm here — each valid `LeaseRenew` moves it — has to travel, or
                    // the agent's deadline fires `lease_ttl_ms` after the commit while
                    // renewals keep arriving.
                    let rearm = routing::deadline_rearm_ms(timer, deadline_ms, self.now_ms())
                        .and_then(|remaining_ms| {
                            self.router
                                .lease()
                                .receiver()
                                .map(|lease| (lease.lease_id, remaining_ms))
                        });
                    if let Some((lease_id, remaining_ms)) = rearm {
                        self.send_agent_deadline(lease_id, remaining_ms).await;
                    }
                }
                Effect::Cancel { timer } => {
                    self.timers.cancel(timer);
                }
                Effect::CloseLink { peer, code } => {
                    tracing::warn!(peer = %peer, code = ?code, "closing a link");
                    if let Some(handle) = self.manager.handle(&peer).await {
                        handle.close();
                    }
                }
                Effect::Notice(notice) => {
                    if let fr::Notice::RepeatGuardElapsed { lease_id } = notice {
                        // AC-14 and the IPC table of `docs/03-architecture.md`: this is the
                        // translation step, the agent owns the mechanism (software engine off
                        // or the held keys up).
                        self.send_agent_stop_repeat(lease_id).await;
                    }
                    self.log_notice(&notice)
                }
            }
        }
        // Ownership is a persisted counter, not only FSM state: a restart that forgot the
        // pair would claim serial 1 again and look stale to the survivor (`ownership_store`).
        self.ownership_store.observe(self.router.ownership());
    }

    /// Arms (or re-arms) the agent's own lease deadline.
    ///
    /// Rule 4 puts the deadline on the agent's monotonic clock and bounds the frame at
    /// `1..=1000` ms; the core owns the deadline itself, so this is the translation step of
    /// the IPC mapping table, not a second source of truth.
    async fn send_agent_deadline(&mut self, lease_id: fr::LeaseId, remaining_ms: u32) {
        let frame = Msg::ArmDeadline(protocol::ipc::ArmDeadline {
            lease_id: lease_id.to_vec(),
            remaining_ms,
        });
        tracing::info!(frame = routing::agent_frame_name(&frame), "core -> agent");
        if let Err(error) = self.agent.send(frame).await {
            tracing::warn!(%error, "cannot reach the agent");
            self.on_agent_lost().await;
        }
    }
    async fn query_agent_displays(&mut self) -> bool {
        let frame = Msg::QueryDisplays(protocol::ipc::QueryDisplays {});
        tracing::info!(frame = routing::agent_frame_name(&frame), "core -> agent");
        match self.agent.send(frame).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "cannot query the agent's displays");
                self.on_agent_lost().await;
                false
            }
        }
    }

    /// `StopRepeat`: the receiver must stop repeating (`repeat_guard_ms` of silence).
    async fn send_agent_stop_repeat(&mut self, lease_id: fr::LeaseId) {
        let frame = Msg::StopRepeat(protocol::ipc::StopRepeat {
            lease_id: lease_id.to_vec(),
        });
        tracing::warn!(
            frame = routing::agent_frame_name(&frame),
            "repeat guard elapsed; telling the agent to stop repeating"
        );
        if let Err(error) = self.agent.send(frame).await {
            tracing::warn!(%error, "cannot reach the agent");
            self.on_agent_lost().await;
        }
    }

    async fn send_to(&mut self, peer: NodeId, message: fr::Message) {
        let name = routing::message_name(&message);
        match self.manager.handle(&peer).await {
            Some(handle) => {
                tracing::info!(peer = %peer, message = name, "core -> peer");
                if let Err(error) = handle.send(message).await {
                    tracing::warn!(peer = %peer, message = name, %error, "send failed");
                }
            }
            None => {
                tracing::warn!(peer = %peer, message = name, "no link to the peer");
            }
        }
    }

    /// The document of the applied revision, rebuilt from live manifests when needed.
    ///
    /// The nodes always come from the live document (`docs/05-protocol.md`: a member's
    /// displays are what its agent reports, not what the configuration snapshot says), so a
    /// node whose configuration has no `[[topology.displays]]` rows still publishes a usable
    /// document.
    fn applied_document(&self) -> protocol::wire::Topology {
        let applied = self.layout_state.applied();
        let mut document = match &self.applied_wire {
            Some(document) => document.clone(),
            None => match &self.topology {
                Some(live) => crate::topology::wire_from_live(
                    live,
                    &self.layout,
                    applied.revision,
                    applied.author.unwrap_or(self.node_id),
                    self.node_id,
                ),
                None => crate::topology::wire_document(
                    &self.layout,
                    applied.revision,
                    applied.author.unwrap_or(self.node_id),
                    self.node_id,
                ),
            },
        };
        document.revision = applied.revision;
        if let Some(author) = applied.author {
            document.author_node_id = author.as_bytes().to_vec();
        }
        document
    }

    /// One layout-document message from a peer (KVM-072).
    ///
    /// `docs/03-architecture.md`, «Владение вводом и координатор»: the document is published
    /// only by the coordinator and accepted only from its author; a member asks for it with
    /// `TopologyRequest` or sends a whole-document `LayoutEdit`, which only the coordinator
    /// applies.
    async fn on_layout(&mut self, from: NodeId, message: transport::convert::LayoutMessage) {
        use transport::convert::LayoutMessage;

        match message {
            LayoutMessage::DisplaysChanged { revision, displays } => {
                if self
                    .peer_displays
                    .get(&from)
                    .is_some_and(|(known, _)| *known >= revision)
                {
                    return;
                }
                tracing::info!(
                    peer = %from,
                    displays = displays.len(),
                    revision,
                    "peer display report"
                );
                self.peer_displays.insert(from, (revision, displays));
                self.rebuild_topology();
            }
            LayoutMessage::Topology(document) => {
                let Some(pair) = crate::topology::wire_pair(&document) else {
                    tracing::warn!(peer = %from, "topology without a readable author id");
                    return;
                };
                let validation = crate::topology::validate_wire(&document);
                let outcome = self
                    .layout_state
                    .accept_topology(from, pair, || validation.clone());
                tracing::info!(
                    peer = %from,
                    revision = pair.revision,
                    outcome = ?outcome,
                    "topology received"
                );
                match outcome {
                    crate::layout::TopologyOutcome::Applied(pair) => {
                        self.layout_editing_disabled = None;
                        self.last_layout_error = None;
                        self.applied_wire = Some(match &self.topology {
                            // Fold the live manifests in: the revision is the document the
                            // author published, its node list is what the agents report.
                            Some(live) => crate::topology::wire_from_live(
                                live,
                                &self.layout,
                                document.revision,
                                document
                                    .author_node_id
                                    .as_slice()
                                    .try_into()
                                    .map(NodeId::new)
                                    .unwrap_or(self.node_id),
                                self.node_id,
                            ),
                            None => document.clone(),
                        });
                        self.layout_store.observe(&self.layout_state);
                        self.router.set_topology_revision(pair.revision);
                        // The coordinator asked for it (rejoin) or published it; either way the
                        // answer is the confirmation `docs/05-protocol.md` names.
                        self.send_layout_to(
                            from,
                            transport::convert::LayoutMessage::TopologyApplied {
                                revision: pair.revision,
                                // A document push, not a request: nothing to correlate.
                                request_nonce: 0,
                            },
                        )
                        .await;
                        if self.router.ownership().is_owner() {
                            // The coordinator takes the winning revision and republishes it
                            // under itself as `max_seen + 1`, keeping its own edit aside.
                            self.republish_layout(document).await;
                        }
                    }
                    crate::layout::TopologyOutcome::NotNewer(current) => {
                        self.send_layout_to(
                            from,
                            transport::convert::LayoutMessage::TopologyApplied {
                                revision: current.revision,
                                request_nonce: 0,
                            },
                        )
                        .await;
                    }
                    crate::layout::TopologyOutcome::Invalid(detail) => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::InvalidTopology,
                            &detail,
                            // A document push is not a request: there is no id or revision to
                            // name.
                            0,
                            0,
                        )
                        .await;
                    }
                    crate::layout::TopologyOutcome::NotAuthor => {
                        tracing::warn!(peer = %from, "a topology was relayed by someone who is not its author");
                    }
                }
            }
            LayoutMessage::LayoutEdit(edit) => {
                // Only the coordinator applies, and it answers the editor either way.
                if !self.router.ownership().is_owner() {
                    self.send_layout_error(
                        from,
                        protocol::wire::ErrorCode::NoCoordinator,
                        "this node is not the coordinator",
                        edit.request_nonce,
                        0,
                    )
                    .await;
                    return;
                }
                let base = match <[u8; 32]>::try_from(edit.expected_author.as_slice()) {
                    Ok(bytes) => {
                        crate::layout::Pair::new(edit.expected_revision, NodeId::new(bytes))
                    }
                    Err(_) => crate::layout::Pair::NONE,
                };
                let editor = NodeId::new(
                    edit.author_node_id
                        .clone()
                        .try_into()
                        .unwrap_or(*from.as_bytes()),
                );
                let layout_toml = String::from_utf8_lossy(&edit.layout_toml).into_owned();
                let outcome = self.layout_state.apply_edit(
                    &crate::layout::Edit {
                        expected: base,
                        editor,
                        layout_toml: layout_toml.clone(),
                    },
                    self.node_id,
                    true,
                    validate_layout_toml,
                );
                match outcome {
                    crate::layout::EditOutcome::Applied(pair) => {
                        self.layout_editing_disabled = None;
                        self.last_layout_error = None;
                        self.layout_store.observe(&self.layout_state);
                        self.router.set_topology_revision(pair.revision);
                        tracing::info!(peer = %from, revision = pair.revision, "layout edit applied");
                        // The editor asked by id, so it gets the verdict by id: a successful
                        // edit is otherwise only visible as a document change, and two
                        // requests from one node must not take each other's answer.
                        self.send_layout_to(
                            from,
                            transport::convert::LayoutMessage::TopologyApplied {
                                revision: pair.revision,
                                request_nonce: edit.request_nonce,
                            },
                        )
                        .await;
                        let document = match toml::from_str::<config::TopologyConfig>(&layout_toml)
                        {
                            Ok(parsed) => match &self.topology {
                                Some(live) => crate::topology::wire_from_live(
                                    live,
                                    &parsed,
                                    pair.revision,
                                    self.node_id,
                                    self.node_id,
                                ),
                                None => crate::topology::wire_document(
                                    &parsed,
                                    pair.revision,
                                    self.node_id,
                                    self.node_id,
                                ),
                            },
                            Err(error) => {
                                tracing::warn!(%error, "applied layout cannot be republished");
                                return;
                            }
                        };
                        self.publish_layout(document).await;
                    }
                    crate::layout::EditOutcome::Conflict(current) => {
                        // The editor cannot reload what it cannot name: the refusal carries the
                        // revision that is applied now (`docs/05-protocol.md`, the error table:
                        // «отклонить, показать текущую ревизию и перезагрузить документ»).
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::LayoutConflict,
                            &format!("expected revision {} is applied", current.revision),
                            edit.request_nonce,
                            current.revision,
                        )
                        .await;
                    }
                    crate::layout::EditOutcome::Invalid(detail) => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::InvalidTopology,
                            &detail,
                            edit.request_nonce,
                            0,
                        )
                        .await;
                    }
                    crate::layout::EditOutcome::NotCoordinator => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::NoCoordinator,
                            "this node is not the coordinator",
                            edit.request_nonce,
                            0,
                        )
                        .await;
                    }
                }
            }
            LayoutMessage::TopologyRequest => {
                // «Принимается она только от своего автора»: only the author answers, and here
                // that is this node when it published what is applied.
                if self.layout_state.applied().author != Some(self.node_id) {
                    tracing::info!(peer = %from, "topology requested from a node that is not its author");
                    return;
                }
                let document = self.applied_document();
                self.send_layout_to(from, transport::convert::LayoutMessage::Topology(document))
                    .await;
            }
            LayoutMessage::Error {
                code,
                request_nonce,
                layout_revision,
            } => {
                // The editor's answer: `layout show` reports it until a revision moves.
                let detail = refusal_detail(code, layout_revision);
                self.last_layout_error = Some((code.as_str_name().to_owned(), detail.clone()));
                tracing::warn!(peer = %from, code = code.as_str_name(), detail, "layout edit refused");
                self.deliver_layout_verdict(
                    request_nonce,
                    false,
                    ctl::Response::Error {
                        code: layout_refusal_code(code),
                        detail,
                    },
                );
            }
            LayoutMessage::TopologyApplied {
                revision,
                request_nonce,
            } => {
                tracing::info!(peer = %from, revision, "the peer applied the layout revision");
                // With an id this is the coordinator's verdict on an edit of ours; without one
                // it is a member confirming a document it was sent, which nobody here waits
                // for (`deliver_layout_verdict` ignores 0).
                self.deliver_layout_verdict(
                    request_nonce,
                    true,
                    ctl::Response::Status(format!("applied revision={revision}")),
                );
            }
            LayoutMessage::ActiveLayout(layout) => {
                // The receiver's keyboard layout, for the GUI («печать идёт на <ПК>, раскладка
                // <xx>»); nothing in the input path depends on it.
                tracing::info!(
                    peer = %from,
                    layout_id = layout.layout_id.as_str(),
                    revision = layout.layout_revision,
                    "receiver layout"
                );
                self.active_layout = layout.layout_id.clone();
            }
        }
    }

    /// Republishes a document the coordinator has just adopted as `max_seen + 1` under itself.
    ///
    /// A divergent local edit stays pending: it was based on the revision this document
    /// supersedes, so applying it would silently revert the other side's work.
    async fn republish_layout(&mut self, document: protocol::wire::Topology) {
        if self.layout_state.pending().is_some() {
            tracing::warn!(
                "a pending layout edit stays aside while the winning revision is republished"
            );
        }
        let published = self.layout_state.republish(self.node_id);
        self.layout_store.observe(&self.layout_state);
        self.router.set_topology_revision(published.revision);
        let mut wire = document;
        wire.revision = published.revision;
        wire.author_node_id = self.node_id.as_bytes().to_vec();
        tracing::info!(
            revision = published.revision,
            "republishing the winning layout"
        );
        self.publish_layout(wire).await;
    }

    /// Sends a document to every member (the coordinator's publish step).
    ///
    /// Anything that would not survive the receiver's revalidation is refused here instead of
    /// being sent: an invalid document only raises the peer's floor and teaches it nothing
    /// (`INVALID_TOPOLOGY` keeps the old revision on both sides).
    async fn publish_layout(&mut self, document: protocol::wire::Topology) {
        if let Err(detail) = crate::topology::validate_wire(&document) {
            tracing::warn!(
                revision = document.revision,
                nodes = document.nodes.len(),
                detail,
                "the applied layout cannot be published as it stands"
            );
            return;
        }
        tracing::info!(
            revision = document.revision,
            nodes = document.nodes.len(),
            links = document.links.len(),
            "core -> peers: Topology"
        );
        self.applied_wire = Some(document.clone());
        for peer in self.manager.members().await {
            self.send_layout_to(
                peer,
                transport::convert::LayoutMessage::Topology(document.clone()),
            )
            .await;
        }
    }

    /// One layout message to one peer; `false` when there is no usable link.
    ///
    /// The answer is what tells a follower that the coordinator is unreachable: a partition
    /// looks exactly like a send that never lands, and that is when an edit is kept as
    /// pending instead of being lost.
    async fn send_layout_to(
        &mut self,
        peer: NodeId,
        message: transport::convert::LayoutMessage,
    ) -> bool {
        match self.manager.handle(&peer).await {
            Some(handle) => match handle.send_layout(message).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(peer = %peer, %error, "cannot send a layout message");
                    false
                }
            },
            None => {
                tracing::warn!(peer = %peer, "no link for a layout message");
                false
            }
        }
    }

    /// One layout refusal to a peer: a `ProtocolError` with the code the contract names.
    ///
    /// It travels on the layout arm (`LayoutMessage::Error`), so the editor sees
    /// `LAYOUT_CONFLICT` / `INVALID_TOPOLOGY` / `NO_COORDINATOR` rather than a generic
    /// protocol error, and the focus mirror stays what it is.
    async fn send_layout_error(
        &mut self,
        peer: NodeId,
        code: protocol::wire::ErrorCode,
        detail: &str,
        request_nonce: u64,
        layout_revision: u64,
    ) {
        tracing::warn!(peer = %peer, code = code.as_str_name(), detail, "layout refused for a peer");
        // «The last refusal this node saw *or sent*»: a refusal this node emits is as much a
        // fact about the document as one it received, and `layout show` reports it either way.
        self.last_layout_error = Some((code.as_str_name().to_owned(), detail.to_owned()));
        self.send_layout_to(
            peer,
            transport::convert::LayoutMessage::Error {
                code,
                request_nonce,
                layout_revision,
            },
        )
        .await;
    }

    /// Registers one layout request and returns its id together with where its verdict goes.
    ///
    /// The id is what the coordinator echoes, so two concurrent requests cannot take each
    /// other's verdict. Requests whose connection already gave up are dropped first: their
    /// verdict has nobody to tell.
    fn hold_layout_request(
        &mut self,
        layout_toml: String,
    ) -> (u64, oneshot::Receiver<ctl::Response>) {
        self.layout_requests
            .retain(|_, request| !request.verdict.is_closed());
        // `.max(1)` keeps 0 out of the wire: it means «this frame is not a request».
        self.next_layout_nonce = self.next_layout_nonce.wrapping_add(1).max(1);
        let nonce = self.next_layout_nonce;
        let (verdict, receiver) = oneshot::channel();
        self.layout_requests.insert(
            nonce,
            LayoutRequest {
                layout_toml,
                verdict,
            },
        );
        (nonce, receiver)
    }

    /// Puts an edit in the pending slot, and the same document in `layout.pending.toml`.
    ///
    /// One fact, two places: an edit is written when it enters the slot and the file is removed
    /// when the slot clears (see [`Self::release_layout_pending`]), so «a
    /// `layout.pending.toml` is here» always means «this node holds an edit the coordinator has
    /// not applied».
    fn hold_layout_pending(&mut self, layout_toml: &str) {
        self.layout_state.hold_pending(layout_toml.to_owned());
        self.layout_store.write_pending(layout_toml);
    }

    /// Empties the pending slot and removes `layout.pending.toml`; `true` when there was one.
    fn release_layout_pending(&mut self) -> bool {
        let dropped = self.layout_state.take_pending().is_some();
        self.layout_store.clear_pending();
        dropped
    }

    /// Hands the coordinator's verdict to the connection that asked, if it is still there.
    ///
    /// `applied` is the verdict's own kind, reported by the coordinator: an *applied* verdict
    /// releases the pending slot, and only when the slot still holds the document that was
    /// applied — a newer edit that arrived while this one was in flight is not thrown away. A
    /// refusal keeps the edit (in the slot and in `layout.pending.toml`), which is what
    /// ADR-013's «перезагрузить и применить своё» needs.
    fn deliver_layout_verdict(
        &mut self,
        request_nonce: u64,
        applied: bool,
        verdict: ctl::Response,
    ) {
        if request_nonce == 0 {
            return;
        }
        let Some(request) = self.layout_requests.remove(&request_nonce) else {
            tracing::debug!(
                request_nonce,
                "a layout verdict for a request nobody waits for"
            );
            return;
        };
        if applied && self.layout_state.pending() == Some(request.layout_toml.as_str()) {
            // The edit is the document now: the slot has nothing left to keep.
            self.release_layout_pending();
        }
        if request.verdict.send(verdict).is_err() {
            tracing::debug!(
                request_nonce,
                "the layout requester is gone; the verdict is dropped"
            );
        }
    }

    /// `layout apply`: hand the pending edit to the coordinator's CAS.
    ///
    /// On the coordinator the verdict is immediate. A follower sends the edit with the base it
    /// names and answers with the coordinator's verdict, which is why the reply is a
    /// [`Handled`]: this node is not the one that decides.
    async fn layout_apply(&mut self) -> Handled {
        let (base_pair, layout_toml) = match self.layout_state.pending() {
            Some(toml) => (
                self.layout_state
                    .pending_base()
                    .unwrap_or(self.layout_state.applied()),
                toml.to_owned(),
            ),
            None => {
                return Handled::Reply(ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail: "no pending layout edit".to_owned(),
                })
            }
        };
        if !self.router.ownership().is_owner() {
            // The pending edit belongs to the coordinator's CAS: a follower forwards it with
            // the base the slot keeps and answers with the verdict that comes back. The slot
            // is released by that verdict, not by the send, so a refusal or a lost answer
            // leaves the edit to reload (ADR-013).
            let Some(owner) = self.router.ownership().owner().copied() else {
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the pending edit waits".to_owned(),
                });
            };
            let (nonce, verdict) = self.hold_layout_request(layout_toml.clone());
            // The base is the pair the edit was *written against*, never the pair this node
            // has applied now: a pending edit waits across a partition while the document
            // moves, and sending `applied` would turn the coordinator's CAS into a no-op that
            // always succeeds — silently reverting the side that won the rejoin (ADR-013).
            let message =
                transport::convert::LayoutMessage::LayoutEdit(protocol::wire::LayoutEdit {
                    expected_revision: base_pair.revision,
                    expected_author: base_pair
                        .author
                        .map_or_else(Vec::new, |id| id.as_bytes().to_vec()),
                    author_node_id: self.node_id.as_bytes().to_vec(),
                    layout_toml: layout_toml.clone().into_bytes(),
                    request_nonce: nonce,
                });
            if !self.send_layout_to(owner, message).await {
                // Nothing was asked, so nobody will answer; the pending edit stays where it is.
                self.layout_requests.remove(&nonce);
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "the coordinator is unreachable; the pending edit waits".to_owned(),
                });
            }
            return Handled::Deferred(verdict);
        }
        let edit = crate::layout::Edit {
            expected: base_pair,
            editor: self.node_id,
            layout_toml,
        };
        let outcome = self
            .layout_state
            .apply_edit(&edit, self.node_id, true, validate_layout_toml);
        Handled::Reply(match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                self.release_layout_pending();
                self.layout_store.observe(&self.layout_state);
                self.router.set_topology_revision(pair.revision);
                ctl::Response::Status(format!("applied revision={}", pair.revision))
            }
            crate::layout::EditOutcome::Conflict(current) => {
                let detail = format!(
                    "the pending edit is based on revision {} but {} is applied",
                    edit.expected.revision, current.revision
                );
                self.last_layout_error =
                    Some(("ERROR_CODE_LAYOUT_CONFLICT".to_owned(), detail.clone()));
                ctl::Response::Error {
                    code: "LAYOUT_CONFLICT".to_owned(),
                    detail,
                }
            }
            crate::layout::EditOutcome::Invalid(detail) => {
                self.last_layout_error =
                    Some(("ERROR_CODE_INVALID_TOPOLOGY".to_owned(), detail.clone()));
                ctl::Response::Error {
                    code: "INVALID_TOPOLOGY".to_owned(),
                    detail,
                }
            }
            crate::layout::EditOutcome::NotCoordinator => ctl::Response::Error {
                code: "NO_COORDINATOR".to_owned(),
                detail: "this node is not the owner".to_owned(),
            },
        })
    }

    /// `layout show`: the pair, the floor, whether an edit is waiting, and the last refusal.
    fn layout_line(&self) -> String {
        let hex = |pair: crate::layout::Pair| {
            pair.author
                .map_or_else(|| "-".to_owned(), |id| identity::fingerprint(&id))
        };
        let applied = self.layout_state.applied();
        let floor = self.layout_state.max_seen();
        let mut line = format!(
            "revision={} author={} max_seen={} max_seen_author={} pending={}",
            applied.revision,
            hex(applied),
            floor.revision,
            hex(floor),
            if self.layout_state.pending().is_some() {
                "yes"
            } else {
                "no"
            },
        );
        // The refusal is appended only when there is one, exactly as `status` does with its
        // `refusal=`: it is what the GUI shows after `LAYOUT_CONFLICT` («layout изменён на
        // другом узле»), and it stays until a revision moves.
        if let Some((code, _detail)) = &self.last_layout_error {
            line.push_str(&format!(" last_error={code}"));
        }
        line
    }

    /// `layout edit <file>`: the whole-document CAS of KVM-072.
    ///
    /// Only the coordinator applies (`docs/03-architecture.md`: «Layout правит любой узел, но
    /// применяет только координатор через CAS по `(revision, author)`»), and the base is the
    /// revision this node has applied — which is why the answer names the current one on a
    /// conflict instead of guessing what the caller meant. A follower's answer is the
    /// coordinator's own verdict: `OK applied revision=N` and `ERR LAYOUT_CONFLICT …` mean the
    /// same thing on both sides of the link.
    async fn layout_edit(&mut self, path: &str) -> Handled {
        let document_text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                return Handled::Reply(ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail: format!("cannot read {path}: {error}"),
                })
            }
        };
        let applied = self.layout_state.applied();
        self.layout_edit_document(document_text, applied).await
    }

    /// The whole-document CAS itself, for both front-ends.
    ///
    /// `expected` is the pair the editor wrote its document against: the applied revision for
    /// `layout edit <file>` (which is where the file came from), and the pair the GUI names in
    /// `UpdateTopology` — a document it may have loaded a revision ago. Whichever it is, it is
    /// what the coordinator's CAS compares, so a stale base is refused `LAYOUT_CONFLICT`
    /// instead of silently reverting the winning side (ADR-013).
    async fn layout_edit_document(
        &mut self,
        document_text: String,
        expected: crate::layout::Pair,
    ) -> Handled {
        let is_coordinator = self.router.ownership().is_owner();
        if let Some(reason) = self.layout_editing_disabled.clone() {
            self.hold_layout_pending(&document_text);
            return Handled::Reply(ctl::Response::Error {
                code: "NO_COORDINATOR".to_owned(),
                detail: format!("{reason}; the edit is kept as pending"),
            });
        }
        if !is_coordinator {
            // A follower sends the edit to the coordinator and answers with the verdict that
            // comes back; only a partition keeps it pending (`docs/05-protocol.md`).
            let Some(owner) = self.router.ownership().owner().copied() else {
                self.hold_layout_pending(&document_text);
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the edit is kept as pending".to_owned(),
                });
            };
            // The edit occupies the pending slot until the verdict says it was applied, so a
            // refusal or a lost answer leaves the edit — slot and file — for the user's reload.
            self.hold_layout_pending(&document_text);
            let (nonce, verdict) = self.hold_layout_request(document_text.clone());
            let message =
                transport::convert::LayoutMessage::LayoutEdit(protocol::wire::LayoutEdit {
                    expected_revision: expected.revision,
                    expected_author: expected
                        .author
                        .map_or_else(Vec::new, |id| id.as_bytes().to_vec()),
                    author_node_id: self.node_id.as_bytes().to_vec(),
                    layout_toml: document_text.clone().into_bytes(),
                    request_nonce: nonce,
                });
            if !self.send_layout_to(owner, message).await {
                self.layout_requests.remove(&nonce);
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "the coordinator is unreachable; the edit is kept as pending"
                        .to_owned(),
                });
            }
            return Handled::Deferred(verdict);
        }
        let edit = crate::layout::Edit {
            expected,
            editor: self.node_id,
            layout_toml: document_text,
        };
        let outcome =
            self.layout_state
                .apply_edit(&edit, self.node_id, is_coordinator, validate_layout_toml);
        Handled::Reply(match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                self.layout_store.observe(&self.layout_state);
                self.router.set_topology_revision(pair.revision);
                tracing::info!(
                    revision = pair.revision,
                    author = identity::fingerprint(&self.node_id).as_str(),
                    "layout applied by the coordinator"
                );
                // The same words a follower's verdict uses: the published document is always
                // authored by the coordinator, so naming it here adds nothing.
                ctl::Response::Status(format!("applied revision={}", pair.revision))
            }
            crate::layout::EditOutcome::Conflict(current) => {
                let detail = format!(
                    "the edit is based on revision {} but {} is applied",
                    edit.expected.revision, current.revision
                );
                self.last_layout_error =
                    Some(("ERROR_CODE_LAYOUT_CONFLICT".to_owned(), detail.clone()));
                ctl::Response::Error {
                    code: "LAYOUT_CONFLICT".to_owned(),
                    detail,
                }
            }
            crate::layout::EditOutcome::Invalid(detail) => {
                self.last_layout_error =
                    Some(("ERROR_CODE_INVALID_TOPOLOGY".to_owned(), detail.clone()));
                ctl::Response::Error {
                    code: "INVALID_TOPOLOGY".to_owned(),
                    detail,
                }
            }
            crate::layout::EditOutcome::NotCoordinator => ctl::Response::Error {
                code: "NO_COORDINATOR".to_owned(),
                detail: "this node is not the owner".to_owned(),
            },
        })
    }

    /// One line per FSM transition, and the reason when something was refused.
    fn log_notice(&mut self, notice: &fr::Notice) {
        match notice {
            fr::Notice::Focus(state) => tracing::info!(focus = state.as_str(), "focus"),
            fr::Notice::Ownership(state) => {
                tracing::info!(ownership = state.as_str(), "ownership")
            }
            fr::Notice::SwitchRefused { trigger, reason } => {
                tracing::warn!(?trigger, ?reason, "switch refused")
            }
            fr::Notice::Rejected { peer, code } => {
                tracing::warn!(peer = %peer, ?code, "peer refused")
            }
            other => tracing::info!(notice = ?other, "notice"),
        }
    }

    async fn on_link_event(&mut self, event: LinkEvent) {
        match event {
            // `pair/1` on the link listener: the listener admitted the dialer through the
            // pairing gate, and the ceremony itself runs in a task of its own so a peer that
            // never answers cannot stall this loop.
            LinkEvent::Pairing(connection) => {
                tracing::info!(
                    peer = %connection.address,
                    "pairing: an accepted dialer enters the window"
                );
                self.spawn_pair_accept(*connection);
            }
            LinkEvent::Up {
                peer,
                hello,
                connection_id,
                remote,
            } => {
                tracing::info!(
                    peer = %peer,
                    name = hello.display_name.as_str(),
                    can_own = hello.can_own,
                    receiver_ready = hello.capabilities.receiver_ready(),
                    "link up"
                );
                if hello.displays_revision > 0 {
                    self.peer_displays
                        .insert(peer, (hello.displays_revision, hello.displays.clone()));
                } else {
                    self.peer_displays.remove(&peer);
                }
                self.rebuild_topology();
                // The clipboard channel of this pair is bound to this connection id as its
                // parent: the grant carries it, the bind echoes it, and closing the input link
                // closes the channel (ADR-008, «Clipboard канал»).
                self.on_clipboard_link_up(peer, connection_id, hello.text_clipboard, remote)
                    .await;
                self.feed(fr::Input::Hello {
                    peer,
                    hello: fr::PeerHello {
                        can_own: hello.can_own,
                        capabilities: hello.capabilities,
                        owner_serial: hello.owner_serial,
                        owner: hello.owner,
                    },
                })
                .await;
                // `Hello` carries the ownership pair as it was when this node started, so
                // a peer that joins after a local claim (the normal case: the owner claims
                // at startup, the link forms later) would know no owner and refuse our
                // `EnterPrepare` with `NOT_OWNER`. The owner therefore states its pair to
                // every link that comes up, in the same shape the FSM broadcasts when the
                // claim lands.
                // KVM-072: a peer may hold a *stored* revision newer than ours (it was the
                // author, or it was in the other partition half). `docs/05-protocol.md`: the
                // coordinator asks that author for the document; if the author is
                // unreachable, layout editing is disabled rather than based on a stale
                // revision.
                let peer_pair = crate::layout::Pair {
                    revision: hello.topology_revision,
                    author: hello.topology_author,
                };
                if peer_pair.is_newer_than(self.layout_state.applied()) {
                    if !self.router.ownership().is_owner() {
                        self.layout_editing_disabled = Some(format!(
                            "a newer stored layout revision ({}) is known; editing is disabled \
                             until the coordinator resolves it",
                            peer_pair.revision
                        ));
                        tracing::warn!(
                            peer = %peer,
                            revision = peer_pair.revision,
                            "layout editing disabled: a newer stored revision is known"
                        );
                    } else if peer_pair.author == Some(peer) {
                        tracing::info!(
                            peer = %peer,
                            revision = peer_pair.revision,
                            "asking the author for the newer stored layout revision"
                        );
                        self.send_layout_to(
                            peer,
                            transport::convert::LayoutMessage::TopologyRequest,
                        )
                        .await;
                    } else {
                        self.layout_editing_disabled = Some(format!(
                            "the author of the newer stored layout revision ({}) is not the peer \
                             that advertised it",
                            peer_pair.revision
                        ));
                    }
                }
                if self.layout_state.applied().is_newer_than(peer_pair)
                    && self.layout_state.applied().author == Some(self.node_id)
                {
                    // This node published what is applied, so the peer is behind: replicate,
                    // which is how a member that was in the other partition half catches up
                    // without asking (KVM-072).
                    let applied = self.layout_state.applied();
                    let document = self.applied_document();
                    tracing::info!(
                        peer = %peer,
                        revision = applied.revision,
                        "replicating the applied layout to a rejoining peer"
                    );
                    self.send_layout_to(
                        peer,
                        transport::convert::LayoutMessage::Topology(document),
                    )
                    .await;
                }
                let ownership = self.router.ownership();
                if ownership.is_owner() {
                    self.send_to(
                        peer,
                        fr::Message::OwnerState {
                            serial: ownership.serial(),
                            owner: ownership.owner().copied(),
                            granted: true,
                        },
                    )
                    .await;
                }
            }
            LinkEvent::Message { peer, message } => match message {
                transport::convert::Inbound::Focus(message) => {
                    tracing::info!(
                        peer = %peer,
                        message = routing::message_name(&message),
                        "peer -> core"
                    );
                    self.feed(fr::Input::Net {
                        from: peer,
                        message,
                    })
                    .await;
                }
                transport::convert::Inbound::Layout(message) => {
                    self.on_layout(peer, message).await;
                }
                transport::convert::Inbound::Clipboard {
                    message,
                    connection_id,
                } => {
                    // A grant is the only clipboard message the input link carries, and it has
                    // to arrive before the channel it authorises can exist.
                    tracing::info!(peer = %peer, "clipboard grant");
                    let out = match self.clipboard.as_mut() {
                        Some(clipboard) => clipboard.on_grant(peer, connection_id, &message),
                        None => Vec::new(),
                    };
                    self.apply_clipboard(out).await;
                }
            },
            LinkEvent::Down { peer, reason } => {
                tracing::warn!(peer = %peer, ?reason, "link down");
                if let Some(clipboard) = self.clipboard.as_mut() {
                    clipboard.on_link_down(peer).await;
                }
                self.peer_displays.remove(&peer);
                self.rebuild_topology();
                self.feed(fr::Input::LinkDown { peer }).await;
            }
        }
    }

    /// `[clipboard].transfer = "on_focus_enter"`: while this node is the receiver of an active
    /// lease, ask the peer for the offer it made while focus was elsewhere.
    async fn clipboard_on_focus(&mut self) {
        let receiving = matches!(
            self.router.lease().receiver(),
            Some(lease) if lease.state == fr::ReceiverState::Active
        );
        if !receiving {
            return;
        }
        if let Some(clipboard) = self.clipboard.as_mut() {
            clipboard.on_focus().await;
        }
    }

    /// Tells the clipboard endpoint that a member's input link came up.
    async fn on_clipboard_link_up(
        &mut self,
        peer: NodeId,
        connection_id: [u8; 16],
        text_clipboard: bool,
        remote: std::net::SocketAddr,
    ) {
        let out = match self.clipboard.as_mut() {
            Some(clipboard) => {
                clipboard
                    .on_link_up(peer, connection_id, text_clipboard, remote.ip())
                    .await
            }
            None => Vec::new(),
        };
        self.apply_clipboard(out).await;
    }

    /// One event from the clipboard runtime.
    async fn on_clipboard(&mut self, event: crate::clipboard::Inbound) {
        let out = match self.clipboard.as_mut() {
            Some(clipboard) => match event {
                crate::clipboard::Inbound::Bound { peer, channel_id } => {
                    tracing::info!(peer = %peer, "clipboard channel bound");
                    clipboard.on_bound(peer, channel_id).await
                }
                crate::clipboard::Inbound::Message { peer, message } => {
                    clipboard.on_peer_message(peer, message).await
                }
                crate::clipboard::Inbound::Down { peer } => {
                    clipboard.on_channel_down(peer).await;
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        self.apply_clipboard(out).await;
    }

    /// Performs what the clipboard endpoint decided.
    async fn apply_clipboard(&mut self, out: Vec<crate::clipboard::Out>) {
        for action in out {
            match action {
                crate::clipboard::Out::Grant { peer, message } => {
                    // The grant travels on the input link: the channel it authorises does not
                    // exist until the peer has it.
                    match self.manager.handle(&peer).await {
                        Some(handle) => {
                            tracing::info!(peer = %peer, "core -> peer");
                            if let Err(error) = handle.send_clipboard(message).await {
                                tracing::warn!(peer = %peer, %error, "clipboard grant failed");
                            }
                        }
                        None => tracing::warn!(peer = %peer, "no input link for a clipboard grant"),
                    }
                }
                crate::clipboard::Out::Dial {
                    peer,
                    address,
                    parent,
                    channel_id,
                    token,
                } => {
                    if let Some(clipboard) = self.clipboard.as_ref() {
                        clipboard
                            .dial(peer, address, parent, channel_id, token)
                            .await;
                    }
                }
                crate::clipboard::Out::Agent(frame) => {
                    tracing::info!(frame = routing::agent_frame_name(&frame), "core -> agent");
                    if let Err(error) = self.agent.send(frame).await {
                        tracing::warn!(%error, "cannot reach the agent");
                        self.on_agent_lost().await;
                    }
                }
                crate::clipboard::Out::Cancel {
                    peer,
                    offer_id,
                    reason,
                } => {
                    if let Some(clipboard) = self.clipboard.as_ref() {
                        clipboard.cancel(peer, offer_id, reason).await;
                    }
                }
            }
        }
    }

    async fn on_agent_frame(&mut self, frame: protocol::ipc::AgentFrame) -> anyhow::Result<()> {
        // The frame is liveness wherever it was received: `Supervisor::recv` already
        // marked it on the heartbeat, so nothing to do here but handle it.
        let Some(message) = frame.msg else {
            return Ok(());
        };
        tracing::info!(frame = routing::agent_frame_name(&message), "agent -> core");
        if let Msg::Error(error) = &message {
            if error.code.starts_with("PORTAL_") {
                self.portal_failure = true;
                self.agent.set_portal_sessions_allowed(false);
                if self.agent_degraded_reason.is_empty() {
                    self.agent_degraded_reason = format!("{}: {}", error.code, error.detail.trim());
                }
            }
        }
        // `status` shows the last thing the agent had to report; the note is derived
        // from the frame and never contains a token (`routing::agent_note`).
        if let Some(note) = routing::agent_note(&message) {
            self.refusal = Some(note);
            // `Status.portal_state`: whether the compositor is asking for permission, the
            // user refused it, or this backend has no such portal. The GUI shows it next to
            // the node, which is the only place a Wayland limitation becomes visible
            // (`docs/03-architecture.md`: «GUI обязан сообщить это заранее, чтобы
            // ограничение не выглядело дефектом»).
            if let Some(note) = self.refusal.as_deref() {
                if note.contains("PORTAL_DENIED") {
                    self.portal_state = "denied".to_owned();
                } else if note.contains("PORTAL_UNAVAILABLE") {
                    self.portal_state = "unavailable".to_owned();
                }
            }
        }
        if let Msg::ClipboardChanged(changed) = &message {
            // The clipboard never drives the focus FSM: a copy is core's own business, so it is
            // handled here and does not reach `routing::agent_signal`.
            let out = match self.clipboard.as_mut() {
                Some(clipboard) => clipboard.on_agent_frame(changed).await,
                None => Vec::new(),
            };
            self.apply_clipboard(out).await;
            return Ok(());
        }
        match routing::agent_signal(&message, &mut self.context) {
            AgentSignal::Fsm(event) => {
                if is_agent_ready(&event) {
                    self.agent.mark_ready();
                    self.agent_lost_reported = false;
                    self.agent_restart_due = None;
                    // A new session starts with a clean slate; the previous crash's
                    // error is not this session's state.
                    if !self.portal_failure && !self.agent_restart_circuit_open {
                        self.refusal = None;
                        self.portal_state.clear();
                        self.agent_degraded_reason.clear();
                    }
                    if !self.query_agent_displays().await {
                        return Ok(());
                    }
                }
                self.feed(fr::Input::Agent(event)).await;
            }
            AgentSignal::Local(local) => self.feed(fr::Input::Local(local)).await,
            AgentSignal::Captured(events) => {
                // The tracker needs the display id, which only the wire form has; the
                // wire itself needs the normalised events.
                self.track_pointer(&events).await;
                let converted: Vec<InputEvent> =
                    events.iter().filter_map(routing::input_event_of).collect();
                self.forward_captured(converted).await;
            }
            AgentSignal::Displays(report) => {
                tracing::info!(
                    displays = report.displays.len(),
                    revision = report.local_revision,
                    "agent display report"
                );
                let displays: Vec<_> = report
                    .displays
                    .iter()
                    .filter_map(crate::topology::display_to_wire)
                    .collect();
                let revision = if report.local_revision > self.local_hello.displays_revision {
                    report.local_revision
                } else if displays != self.local_hello.displays {
                    self.local_hello.displays_revision.saturating_add(1)
                } else {
                    self.local_hello.displays_revision
                };
                self.agent_displays = report.displays;
                self.agent_displays_received = true;
                self.local_hello.displays_revision = revision;
                self.local_hello.displays = displays.clone();
                self.manager
                    .update_local_hello(self.local_hello.clone())
                    .await;
                self.rebuild_topology();
                let message =
                    transport::convert::LayoutMessage::DisplaysChanged { revision, displays };
                for peer in self.manager.members().await {
                    self.send_layout_to(peer, message.clone()).await;
                }
            }
            AgentSignal::Portal { interface, detail } => {
                tracing::info!(
                    interface = interface.as_str(),
                    detail = detail.as_str(),
                    "portal"
                );
                self.portal_state = "permission_required".to_owned();
                self.agent_degraded_reason =
                    format!("Ожидание разрешения портала: {interface} ({detail})");
            }
            AgentSignal::PortalRestoring { interface, detail } => {
                tracing::info!(
                    interface = interface.as_str(),
                    detail = detail.as_str(),
                    "portal permission restoring"
                );
                self.portal_state = "restoring".to_owned();
                self.agent_degraded_reason =
                    format!("Восстановление разрешения портала: {interface} ({detail})");
            }
            AgentSignal::Grant(changed) => self.on_portal_grant(changed).await,
            AgentSignal::Ignored => {}
        }
        Ok(())
    }

    /// Follows the captured pointer and turns a link edge into a transition.
    ///
    /// `docs/03-architecture.md`, «Топология и движение»: «Локально использовать
    /// фактическую позицию курсора ОС», and a transition is resolved by
    /// `geometry::resolve_motion`, which is also what makes the reverse direction of a
    /// bidirectional link the return edge.
    async fn track_pointer(&mut self, events: &[protocol::ipc::LocalEvent]) {
        // A link the pointer left through while remote waits for the current lease to be
        // released: a chain is two leases, not a relay.
        if matches!(self.router.lease(), fr::LeaseSlot::Idle) {
            if let Some((target, entry_position)) = self.pending_target.take() {
                tracing::info!(peer = %target, "walking the chained link");
                let topology_revision = self.router.topology_revision();
                self.remote_pointer = Some(entry_position.clone());
                self.remote_node = Some(target);
                self.feed(fr::Input::Local(fr::LocalInput::SwitchRequest {
                    trigger: fr::SwitchTrigger::Edge,
                    target,
                    entry_position,
                    topology_revision,
                }))
                .await;
                return;
            }
        }
        let remote = matches!(
            self.router.lease().source(),
            Some(lease) if lease.state == fr::SourceState::Remote
        );
        for event in events {
            // Only absolute positions are tracked: the wire carries no relative motion
            // (`docs/ipc.proto`: the source integrates deltas into a `PointerAbs`).
            let Some(protocol::ipc::local_event::Ev::Pointer(pointer)) = event.ev.as_ref() else {
                continue;
            };
            // The captured position is in the node's logical space, so it is resolved
            // against the manifests before anything geometric happens.
            let position = match self.topology.as_ref().and_then(|topology| {
                crate::topology::position_on(topology, &self.node_id, pointer.x, pointer.y)
            }) {
                Some(position) => position,
                None => continue,
            };
            let position = &position;
            let motion = match self
                .local_pointer
                .as_ref()
                .filter(|last| last.display_id == position.display_id)
            {
                Some(last) => geometry::Point::new(position.x - last.x, position.y - last.y),
                None => {
                    // A new display: the first event seeds the position, there is no step.
                    self.local_pointer = Some(position.clone());
                    continue;
                }
            };
            self.local_pointer = Some(position.clone());
            if remote {
                self.track_remote(motion).await;
            } else {
                self.track_local(position, motion).await;
            }
        }
    }

    /// Asks the agent to watch the local pointer, once, for the node that owns input.
    ///
    /// An observation has no lease and no suppression, so its `lease_id` is empty — which
    /// is also why the FSM ignores the `CaptureReady` that answers it
    /// (`routing::agent_signal`: an empty lease id is not a transition).
    async fn arm_observation(&mut self) {
        if self.observe_armed || !self.router.is_owner() {
            return;
        }
        let command = fr::AgentCommand::BeginCapture {
            kind: input_model::CaptureKind::Observe,
            // An observation has no lease; `agent_command` sends an empty `lease_id` for
            // it, so its `CaptureReady` answer is ignored by the FSM.
            lease_id: [0u8; 16],
            self_timeout_ms: 0,
        };
        let frame = routing::agent_command(&command);
        match self.agent.send(frame).await {
            Ok(()) => {
                self.observe_armed = true;
                tracing::info!("asked the agent to observe the local pointer");
            }
            Err(error) => tracing::warn!(%error, "cannot arm the local observation"),
        }
    }

    /// `Local`: a step that leaves a display through a link endpoint opens the transition.
    async fn track_local(&mut self, position: &geometry::Position, motion: geometry::Point) {
        // Only the owner opens leases, so only the owner reacts to an edge; a follower
        // watches its own pointer without acting on it.
        if !self.router.is_owner() {
            return;
        }
        let step = match self.topology.as_ref() {
            Some(topology) => geometry::resolve_motion(topology, &self.node_id, position, motion),
            None => None,
        };
        let Some(transition) = step else {
            return;
        };
        let target = transition.target.node;
        if target == self.node_id {
            return;
        }
        let entry_position = transition.target_position;
        let topology_revision = self.router.topology_revision();
        tracing::info!(
            peer = %target,
            x = entry_position.x,
            y = entry_position.y,
            "the pointer reached a link edge; asking for a transition"
        );
        self.remote_pointer = Some(entry_position.clone());
        self.remote_node = Some(target);
        self.feed(fr::Input::Local(fr::LocalInput::SwitchRequest {
            trigger: fr::SwitchTrigger::Edge,
            target,
            entry_position,
            topology_revision,
        }))
        .await;
    }

    /// Remote: the virtual position follows the captured deltas inside the peer's display
    /// until it leaves through the entry edge (the return) or through another link (a
    /// chain).
    async fn track_remote(&mut self, motion: geometry::Point) {
        let (Some(node), Some(position), Some(topology)) = (
            self.remote_node,
            self.remote_pointer.clone(),
            self.topology.as_ref(),
        ) else {
            return;
        };
        let step = geometry::Point::new(motion.x * self.pointer_gain, motion.y * self.pointer_gain);
        let crossing = geometry::resolve_motion(topology, &node, &position, step);
        let advanced = geometry::Position::new(
            position.display_id.clone(),
            position.x + step.x,
            position.y + step.y,
        );
        let Some(transition) = crossing else {
            self.remote_pointer = Some(advanced);
            return;
        };
        let target = transition.target.node;
        let entry_position = transition.target_position;
        let topology_revision = self.router.topology_revision();
        if target == self.node_id {
            // The reverse direction of the link we came in on: back to Local, with the
            // reason the FSM takes from the trigger (`RETURN_EDGE`).
            tracing::info!("the pointer came back over the entry edge; returning to Local");
            self.remote_pointer = None;
            self.remote_node = None;
        } else {
            // A link that leads further on: the lease has to be released first, so the
            // target is remembered and walked once the FSM is Local again.
            tracing::info!(peer = %target, "the pointer crossed a chained link");
            self.pending_target = Some((target, entry_position.clone()));
            self.remote_pointer = None;
            self.remote_node = None;
        }
        let peer = self
            .router
            .lease()
            .source()
            .map(|lease| lease.peer)
            .unwrap_or(target);
        self.feed(fr::Input::Local(fr::LocalInput::SwitchRequest {
            trigger: fr::SwitchTrigger::Edge,
            target: peer,
            // The receiver ignores the position of a return (the pointer is already
            // there); the chained entry point is carried by `pending_target`.
            entry_position: geometry::Position::new(entry_position.display_id.clone(), 0.0, 0.0),
            topology_revision,
        }))
        .await;
    }

    /// Captured input goes on the wire as `InputBatch` for the active source lease.
    ///
    /// The FSM does not model captured events: the source integrates them and stamps
    /// them with the lease ([`fr::Message::InputBatch`]), and `transport` owns the
    /// batch sequence (it *is* the envelope `seq`).
    async fn forward_captured(&mut self, events: Vec<InputEvent>) {
        if events.is_empty() {
            return;
        }
        let lease = match self.router.lease().source() {
            Some(lease) if lease.state == fr::SourceState::Remote => (lease.peer, lease.lease_id),
            _ => {
                tracing::warn!(
                    events = events.len(),
                    "captured input outside a remote source lease; dropped"
                );
                return;
            }
        };
        let (peer, lease_id) = lease;
        // The peer receives *its* display's local coordinates: the tracker's virtual
        // position, not the source's node-space numbers (see `topology::forward_events`).
        let events = crate::topology::forward_events(events, self.remote_pointer.as_ref());
        let message = fr::Message::InputBatch {
            lease_id,
            // Stamped over by the transport (the wire defines the batch sequence as
            // the envelope sequence).
            input_batch_seq: 0,
            events,
        };
        tracing::info!(peer = %peer, "core -> peer InputBatch");
        if let Some(handle) = self.manager.handle(&peer).await {
            if let Err(error) = handle.send(message).await {
                tracing::warn!(peer = %peer, %error, "InputBatch send failed");
            }
        }
    }

    /// Core's half of scenarios S3/S4: report the loss and restart in `RECOVERY`.
    async fn on_agent_lost(&mut self) {
        if self.agent_lost_reported {
            return;
        }
        for grant in self.agent.drain_portal_grants().await {
            self.on_portal_grant(grant).await;
        }
        self.agent_lost_reported = true;
        self.agent.stop().await;
        self.feed(fr::Input::Agent(fr::AgentEvent::AgentLost)).await;
        if record_restart_failure(&mut self.agent_restart_failures, Instant::now()) {
            self.open_restart_circuit("agent lost three times within 60 seconds");
            return;
        }
        tracing::warn!("agent lost; scheduling a RECOVERY restart");
        self.agent_restart_due = Some(Instant::now() + self.agent_restart_backoff);
        self.agent_restart_backoff = next_restart_backoff(self.agent_restart_backoff);
    }

    /// The `RECOVERY` restart from core's conservative ledger.
    async fn restart_agent(&mut self) {
        if self.agent_restart_circuit_open {
            return;
        }
        let keys: Vec<input_model::HidKey> = self.router.pressed().keys().collect();
        let buttons: Vec<input_model::MouseButton> = self.router.pressed().buttons().collect();
        tracing::info!(
            keys = keys.len(),
            buttons = buttons.len(),
            "restarting the agent in RECOVERY"
        );
        match self.agent.restart_recovery(&keys, &buttons).await {
            Ok(_) => {
                self.agent_lost_reported = false;
            }
            Err(error) => {
                tracing::error!(%error, "agent restart failed");
                if record_restart_failure(&mut self.agent_restart_failures, Instant::now()) {
                    self.open_restart_circuit("agent restart failed three times within 60 seconds");
                } else {
                    self.agent_restart_due = Some(Instant::now() + self.agent_restart_backoff);
                    self.agent_restart_backoff = next_restart_backoff(self.agent_restart_backoff);
                }
            }
        }
    }

    fn open_restart_circuit(&mut self, reason: &str) {
        self.agent_restart_circuit_open = true;
        self.agent_restart_due = None;
        self.agent_degraded_reason =
            format!("Ввод остановлен: {reason}; нажмите «Повторить» для ручного запуска");
        tracing::error!(reason, "agent restart circuit breaker opened");
    }
    async fn retry_agent(&mut self) -> protocol::ipc::OpResult {
        self.agent_restart_circuit_open = false;
        self.agent_restart_failures.clear();
        self.agent_restart_backoff = RESTART_BACKOFF;
        self.agent_restart_due = None;
        self.portal_failure = false;
        self.agent.set_portal_sessions_allowed(true);
        self.agent_degraded_reason.clear();
        self.refusal = None;
        self.portal_state.clear();
        let keys: Vec<input_model::HidKey> = self.router.pressed().keys().collect();
        let buttons: Vec<input_model::MouseButton> = self.router.pressed().buttons().collect();
        match self.agent.restart_recovery(&keys, &buttons).await {
            Ok(_) => {
                self.agent_lost_reported = false;
                protocol::ipc::OpResult {
                    ok: true,
                    error_code: String::new(),
                }
            }
            Err(error) => {
                self.agent_degraded_reason = format!("manual retry failed: {error}");
                protocol::ipc::OpResult {
                    ok: false,
                    error_code: "AGENT_RETRY_FAILED".to_owned(),
                }
            }
        }
    }

    /// One CLI request.
    ///
    /// One request, one reply, one connection: the daemon reads a single line and writes a
    /// single line, so it never reads again after answering (that second read is what used
    /// to race the client's close and log every answer as a failure).
    async fn on_control(&mut self, mut stream: ctl::ClientStream) {
        let line = match ctl::read_line(&mut stream).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                // A client that connected and closed without a line. Core used to drop such
                // connections without saying anything, which hid a cancelled `accept` for a
                // while (`ipc::transport::GuiListener::accept`); the warning is how the next
                // reader of the log sees it.
                tracing::warn!("a control client closed without sending a request");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot read a control request");
                return;
            }
        };
        let response = self.handle_request(&line).await;
        match response {
            Handled::Reply(response) => {
                let reply = response.render();
                if let Err(error) = ctl::write_line(&mut stream, &reply).await {
                    tracing::warn!(%error, "cannot answer a control request");
                }
            }
            Handled::Deferred(verdict) => {
                // The loop must not block on a peer: the connection is handed to a writer that
                // waits for the coordinator's verdict and answers either way. A verdict that
                // never comes is not invented — `PENDING_VERDICT` says so, and the edit stays
                // pending on this node.
                tokio::spawn(async move {
                    let mut stream = stream;
                    let reply = match tokio::time::timeout(LAYOUT_VERDICT_TIMEOUT, verdict).await {
                        Ok(Ok(response)) => response.render(),
                        // The request was dropped (core is stopping) or the answer never came:
                        // both mean there is no verdict to report.
                        Ok(Err(_)) | Err(_) => ctl::Response::Error {
                            code: "PENDING_VERDICT".to_owned(),
                            detail: "the coordinator has not answered; the edit is kept as pending"
                                .to_owned(),
                        }
                        .render(),
                    };
                    if let Err(error) = ctl::write_line(&mut stream, &reply).await {
                        tracing::warn!(%error, "cannot answer a deferred control request");
                    }
                });
            }
        }
    }

    async fn handle_request(&mut self, line: &str) -> Handled {
        let request = match ctl::Request::parse(line) {
            Ok(request) => request,
            Err(detail) => {
                return Handled::Reply(ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail,
                })
            }
        };
        tracing::info!(?request, "control request");
        match request {
            ctl::Request::Ping => ctl::Response::Pong.into(),
            ctl::Request::Status => ctl::Response::Status(self.status_line().await).into(),
            ctl::Request::ResetPortals => self.reset_portals().await.into(),
            ctl::Request::Quit => {
                self.stopping = true;
                ctl::Response::Stopping.into()
            }
            ctl::Request::Switch(peer) => self.switch_to(&peer).await.into(),
            ctl::Request::Claim => self.claim().await.into(),
            ctl::Request::Abandon => self.abandon().await.into(),
            ctl::Request::LayoutShow => ctl::Response::Status(self.layout_line()).into(),
            ctl::Request::LayoutEdit(path) => self.layout_edit(&path).await,
            ctl::Request::LayoutApply => self.layout_apply().await,
            ctl::Request::PairOpen(seconds) => {
                let window_s = seconds.unwrap_or(120);
                match self.open_pairing_window(window_s) {
                    Ok(window) => ctl::Response::Status(format!(
                        "address={} attempt={} seconds={}",
                        window.address,
                        crate::command::hex16(&window.attempt_id),
                        window.seconds
                    )),
                    Err((code, detail)) => ctl::Response::Error { code, detail },
                }
                .into()
            }
            ctl::Request::PairPrompt => {
                Handled::Deferred(prompt_of(self.pairing.wait_for_prompt()))
            }
            ctl::Request::PairAnswer { attempt, approve } => {
                match crate::command::hex16_decode(&attempt) {
                    Some(attempt) => match self.pairing.answer(&attempt, approve) {
                        Ok(verdict) => Handled::Deferred(verdict_of(verdict)),
                        Err((code, detail)) => ctl::Response::Error { code, detail }.into(),
                    },
                    None => ctl::Response::Error {
                        code: "BAD_REQUEST".to_owned(),
                        detail: "the attempt id is 32 hex characters".to_owned(),
                    }
                    .into(),
                }
            }
            ctl::Request::PairWith(address) => {
                let wait = self.pairing.dial_started();
                self.spawn_pair_dial(address);
                Handled::Deferred(verdict_of(wait))
            }
            ctl::Request::LayoutDiscard => {
                let dropped = self.release_layout_pending();
                tracing::info!(dropped, "pending layout edit discarded");
                ctl::Response::Status(format!(
                    "pending={}",
                    if dropped { "discarded" } else { "none" }
                ))
                .into()
            }
        }
    }

    /// `switch <peer>`: a manual transition out of `Local`, or back to `Local` when a
    /// lease is already active.
    ///
    /// The FSM models the manual trigger as [`fr::SwitchTrigger::Hotkey`] and treats a
    /// trigger while `Remote` as the return edge, which is what makes two invocations
    /// a full there-and-back cycle without a second verb.
    async fn switch_to(&mut self, peer: &PeerRef) -> ctl::Response {
        let target = match crate::command::resolve_peer(&self.trust, peer) {
            Ok(target) => target,
            Err(detail) => {
                return ctl::Response::Error {
                    code: "UNKNOWN_PEER".to_owned(),
                    detail,
                }
            }
        };
        // The layout document (KVM-072) is what supplies a real entry point; the
        // receiver ignores the position (`focus_router` treats it as advisory on
        // `EnterPrepare`), so the manual trigger carries a neutral one.
        let entry_position = geometry::Position::new(String::new(), 0.0, 0.0);
        let revision = self.router.topology_revision();
        let now = self.now_ms();
        let outputs = self.router.handle(
            now,
            fr::Input::Local(fr::LocalInput::SwitchRequest {
                trigger: fr::SwitchTrigger::Hotkey,
                target,
                entry_position,
                topology_revision: revision,
            }),
        );
        let refusal = outputs.iter().find_map(|output| match output {
            fr::Output::Notice(fr::Notice::SwitchRefused { reason, .. }) => {
                Some(format!("{reason:?}"))
            }
            _ => None,
        });
        self.apply(outputs).await;
        match refusal {
            Some(reason) => ctl::Response::Error {
                code: "SWITCH_REFUSED".to_owned(),
                detail: reason,
            },
            None => ctl::Response::Switched(peer.clone()),
        }
    }

    /// `claim`: the user (GUI button or the claim hotkey) asks this node to own the
    /// workspace.
    ///
    /// `docs/05-protocol.md`: «Владение можно заявить с любого узла» — the FSM runs the
    /// whole claim itself (`OwnerClaim`, then self-grant after `claim_grant_timeout_ms`
    /// when the current owner cannot answer), and a refusal (`CANNOT_OWN`,
    /// `STALE_OWNER_SERIAL`) comes back as an `ERR` instead of a silent no-op.
    async fn claim(&mut self) -> ctl::Response {
        let now = self.now_ms();
        let outputs = self.router.handle(
            now,
            fr::Input::Local(fr::LocalInput::Claim {
                reason: fr::ClaimReason::User,
            }),
        );
        let refusal = outputs.iter().find_map(|output| match output {
            fr::Output::Notice(fr::Notice::ClaimRefused { code }) => {
                Some(code.map_or("refused".to_owned(), |code| format!("{code:?}")))
            }
            _ => None,
        });
        self.apply(outputs).await;
        match refusal {
            Some(detail) => ctl::Response::Error {
                code: "CLAIM_REFUSED".to_owned(),
                detail,
            },
            None => ctl::Response::Claimed,
        }
    }

    /// `abandon`: the user gives up ownership and stays a follower.
    ///
    /// The FSM releases any lease of ours before it publishes the loss
    /// (`scenarios.rs::s26_release_precedes_ownership_grant`), so the reply means the
    /// abandonment was handed over, not that every peer has seen it yet.
    async fn abandon(&mut self) -> ctl::Response {
        let now = self.now_ms();
        let outputs = self
            .router
            .handle(now, fr::Input::Local(fr::LocalInput::Abandon));
        self.apply(outputs).await;
        ctl::Response::Abandoned
    }

    /// Stores or drops a portal token the agent reported.
    ///
    /// `docs/03-architecture.md`: core owns `portal-restore.toml`, so the token is
    /// written back atomically here and never logged; a revocation drops the interface
    /// it names, and an empty grant drops everything.
    async fn on_portal_grant(&mut self, changed: protocol::ipc::PortalGrantChanged) {
        let (changed_store, note) = match (changed.grant, changed.revoked) {
            (Some(grant), false) => {
                let interface = grant.portal_interface.clone();
                let stored = self.agent.portal().accept(grant);
                (
                    stored,
                    if stored {
                        format!("portal grant received: {interface}")
                    } else {
                        format!("portal grant unchanged: {interface}")
                    },
                )
            }
            (Some(grant), true) => {
                let interface = grant.portal_interface.clone();
                let dropped = self.agent.portal().revoke(&interface);
                (dropped, format!("portal grant revoked: {interface}"))
            }
            (None, _) => {
                self.agent.portal().clear();
                (true, "portal grants cleared".to_owned())
            }
        };
        if changed_store {
            if let Err(error) = self.agent.portal().save() {
                tracing::warn!(error = error.as_str(), "cannot persist the portal tokens");
            }
        }
        let tokens = self.agent.portal().len();
        let interfaces = self.agent.portal().interfaces().join(",");
        tracing::info!(
            note = note.as_str(),
            tokens,
            interfaces = interfaces.as_str(),
            "portal token store updated"
        );
        self.refusal = Some(note);
    }

    /// `reset-portals`: forget every stored token, in core and in the agent.
    ///
    /// This is the `ResetPortalGrants`/`ForgetPortalGrants` path of `docs/ipc.proto`:
    /// the agent drops its in-memory copy and the portal asks again on the next
    /// session, which is what a user wants after changing the compositor or denying a
    /// dialog by mistake.
    async fn reset_portals(&mut self) -> ctl::Response {
        self.agent.portal().clear();
        if let Err(error) = self.agent.portal().save() {
            return ctl::Response::Error {
                code: "PORTAL_STORE".to_owned(),
                detail: error,
            };
        }
        if let Err(error) = self
            .agent
            .send(Msg::ForgetPortalGrants(
                protocol::ipc::ForgetPortalGrants {},
            ))
            .await
        {
            return ctl::Response::Error {
                code: "AGENT_LOST".to_owned(),
                detail: error,
            };
        }
        self.refusal = Some("portal grants cleared".to_owned());
        tracing::info!("portal grants cleared in core and in the agent");
        ctl::Response::Status(self.status_line().await)
    }

    async fn status_line(&mut self) -> String {
        let focus = self.router.focus();
        let lease_peer = match self.router.lease() {
            fr::LeaseSlot::Source(lease) => Some(lease.peer),
            fr::LeaseSlot::Receiver(lease) => Some(lease.source),
            fr::LeaseSlot::Idle => None,
        };
        let owner = self.router.owner().copied();
        let node_id = crate::command::node_id_hex(&self.node_id);
        let owner_hex = owner.map(|owner| crate::command::node_id_hex(&owner));
        let peer_hex = lease_peer.map(|peer| crate::command::node_id_hex(&peer));
        ctl::status_line(&ctl::StatusInput {
            focus: focus.as_str(),
            role: if self.router.is_owner() {
                "owner"
            } else {
                "follower"
            },
            lease_peer: peer_hex.as_deref(),
            members: self.manager.member_count().await,
            agent: self.agent.state(),
            backend: self.backend.as_str(),
            node_id: node_id.as_str(),
            owner: owner_hex.as_deref().unwrap_or(""),
            serial: self.router.serial(),
            // The mirror of the agent's ledger: what the FSM refuses a normal transition on.
            pressed: self.router.pressed().keys().count() + self.router.pressed().buttons().count(),
            refusal: self.refusal.as_deref().unwrap_or(""),
        })
    }
}

/// The ctl code of a layout refusal: the contract's enum name without its prefix.
///
/// The proto spells it `ERROR_CODE_LAYOUT_CONFLICT`; the CLI, the GUI and the `last_error=`
/// field of `layout show` all use the code itself, and the coordinator refuses with the same
/// word, so `ERR LAYOUT_CONFLICT …` reads the same on both sides of the link.
fn layout_refusal_code(code: protocol::wire::ErrorCode) -> String {
    code.as_str_name()
        .strip_prefix("ERROR_CODE_")
        .unwrap_or_else(|| code.as_str_name())
        .to_owned()
}

/// The sentence an editor reads when the coordinator refuses its edit.
///
/// A `LAYOUT_CONFLICT` that names the revision the coordinator is on is what «показать текущую
/// ревизию и перезагрузить документ» (`docs/05-protocol.md`, the error table) asks for: the
/// editor reloads *that* revision and writes its edit against it.
fn refusal_detail(code: protocol::wire::ErrorCode, layout_revision: u64) -> String {
    if code == protocol::wire::ErrorCode::LayoutConflict && layout_revision > 0 {
        return format!(
            "the coordinator has revision {layout_revision} applied; reload the document and \
             reapply"
        );
    }
    format!(
        "the coordinator refused the edit ({})",
        layout_refusal_code(code)
    )
}

/// What one turn of the loop produced.
enum Step {
    Link(Option<LinkEvent>),
    Agent(Result<protocol::ipc::AgentFrame, String>),
    Control(Result<ctl::ClientStream, ipc::IpcError>),
    /// A GUI connected; its connection gets its own task.
    GuiClient(Result<ipc::transport::ClientStream, ipc::IpcError>),
    /// One request from a connected GUI.
    Gui(Option<crate::gui::Request>),
    /// One report from a pairing ceremony.
    Pairing(Option<pairing::Event>),
    /// One event from the clipboard runtime: a bound channel, a message, or a lost channel.
    Clipboard(Option<crate::clipboard::Inbound>),
    Tick,
}

/// What one control request produced.
///
/// A reply that arrives when a pairing ceremony reaches the step it names.
///
/// The `pair-*` verbs cannot answer at once — the window is opened here and the peer drives it
/// from its own node — so the caller is handed a channel and the loop's `Deferred` writer
/// answers it whenever the ceremony gets there (and gives up on it at
/// [`LAYOUT_VERDICT_TIMEOUT`], like every other deferred answer).
fn verdict_of(verdict: oneshot::Receiver<pairing::Verdict>) -> oneshot::Receiver<ctl::Response> {
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let response = match verdict.await {
            Ok(Ok(())) => ctl::Response::Status("paired".to_owned()),
            Ok(Err((code, detail))) => ctl::Response::Error { code, detail },
            Err(_) => ctl::Response::Error {
                code: "PAIRING_GONE".to_owned(),
                detail: "the ceremony ended before it could answer".to_owned(),
            },
        };
        let _ = sender.send(response);
    });
    receiver
}

/// The `pair-prompt` reply: the six digits, and everything needed to answer them.
fn prompt_of(prompt: oneshot::Receiver<pairing::Prompt>) -> oneshot::Receiver<ctl::Response> {
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let response = match prompt.await {
            Ok(prompt) => ctl::Response::Status(format!(
                "sas={} peer={} attempt={} address={}",
                prompt.sas_code,
                prompt.peer_name,
                crate::command::hex16(&prompt.attempt_id),
                prompt.address.unwrap_or_default()
            )),
            Err(_) => ctl::Response::Error {
                code: "PAIRING_GONE".to_owned(),
                detail: "the window closed before a peer appeared".to_owned(),
            },
        };
        let _ = sender.send(response);
    });
    receiver
}

/// A `layout edit`/`layout apply` on a follower is not answered by the node that read the
/// line: only the coordinator's CAS knows the verdict, so the connection is handed the pair of
/// [`oneshot::Receiver`] and the timeout instead of an invented answer.
enum Handled {
    /// The answer is known here and now.
    Reply(ctl::Response),
    /// The coordinator was asked; the verdict is on its way.
    Deferred(oneshot::Receiver<ctl::Response>),
}

impl From<ctl::Response> for Handled {
    fn from(response: ctl::Response) -> Self {
        Handled::Reply(response)
    }
}

/// One layout request whose verdict has not arrived.
struct LayoutRequest {
    /// The document the request carried.
    ///
    /// An applied verdict releases the pending slot only when the slot still holds *this*
    /// document: a newer edit that arrived while the request was in flight is not thrown away.
    layout_toml: String,
    /// Where the connection that asked is waiting.
    verdict: oneshot::Sender<ctl::Response>,
}

/// The FSM's timers on the loop's millisecond clock.
#[derive(Default)]
struct Timers {
    deadlines: BTreeMap<fr::Timer, u64>,
}

impl Timers {
    fn arm(&mut self, timer: fr::Timer, deadline_ms: u64) {
        self.deadlines.insert(timer, deadline_ms);
    }

    fn cancel(&mut self, timer: fr::Timer) {
        self.deadlines.remove(&timer);
    }

    /// The timer the loop must service first: the earliest deadline, with the timer name
    /// as a deterministic tie-break (`Session::next_deadline` uses this to bound its sleep,
    /// so a name-ordered map must not decide it).
    fn next(&self) -> Option<(fr::Timer, u64)> {
        self.deadlines
            .iter()
            .min_by(|left, right| left.1.cmp(right.1).then_with(|| left.0.cmp(right.0)))
            .map(|(timer, deadline)| (*timer, *deadline))
    }

    /// Every timer whose deadline has passed, removed from the wheel.
    fn due(&mut self, now_ms: u64) -> Vec<fr::Timer> {
        let due: Vec<fr::Timer> = self
            .deadlines
            .iter()
            .filter(|(_, deadline)| **deadline <= now_ms)
            .map(|(timer, _)| *timer)
            .collect();
        for timer in &due {
            self.deadlines.remove(timer);
        }
        due
    }
}

/// Revalidates an edited layout document (KVM-072: the coordinator validates the whole
/// document; ADR-013 rejected merging rows for exactly that reason).
///
/// The document is the `[topology]` shape, so it parses as the same struct the configuration
/// uses; a document with neither displays nor links is refused, because an empty layout would
/// silently disable every transition.
fn validate_layout_toml(layout_toml: &str) -> Result<(), String> {
    let parsed: config::TopologyConfig =
        toml::from_str(layout_toml).map_err(|error| format!("layout_toml: {error}"))?;
    if parsed.displays.is_empty() && parsed.links.is_empty() {
        return Err("layout_toml: a document with neither displays nor links".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod timer_tests {
    use super::*;

    #[test]
    fn three_restart_failures_open_the_circuit_only_inside_the_window() {
        let start = Instant::now();
        let mut failures = VecDeque::new();
        assert!(!record_restart_failure(&mut failures, start));
        assert!(!record_restart_failure(
            &mut failures,
            start + Duration::from_secs(10)
        ));
        assert!(record_restart_failure(
            &mut failures,
            start + Duration::from_secs(20)
        ));

        let mut failures = VecDeque::new();
        assert!(!record_restart_failure(&mut failures, start));
        assert!(!record_restart_failure(
            &mut failures,
            start + RESTART_FAILURE_WINDOW + Duration::from_millis(1)
        ));
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn restart_policy_stops_retries_instead_of_spawning_a_ten_second_storm() {
        let start = Instant::now();
        let mut failures = VecDeque::new();
        let mut backoff = RESTART_BACKOFF;
        let mut now = start;
        let mut automatic_spawns = 0;
        while !record_restart_failure(&mut failures, now)
            && now.duration_since(start) < Duration::from_secs(10)
        {
            now += backoff;
            automatic_spawns += 1;
            backoff = next_restart_backoff(backoff);
        }
        assert_eq!(automatic_spawns, 2);
        assert!(now.duration_since(start) < Duration::from_secs(10));
        assert_eq!(backoff, Duration::from_secs(2));
    }

    #[test]
    fn restart_backoff_doubles_and_caps_at_eight_seconds() {
        let delays = (0..5)
            .scan(RESTART_BACKOFF, |current, _| {
                let next = *current;
                *current = next_restart_backoff(next);
                Some(next)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ]
        );
        assert_eq!(
            next_restart_backoff(Duration::from_secs(8)),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn a_timer_fires_once_at_its_deadline() {
        let mut timers = Timers::default();
        timers.arm(fr::Timer::PrepareTimeout, 100);

        assert!(timers.due(99).is_empty(), "not yet due");
        assert_eq!(timers.due(100).to_vec(), vec![fr::Timer::PrepareTimeout]);
        assert!(
            timers.due(1_000).is_empty(),
            "a fired timer is removed from the wheel"
        );
        assert_eq!(timers.next(), None);
    }

    #[test]
    fn cancelling_removes_a_timer_and_re_arming_moves_its_deadline() {
        let mut timers = Timers::default();
        timers.arm(fr::Timer::ReleaseGuard, 500);
        assert_eq!(timers.next(), Some((fr::Timer::ReleaseGuard, 500)));

        timers.cancel(fr::Timer::ReleaseGuard);
        assert_eq!(timers.next(), None);
        assert!(timers.due(10_000).is_empty());

        // Cancelling something that was never armed is a no-op.
        timers.cancel(fr::Timer::ClaimGrant);
        assert_eq!(timers.next(), None);

        timers.arm(fr::Timer::ClaimGrant, 600);
        timers.arm(fr::Timer::ClaimGrant, 900);
        assert_eq!(
            timers.next(),
            Some((fr::Timer::ClaimGrant, 900)),
            "re-arming replaces the deadline"
        );
        assert!(timers.due(600).is_empty(), "the old deadline is gone");
        assert_eq!(timers.due(900).to_vec(), vec![fr::Timer::ClaimGrant]);
    }

    #[test]
    fn several_timers_fire_in_one_turn_and_leave_the_future_ones_armed() {
        let mut timers = Timers::default();
        timers.arm(fr::Timer::PrepareTimeout, 100);
        timers.arm(fr::Timer::ReceiverChallenge, 100);
        timers.arm(fr::Timer::ReceiverLeaseDeadline, 900);

        let mut due = timers.due(100);
        due.sort_by_key(|timer| format!("{timer:?}"));
        assert_eq!(
            due,
            vec![fr::Timer::PrepareTimeout, fr::Timer::ReceiverChallenge]
        );
        assert_eq!(
            timers.next(),
            Some((fr::Timer::ReceiverLeaseDeadline, 900)),
            "the future timer stays armed"
        );
    }

    /// `next()` feeds `Session::next_deadline`, whose contract is «When the loop must wake
    /// next»: the earliest deadline, whatever the timer's name is.
    #[test]
    fn the_next_wakeup_is_the_earliest_deadline_not_the_first_timer_name() {
        let mut timers = Timers::default();
        // `PrepareTimeout` sorts first in `fr::Timer`, but its deadline is the later one.
        timers.arm(fr::Timer::PrepareTimeout, 400);
        timers.arm(fr::Timer::ReceiverChallenge, 20);

        assert_eq!(
            timers.next(),
            Some((fr::Timer::ReceiverChallenge, 20)),
            "a wheel keyed by timer name must still report the earliest deadline"
        );
    }
    #[test]
    fn an_agent_ready_event_triggers_a_display_query() {
        assert!(is_agent_ready(&fr::AgentEvent::AgentReady));
        assert!(!is_agent_ready(&fr::AgentEvent::Ready {
            lease_id: [0; 16]
        }));
    }
}
