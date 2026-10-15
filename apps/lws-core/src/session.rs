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
use ipc::clock::{Clock, SuspendDetector, SystemClock};

/// The GUI's half of the session: `GuiFrame` requests and the events pushed to the window.
mod gui;
/// The pairing window and its ceremonies (ALPN `pair/1` on the link listener).
mod pairing;
mod transition_retry;

/// Attribution belongs to the operation, never to mutable "last requester" state.
/// GUI/control edits (including their Auto buttons) retain the requesting connection.
/// Default publication, display-id writeback, and coordinator republishing are automatic.
/// Peer edits carry the identity negotiated by the currently established transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LayoutSource {
    source: &'static str,
    connection_id: Option<[u8; 16]>,
    peer: Option<NodeId>,
}

impl LayoutSource {
    const AUTO: Self = Self {
        source: "auto",
        connection_id: None,
        peer: None,
    };

    fn local(source: &'static str, connection_id: [u8; 16]) -> Self {
        Self {
            source,
            connection_id: Some(connection_id),
            peer: None,
        }
    }

    fn remote(peer: NodeId, connection_id: [u8; 16]) -> Self {
        Self {
            source: "peer",
            connection_id: Some(connection_id),
            peer: Some(peer),
        }
    }
}

#[cfg(test)]
mod layout_source_tests {
    use super::LayoutSource;
    use geometry::NodeId;

    #[test]
    fn provenance_is_request_specific_and_connection_stable() {
        let gui = LayoutSource::local("gui", [1; 16]);
        let ctl = LayoutSource::local("ctl", [2; 16]);
        let another_gui_request = LayoutSource::local("gui", [1; 16]);
        assert_eq!(gui.source, "gui");
        assert_eq!(gui.connection_id, Some([1; 16]));
        assert_eq!(gui.peer, None);
        assert_eq!(ctl.source, "ctl");
        assert_eq!(ctl.connection_id, Some([2; 16]));
        assert_eq!(gui, another_gui_request);
        assert_ne!(gui, ctl);
    }

    #[test]
    fn remote_and_automatic_revisions_have_distinct_provenance() {
        let peer = NodeId::new([3; 32]);
        let remote = LayoutSource::remote(peer, [4; 16]);
        assert_eq!(remote.source, "peer");
        assert_eq!(remote.peer, Some(peer));
        assert_eq!(remote.connection_id, Some([4; 16]));
        assert_eq!(LayoutSource::AUTO.source, "auto");
        assert_eq!(LayoutSource::AUTO.peer, None);
        assert_eq!(LayoutSource::AUTO.connection_id, None);
    }
}

/// How long the loop sleeps when nothing else has a deadline.
///
/// It bounds how long a dead agent child can go unnoticed; the IPC heartbeat and the
/// FSM timers are all shorter than this in the default configuration.
const IDLE_TICK: Duration = Duration::from_millis(50);
const RESTART_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(8);
const RESTART_CIRCUIT_FAILURES: usize = 3;
/// How much wall time the monotonic clock may fail to account for before the machine is
/// judged to have been suspended.
///
/// `CLOCK_MONOTONIC` ([`Instant`]) is frozen across a suspend while `CLOCK_REALTIME`
/// (`SystemTime`) is not, so a resume leaves wall time ahead by the length of the sleep.
/// Two seconds is far above the scheduling jitter that separates the two clocks in
/// ordinary operation and far below the sleeps that matter; a shorter sleep that goes
/// unnoticed is harmless, because the frozen monotonic clock leaves the heartbeat's
/// silence reading unchanged too.
const SUSPEND_THRESHOLD: Duration = Duration::from_secs(2);
/// Wall-clock window the restart losses are counted in.
const RESTART_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How long an open circuit waits before it closes itself and the agent is retried.
///
/// A breaker that only the user can reset leaves the daemon degraded for ever after one
/// bad minute (`target/live-e2e/agent-lost-20261013T1800Z`: the laptop slept, three
/// losses landed inside the window, and the node stayed `Blocked` with no agent and
/// `members = 0` until the process was restarted by hand).
const RESTART_CIRCUIT_COOLDOWN: Duration = Duration::from_secs(120);

/// Core's agent-restart policy: the recent losses, the circuit they open, and whether the
/// machine was just suspended.
///
/// Three losses inside [`RESTART_FAILURE_WINDOW`] of **wall-clock** time stop the
/// automatic restarts; [`RESTART_CIRCUIT_COOLDOWN`] after the circuit opens it closes
/// itself and the agent is retried. The window is wall time and not monotonic time: a
/// laptop that slept between two losses did not suffer two losses «within 60 seconds»,
/// however little `CLOCK_MONOTONIC` advanced while it was suspended. A loss that follows a
/// suspend is not the agent's at all — the machine froze — so it is not counted and cannot
/// trip the breaker.
#[derive(Debug)]
struct RestartWatch {
    /// Wall-clock milliseconds of the recent losses, oldest first.
    losses: VecDeque<u64>,
    /// When the circuit opened, in wall-clock milliseconds; `None` while it is closed.
    opened_at: Option<u64>,
    /// The machine was suspended since the last agent frame (see [`SuspendDetector`]).
    suspend_recent: bool,
}

impl RestartWatch {
    fn new() -> Self {
        Self {
            losses: VecDeque::new(),
            opened_at: None,
            suspend_recent: false,
        }
    }

    /// Whether the circuit is open: automatic restarts are stopped.
    fn is_open(&self) -> bool {
        self.opened_at.is_some()
    }

    /// A suspend was detected: the loss it would produce is the freeze's, not the agent's.
    fn on_suspend(&mut self) {
        self.suspend_recent = true;
    }

    /// The agent answered: whatever happened across a suspend, it is alive again, so a
    /// later loss is its own.
    fn on_frame(&mut self) {
        self.suspend_recent = false;
    }

    /// Consumes the suspend flag: whether the machine was suspended since the last frame.
    fn take_suspend(&mut self) -> bool {
        std::mem::take(&mut self.suspend_recent)
    }

    /// Records a loss; `true` when that opens the circuit and the restarts must stop.
    fn record_loss(&mut self, wall_ms: u64) -> bool {
        self.forget_expired(wall_ms);
        self.losses.push_back(wall_ms);
        if self.losses.len() >= RESTART_CIRCUIT_FAILURES {
            self.opened_at = Some(wall_ms);
            return true;
        }
        false
    }

    /// Closes the circuit once the cool-down has passed; `true` when it just closed.
    fn close_if_cooled(&mut self, wall_ms: u64) -> bool {
        match self.opened_at {
            Some(opened)
                if wall_ms.saturating_sub(opened)
                    >= RESTART_CIRCUIT_COOLDOWN.as_millis() as u64 =>
            {
                self.close();
                true
            }
            _ => false,
        }
    }

    /// The user asked (`RetryAgent`): the circuit and the recorded losses go.
    fn close(&mut self) {
        self.losses.clear();
        self.opened_at = None;
        self.suspend_recent = false;
    }

    /// Drops the losses that are older than the window.
    fn forget_expired(&mut self, wall_ms: u64) {
        let window = RESTART_FAILURE_WINDOW.as_millis() as u64;
        while self
            .losses
            .front()
            .is_some_and(|failed| wall_ms.saturating_sub(*failed) > window)
        {
            self.losses.pop_front();
        }
    }
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

/// The first four bytes of a lease id in hex, for a log line that a human scans.
fn lease_hex(lease_id: &fr::LeaseId) -> String {
    crate::command::hex16(lease_id).chars().take(8).collect()
}

fn input_event_counts(events: &[InputEvent]) -> (usize, usize, usize, usize) {
    events.iter().fold((0, 0, 0, 0), |mut counts, event| {
        match event {
            InputEvent::Key { .. } => counts.0 += 1,
            InputEvent::PointerAbsolute(_) | InputEvent::PointerRelative { .. } => counts.1 += 1,
            InputEvent::Button { .. } => counts.2 += 1,
            InputEvent::Scroll { .. } => counts.3 += 1,
        }
        counts
    })
}

/// Plans a portal activation without granting a follower a source lease.
fn portal_activation_plan(
    topology: Option<&geometry::Topology>,
    node: &NodeId,
    is_owner: bool,
    focus: fr::FocusState,
    activation: Option<&protocol::ipc::PortalActivation>,
) -> Result<(geometry::Position, geometry::Transition), protocol::ipc::EndCapture> {
    let resolved = if is_owner && focus == fr::FocusState::Local {
        topology
            .zip(activation)
            .and_then(|(topology, activation)| {
                crate::topology::portal_transition(topology, node, activation)
            })
            .filter(|(_, transition)| transition.target.node != *node)
    } else {
        None
    };
    if let Some(resolved) = resolved {
        return Ok(resolved);
    }
    Err(protocol::ipc::EndCapture {
        lease_id: Vec::new(),
        replay_buffered_locally: true,
    })
}

#[derive(Default)]
struct InputDiagnostics {
    lease_id: Option<fr::LeaseId>,
    hook: Option<protocol::ipc::EdgeCounters>,
    captured: input_model::EdgeCounters,
    inject: input_model::EdgeCounters,
    applied: Option<protocol::ipc::EdgeCounters>,
}

impl InputDiagnostics {
    fn for_lease(&mut self, lease_id: fr::LeaseId) {
        if self.lease_id != Some(lease_id) {
            *self = Self {
                lease_id: Some(lease_id),
                ..Self::default()
            };
        }
    }
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
    /// The loop's clock: monotonic milliseconds for the FSM and the deadlines, wall
    /// milliseconds beside them to recognise a suspend.
    clock: SystemClock,
    /// The suspend detector: samples both clocks once per turn.
    suspend: SuspendDetector,
    ping_nonce: u64,
    node_id: NodeId,
    local_hello: protocol::wire::Hello,
    peer_displays: BTreeMap<NodeId, (u64, Vec<protocol::wire::Display>)>,
    backend: String,
    trust: identity::TrustStore,
    /// Persists the ownership pair across restarts (`config.toml`).
    ownership_store: OwnershipStore,
    /// `[appearance]`: saved by `SetAppearance`, pushed live to the agent as `SetEffects`.
    appearance: config::AppearanceConfig,
    appearance_path: Option<std::path::PathBuf>,
    /// The edge-effect frames for the agent's overlay (rate-limited, ~30 Hz).
    edgefx: crate::edgefx::Emitter,
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
    /// Negotiated identities of currently established peer connections.
    layout_connections: HashMap<NodeId, [u8; 16]>,
    /// Set by `quit` on the control channel.
    stopping: bool,
    /// The agent reported `AgentLost` for the current crash and is being restarted.
    agent_restart_due: Option<Instant>,
    /// Recent losses and the circuit they open, measured in wall-clock time.
    agent_restart_circuit: RestartWatch,
    /// Exponential delay after unsuccessful restart attempts.
    agent_restart_backoff: Duration,
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
    /// The configured links the last rebuild left out because an endpoint display is not in
    /// any member's manifest. They are not an error: the link comes back on the next rebuild
    /// once the monitor is reported. Only the GUI and the log read this.
    inactive_links: Vec<crate::topology::InactiveLink>,
    /// Why the applied document cannot be built right now (`None` when it can). While set,
    /// no link is active — the GUI must say so instead of drawing the links as working.
    topology_error: Option<String>,
    /// The `[topology]` document to persist because it still names displays by a device name
    /// Windows has renumbered. Filled by [`Session::rebuild_topology`], taken and handed to the
    /// ordinary CAS path once by the main loop ([`Session::sync_display_ids`]).
    display_id_migration: Option<String>,
    /// The default layout to publish once, as `layout_toml`. Filled by
    /// [`Session::rebuild_topology`] when the applied document has no links and the workspace
    /// has exactly one peer, taken and handed to the ordinary CAS path by the main loop
    /// ([`Session::publish_default_layout`]).
    pending_default_layout: Option<String>,
    /// `[topology]` from `config.toml`: the links and the display snapshot the document
    /// is assembled from.
    layout: config::TopologyConfig,
    /// `input.pointer_gain`, applied to captured deltas while a source lease is remote.
    pointer_gain: f64,
    /// `input.edge_hysteresis_px` in logical units: the band a return disarms its edge by.
    edge_hysteresis_px: f64,
    /// The edge a return came back through, until the pointer leaves its band.
    edge_guard: Option<crate::topology::ReturnGuard>,
    transition_retry: transition_retry::TransitionRetry,
    pending_edge_guard: Option<crate::topology::ReturnGuard>,
    /// The last non-empty agent display report; empty reports never replace it.
    agent_displays: Vec<protocol::ipc::DisplayInfo>,
    /// The last captured local position: the input of an edge crossing in `Local`.
    local_pointer: Option<geometry::Position>,
    /// While a source lease is remote: the virtual position on the peer's display.
    remote_pointer: Option<geometry::Position>,
    /// The node that virtual position lives on (the lease peer, or the peer of a chain).
    remote_node: Option<NodeId>,
    /// A link the pointer left through *while remote*; applied once the lease is
    /// released, which is how a chain A→B→C is walked as two leases and no relay.
    pending_target: Option<(NodeId, geometry::Position)>,
    /// Whether the agent currently holds the local observation (`BeginCapture(OBSERVE)`).
    ///
    /// The FSM only asks for capture when a transition starts, but an edge has to be
    /// *noticed* before there is any transition: «OBSERVE: Local: watch edges, no
    /// suppression», so core arms the observation itself for the node that owns input.
    ///
    /// This is never a latch: the observation rides on the agent's capture, so every way
    /// that capture goes away clears it ([`Session::disarm_observation`] — the FSM's
    /// `EndCapture`/`ReleaseSession`, the agent's own `EmergencyFired`/
    /// `LocalInputDetected`/`Error` reports, an agent restart) and it is also cleared
    /// whenever this node stops being the owner or leaves `Local`. `arm_observation`
    /// re-arms it on the next tick while input is still ours, so the next crossing starts
    /// without a core restart.
    observe_armed: bool,
    /// Development-only permission to capture externally injected Windows input.
    dev_capture_injected: bool,
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
    /// Last second in which an input batch was logged at the agent boundary.
    last_agent_inject_log: Option<Instant>,
    /// Last second in which captured input forwarding was logged.
    last_input_batch_log: Option<Instant>,
    input_diagnostics: InputDiagnostics,
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
        edge_hysteresis_px: f64,
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
        dev_capture_injected: bool,
        pairing_gate: transport::PairingGate,
    ) -> Self {
        let layout_state = layout_store.restored();
        let (gui_requests_tx, gui_requests) = mpsc::channel(64);
        let (pairing_events_tx, pairing_events) = mpsc::channel(16);
        let gui_events = gui_server.publisher();
        let clock = SystemClock::new();
        let suspend = SuspendDetector::new(
            clock.monotonic_ms(),
            clock.wall_ms(),
            SUSPEND_THRESHOLD.as_millis() as u64,
        );
        let mut session = Session {
            router,
            manager,
            link_events,
            agent,
            ctl,
            timers: Timers::default(),
            refusal: None,
            context: routing::AgentContext::default(),
            clock,
            suspend,
            ping_nonce: 0,
            node_id,
            backend,
            trust,
            ownership_store,
            appearance: config::AppearanceConfig::default(),
            appearance_path: None,
            edgefx: crate::edgefx::Emitter::new(true),
            layout_state,
            layout_store,
            applied_wire: None,
            layout_editing_disabled: None,
            last_layout_error: None,
            layout_requests: HashMap::new(),
            next_layout_nonce: 0,
            layout_connections: HashMap::new(),
            stopping: false,
            agent_restart_due: None,
            agent_restart_circuit: RestartWatch::new(),
            agent_restart_backoff: RESTART_BACKOFF,
            agent_degraded_reason: String::new(),
            portal_failure: false,
            agent_lost_reported: false,
            topology: None,
            inactive_links: Vec::new(),
            topology_error: None,
            display_id_migration: None,
            pending_default_layout: None,
            layout,
            pointer_gain,
            edge_hysteresis_px,
            edge_guard: None,
            transition_retry: transition_retry::TransitionRetry::default(),
            pending_edge_guard: None,
            agent_displays: Vec::new(),
            local_hello,
            peer_displays: BTreeMap::new(),
            local_pointer: None,
            remote_pointer: None,
            remote_node: None,
            pending_target: None,
            observe_armed: false,
            dev_capture_injected,
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
            last_agent_inject_log: None,
            pairing_error: None,
            last_input_batch_log: None,
            input_diagnostics: InputDiagnostics::default(),
        };
        session.rebuild_topology();
        session
    }

    /// What the coordinator runs on every edit before applying it: the document must parse and
    /// the live manifests must be able to build it. The clone keeps the closure free of `self`,
    /// which `apply_edit` borrows.
    fn layout_checker(&self) -> impl Fn(&str) -> Result<config::TopologyConfig, String> {
        let node = *self.node_id.as_bytes();
        let agent = self.agent_displays.clone();
        let peers = self.peer_displays.clone();
        move |text| {
            let layout = parse_layout_toml(text)?;
            crate::topology::check_layout(&layout, &node, &agent, &peers)?;
            Ok(layout)
        }
    }

    /// Assembles the layout using the configured snapshot until a non-empty live report
    /// arrives for a member.
    fn rebuild_topology(&mut self) {
        let (displays, peers) = crate::topology::live_inputs(
            &self.layout,
            self.node_id.as_bytes(),
            &self.agent_displays,
            &self.peer_displays,
        );
        let documents = crate::topology::Documents {
            local_node_id: *self.node_id.as_bytes(),
            topology: &self.layout,
            local_displays: &displays,
            peer_displays: &peers,
        };
        match crate::topology::build(&documents) {
            Ok(built) => {
                let document = built.topology;
                self.router.set_topology_revision(document.revision);
                tracing::info!(
                    revision = document.revision,
                    links = document.links.len(),
                    inactive_links = built.inactive.len(),
                    nodes = document.nodes.len(),
                    "layout document assembled"
                );
                for link in &built.inactive {
                    for missing in &link.missing {
                        // Once per rebuild, never per sample: `rebuild_topology` runs on a
                        // display report or a topology change, so one monitor that is off
                        // produces one line here, not one per pointer event.
                        tracing::warn!(
                            link = %link.id,
                            side = ?missing.side,
                            node = %missing.node,
                            display = %missing.display,
                            "layout link inactive: its endpoint display is not reported"
                        );
                    }
                }
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
                // A workspace with no document at all starts from the peer to the right; the
                // publication itself is the owner's and goes through the ordinary CAS path
                // (`publish_default_layout`). A document that has links, however, is the user's
                // and is never replaced here.
                if self.router.ownership().is_owner() && self.layout_state.pending().is_none() {
                    let peers: Vec<NodeId> = document
                        .nodes
                        .iter()
                        .map(|node| node.node)
                        .filter(|node| *node != self.node_id)
                        .collect();
                    if let Some(layout) = crate::topology::default_layout(&self.layout, &peers) {
                        match toml::to_string_pretty(&layout) {
                            Ok(text) => self.pending_default_layout = Some(text),
                            Err(error) => {
                                tracing::warn!(%error, "cannot render the default layout")
                            }
                        }
                    }
                }
                self.topology = Some(document);
                self.inactive_links = built.inactive;
                self.topology_error = None;
                // A stored document that names a display by an old Windows device name still
                // works through the alias `build` applies; this makes the stored revision carry
                // the stable id too, through the ordinary CAS path (see `sync_display_ids`).
                let aliases = crate::topology::display_aliases(&displays);
                if !aliases.is_empty() {
                    if let Some(text) = crate::topology::alias_layout(
                        &self.layout,
                        self.node_id.as_bytes(),
                        &aliases,
                    ) {
                        if self.display_id_migration.as_deref() != Some(text.as_str()) {
                            self.display_id_migration = Some(text);
                        }
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "layout document unusable; edge-triggered transitions are disabled"
                );
                self.topology = None;
                self.inactive_links = Vec::new();
                self.topology_error = Some(error.to_string());
            }
        }
    }

    /// Hands a pending display-id migration to the ordinary layout CAS path.
    ///
    /// Windows renumbers `\\.\DISPLAYn` when monitors change, so an applied document can name a
    /// display by a name that no longer exists. `rebuild_topology` resolves such a name through
    /// the manifest, so the running document is already correct; this makes the *stored* one
    /// correct too, once, through the same whole-document CAS a GUI edit uses. The coordinator
    /// is the only writer (`layout_store` persists exactly what the CAS applied), so a node that
    /// is not the coordinator leaves the alias in effect and says so.
    async fn sync_display_ids(&mut self) {
        let Some(text) = self.display_id_migration.take() else {
            return;
        };
        if !self.router.ownership().is_owner() {
            tracing::debug!(
                "the layout names displays by renumbered Windows device names; the alias keeps \
                 it working until the coordinator rewrites the document"
            );
            return;
        }
        let expected = self.layout_state.applied();
        tracing::info!(
            expected_revision = expected.revision,
            "rewriting the layout's display ids to the ids the agent reports"
        );
        match self
            .layout_edit_document(text, expected, LayoutSource::AUTO)
            .await
        {
            Handled::Reply(ctl::Response::Error { code, detail }) => {
                tracing::warn!(%code, %detail, "the display-id rewrite was refused")
            }
            Handled::Reply(_) => {
                tracing::info!("layout display ids rewritten to the stable ids")
            }
            Handled::Deferred(_) => {}
        }
    }

    /// Publishes the workspace's starting layout once: the peer to the right of this desktop.
    ///
    /// Only the coordinator publishes a document (`docs/03-architecture.md`: the layout is
    /// authored by the owner alone), and only when [`Session::rebuild_topology`] found an
    /// applied document with no links and exactly one peer. It goes through the ordinary
    /// whole-document CAS ([`Session::layout_edit_document`]), so it takes a new revision like
    /// any other edit and the document it publishes can never replace one that has links.
    async fn publish_default_layout(&mut self) {
        let Some(text) = self.pending_default_layout.take() else {
            return;
        };
        if !self.router.ownership().is_owner() {
            return;
        }
        let peer = self
            .live_peers()
            .first()
            .copied()
            .map_or_else(|| "-".to_owned(), |peer| identity::fingerprint(&peer));
        tracing::info!("default layout published: {peer} to the right");
        let expected = self.layout_state.applied();
        match self
            .layout_edit_document(text, expected, LayoutSource::AUTO)
            .await
        {
            Handled::Reply(ctl::Response::Error { code, detail }) => {
                tracing::warn!(%code, %detail, "the default layout was refused")
            }
            Handled::Reply(_) => tracing::info!("layout follows the peers: default applied"),
            Handled::Deferred(_) => {}
        }
    }

    /// The peers the live document knows: every member other than this node.
    fn live_peers(&self) -> Vec<NodeId> {
        self.topology.as_ref().map_or_else(Vec::new, |topology| {
            topology
                .nodes
                .iter()
                .map(|node| node.node)
                .filter(|node| *node != self.node_id)
                .collect()
        })
    }

    /// `layout auto`: replace the applied document with the desktop-anchored default.
    ///
    /// The explicit action the owner takes once: a document whose links name monitors one by
    /// one keeps working as it is, and this replaces it with `self right` → `peer left`, both
    /// `display = "auto"`, through the coordinator's CAS — the same path a GUI edit takes, so
    /// it takes a new revision and reaches every member.
    async fn layout_auto(&mut self, source: LayoutSource) -> Handled {
        let peers = self.live_peers();
        let Some(mut layout) = crate::topology::auto_layout(&peers) else {
            return Handled::Reply(ctl::Response::Error {
                code: "BAD_REQUEST".to_owned(),
                detail: "layout auto needs exactly one peer in the workspace".to_owned(),
            });
        };
        layout.displays = self.layout.displays.clone();
        let text = match toml::to_string_pretty(&layout) {
            Ok(text) => text,
            Err(error) => {
                return Handled::Reply(ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail: format!("cannot render the default layout: {error}"),
                })
            }
        };
        let expected = self.layout_state.applied();
        self.layout_edit_document(text, expected, source).await
    }

    /// Seeds `[appearance]` and the config path `SetAppearance` saves to (the GUI never
    /// writes the file: this core is its single writer, under the config lock).
    pub fn set_appearance_source(
        &mut self,
        path: std::path::PathBuf,
        appearance: config::AppearanceConfig,
    ) {
        self.edgefx.set_enabled(appearance.edge_effects);
        self.appearance = appearance;
        self.appearance_path = Some(path);
    }

    /// The monotonic millisecond clock shared with the FSM.
    fn now_ms(&self) -> u64 {
        self.clock.monotonic_ms()
    }

    /// The wall-clock millisecond reading: what `now_ms` cannot account for across a
    /// suspend is the sleep.
    fn wall_ms(&self) -> u64 {
        self.clock.wall_ms()
    }

    /// Recognises a suspend, once per turn and before the step is acted on.
    ///
    /// A laptop that slept shows up as wall time having jumped far ahead of the monotonic
    /// clock. Everything core times on the monotonic clock was frozen with it, so the
    /// agent heartbeat is re-armed here: the freeze is not the agent's silence, and the
    /// loss it would otherwise produce is not a failure of the agent. Rule 6 is about
    /// *core* going quiet, not the machine going to sleep, and the sleep also cancels no
    /// lease on its own — `docs/05-protocol.md`: «Просыпание после suspend всегда отменяет
    /// lease», which the agent's own release and the FSM's deadlines carry out.
    fn observe_clock(&mut self) {
        let Some(slept_ms) = self.suspend.observe(self.now_ms(), self.wall_ms()) else {
            return;
        };
        tracing::warn!(
            slept_ms,
            "the machine was suspended (or this process was not running); re-arming the agent heartbeat"
        );
        self.agent.on_resume();
        self.agent_restart_circuit.on_suspend();
    }

    /// Keep peer silence separate from time core could not service its own loop.
    /// Carry the next expected wake across turn work as well as the select wait.
    fn observe_loop_wake(&mut self, planned_wake_ms: &mut u64) {
        let now = self.now_ms();
        if let Some(stalled_ms) =
            SuspendDetector::wake_lateness(*planned_wake_ms, now, self.agent.silence_limit_ms() / 2)
        {
            tracing::debug!("core loop stalled {stalled_ms} ms; not blaming the agent");
            self.agent.on_resume();
        }
        // An already-expired service timer is immediate work, not a late wake.
        *planned_wake_ms = self.next_deadline(now).max(now);
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

        let step_log_rate = logging::LogRateLimit::new();
        let now = self.now_ms();
        let mut planned_wake_ms = self.next_deadline(now).max(now);
        loop {
            self.observe_loop_wake(&mut planned_wake_ms);
            self.service_clock().await;
            if self.stopping {
                self.shutdown_agent().await;
                return Ok(());
            }

            // Service work can itself stall; observe it before replacing its wake plan.
            self.observe_loop_wake(&mut planned_wake_ms);
            let deadline = self.instant_of(planned_wake_ms);
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
            // Before the step is acted on: a resume re-arms the agent heartbeat, so the
            // liveness decision below cannot blame the agent for the machine's sleep.
            self.observe_clock();
            self.observe_loop_wake(&mut planned_wake_ms);
            let step_started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
            let link_backlog = self.link_events.len();

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
                    let now = self.now_ms();
                    let expired = prune_layout_requests(&mut self.layout_requests, now);
                    if expired > 0 {
                        tracing::debug!(expired, "layout requests whose verdict cannot arrive");
                    }
                }
            }
            // Include slow step processing, not just a late select wake: a buffered
            // agent response cannot be blamed on core's inability to service it.
            self.observe_loop_wake(&mut planned_wake_ms);
            if self.agent.is_attached()
                && (self.agent.child_exited() || self.agent.silence_exceeded())
            {
                tracing::warn!(
                    silence_ms = self.agent.silence_ms(),
                    "the agent stopped answering"
                );
                self.on_agent_lost().await;
            }
            // A stored layout that names displays by a renumbered Windows device name is
            // rewritten once, through the ordinary CAS path, by this node when it coordinates.
            self.sync_display_ids().await;
            // And a workspace with no layout at all gets its one default link, once.
            self.publish_default_layout().await;
            // Once per turn, and only what changed: the GUI's `Status` and, on a new revision,
            // the document its editor works on.
            self.publish_gui_state().await;
            let elapsed = step_started.map_or(Duration::ZERO, |start| start.elapsed());
            if elapsed >= Duration::from_millis(25)
                && tracing::enabled!(tracing::Level::DEBUG)
                && step_log_rate.allow()
            {
                tracing::debug!(
                    elapsed_ms = elapsed.as_millis(),
                    link_backlog,
                    "core step processing slow"
                );
            }
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

        // The circuit closes itself so a machine that slept through a bad minute is
        // retried: a breaker only the user can reset leaves the daemon degraded for ever
        // after one storm. The check is cheap and the loop wakes at least every
        // `IDLE_TICK`, so no deadline of its own is needed.
        if self.agent_restart_circuit.close_if_cooled(self.wall_ms()) {
            tracing::info!(
                cooldown_ms = RESTART_CIRCUIT_COOLDOWN.as_millis() as u64,
                "agent restart circuit closed after the cool-down; retrying the agent"
            );
            self.refusal = None;
            self.agent_degraded_reason.clear();
            self.agent_restart_backoff = RESTART_BACKOFF;
            self.agent_restart_due = Some(Instant::now());
        }

        if let Some(frame) = self.edgefx.due(now) {
            self.send_edgefx(Some(frame)).await;
        }
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
        if let Some(due) = self.edgefx.due_at() {
            deadline = deadline.min(due.max(now_ms));
        }
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
        tokio::time::Instant::from_std(self.clock.origin() + Duration::from_millis(deadline_ms))
    }

    /// Feeds the FSM and performs what it decided.
    async fn feed(&mut self, input: fr::Input) {
        let now = self.now_ms();
        let preparing_peer =
            self.router.lease().source().and_then(|lease| {
                (lease.state == fr::SourceState::Preparing).then_some(lease.peer)
            });
        let reason = preparing_peer.map(|_| match &input {
            fr::Input::Net {
                message: fr::Message::ProtocolError { code, .. },
                ..
            }
            | fr::Input::Agent(fr::AgentEvent::Error { code, .. }) => format!("{code:?}"),
            _ => "переход не завершён".to_owned(),
        });
        let outputs = self.router.handle(now, input);
        if let Some(peer) = preparing_peer {
            if self.router.focus() == fr::FocusState::Remote {
                self.transition_retry.success(&peer);
                self.pending_edge_guard = None;
            } else if self.router.focus() != fr::FocusState::Preparing {
                self.edge_guard = self.pending_edge_guard.take().or(self.edge_guard);
                let reason = reason.unwrap_or_default();
                if let Some(delay_ms) = self.transition_retry.fail(peer, now, reason.clone()) {
                    tracing::warn!(%peer, delay_ms, %reason, "transition retry backoff advanced");
                }
            }
        }
        // Boxed: `apply` reaches `feed` again through the agent-loss path.
        Box::pin(self.apply(outputs)).await;
    }

    fn transition_disabled_reason(&self) -> String {
        self.transition_retry
            .reasons()
            .map(|(peer, reason)| {
                let name = self
                    .trust
                    .get(peer)
                    .map_or_else(|| peer.to_string(), |record| record.display_name.clone());
                format!("Переход на {name} временно отключён: {reason}")
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Adds the dev-only capture permission only to a source owner's BeginCapture.
    ///
    /// This ownership check is the boundary that makes the flag inert on receivers.
    fn agent_frame(&self, command: &fr::AgentCommand) -> protocol::ipc::agent_frame::Msg {
        let mut frame = routing::agent_command(command);
        if let Msg::BeginCapture(begin) = &mut frame {
            begin.dev_capture_injected = source_owner_may_capture_injected(
                self.dev_capture_injected,
                self.router.is_owner(),
            );
        }
        frame
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
                    self.edgefx_for_command(&command).await;
                    if matches!(
                        command,
                        fr::AgentCommand::EndCapture { .. }
                            | fr::AgentCommand::ReleaseSession { .. }
                    ) {
                        // The capture the observation rode on is being torn down (a source's
                        // `EndCapture`, a receiver's `ReleaseSession`); forget it so the next
                        // tick can re-arm while input is still ours.
                        self.disarm_observation();
                    }
                    let frame = self.agent_frame(&command);
                    if let fr::AgentCommand::Inject {
                        lease_id,
                        input_batch_seq,
                        events,
                    } = &command
                    {
                        self.input_diagnostics.for_lease(*lease_id);
                        for event in events {
                            self.input_diagnostics.inject.record(event);
                        }
                        let report = self
                            .last_agent_inject_log
                            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
                        if report {
                            self.last_agent_inject_log = Some(Instant::now());
                            let mut keys = 0;
                            let mut pointers = 0;
                            let mut buttons = 0;
                            let mut scrolls = 0;
                            for event in events {
                                match event {
                                    InputEvent::Key { .. } => keys += 1,
                                    InputEvent::PointerAbsolute(_)
                                    | InputEvent::PointerRelative { .. } => pointers += 1,
                                    InputEvent::Button { .. } => buttons += 1,
                                    InputEvent::Scroll { .. } => scrolls += 1,
                                }
                            }
                            tracing::info!(
                                lease_id = ?lease_id,
                                input_batch_seq,
                                events = events.len(),
                                keys,
                                pointers,
                                buttons,
                                scrolls,
                                "core -> agent Inject"
                            );
                        }
                    }
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
                Effect::LeaseDiagnostic(snapshot) => {
                    // FSM totals are per lease; socket totals are per connection. Snapshot
                    // before teardown so the final line retains the live transport counters.
                    let transport = self.manager.lease_diagnostics(&snapshot.peer).await;
                    self.input_diagnostics.for_lease(snapshot.lease_id);
                    tracing::info!(
                        peer = %snapshot.peer,
                        lease_id = %lease_hex(&snapshot.lease_id),
                        role = ?snapshot.role,
                        ttl_ms = snapshot.ttl_ms,
                        remaining_ms = snapshot.remaining_ms,
                        final_snapshot = snapshot.final_snapshot,
                        fsm = ?snapshot.counters,
                        inbound_ages_ms = ?snapshot.ages,
                        transport_connection = ?transport,
                        hook_edges = ?self.input_diagnostics.hook,
                        captured_edges = ?self.input_diagnostics.captured,
                        inject_edges = ?self.input_diagnostics.inject,
                        eis_applied_edges = ?self.input_diagnostics.applied,
                        "lease liveness diagnostics"
                    );
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
        if let Err(error) = self.agent.send(frame).await {
            tracing::warn!(%error, "cannot reach the agent");
            self.on_agent_lost().await;
        }
    }
    async fn query_agent_displays(&mut self) -> bool {
        let frame = Msg::QueryDisplays(protocol::ipc::QueryDisplays {});
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
                static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                if tracing::enabled!(tracing::Level::DEBUG)
                    && !matches!(
                        message,
                        fr::Message::LeaseChallenge { .. }
                            | fr::Message::LeaseRenew { .. }
                            | fr::Message::LeaseStatus { .. }
                    )
                    && LOG_RATE.allow()
                {
                    tracing::debug!(peer = %peer, message = name, "core -> peer");
                }
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
        let Some(connection_id) = self.layout_connections.get(&from).copied() else {
            tracing::warn!(peer = %from, "ignoring layout message without an established connection");
            return;
        };
        let source = LayoutSource::remote(from, connection_id);

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
                if displays.is_empty() {
                    return;
                }
                self.peer_displays.insert(from, (revision, displays));
                self.rebuild_topology();
            }
            LayoutMessage::Topology(document) => {
                let Some(pair) = crate::topology::wire_pair(&document) else {
                    tracing::warn!(peer = %from, "topology without a readable author id");
                    return;
                };
                let parsed = crate::topology::config_from_wire(&document, self.node_id);
                let validation = crate::topology::validate_wire(&document)
                    .and_then(|()| parsed.as_ref().map(|_| ()).map_err(Clone::clone));
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
                        let layout = parsed.expect("accepted topology was converted");
                        self.install_applied_layout(layout, pair, source);
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
                            self.republish_layout().await;
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
                let base = match <[u8; 32]>::try_from(edit.expected_author.as_slice()) {
                    Ok(bytes) => {
                        crate::layout::Pair::new(edit.expected_revision, NodeId::new(bytes))
                    }
                    Err(_) => crate::layout::Pair::NONE,
                };
                let applied = self.layout_state.applied();
                if !self.router.ownership().is_owner() {
                    tracing::warn!(
                        peer = %from,
                        expected_revision = base.revision,
                        expected_author = ?base.author.map(|id| identity::fingerprint(&id)),
                        applied_revision = applied.revision,
                        applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                        owner = ?self.router.ownership().owner(),
                        "layout edit refused"
                    );
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
                let editor = NodeId::new(
                    edit.author_node_id
                        .clone()
                        .try_into()
                        .unwrap_or(*from.as_bytes()),
                );
                let layout_toml = String::from_utf8_lossy(&edit.layout_toml).into_owned();
                let checker = self.layout_checker();
                let mut candidate = None;
                let outcome = self.layout_state.apply_edit(
                    &crate::layout::Edit {
                        expected: base,
                        editor,
                        layout_toml: layout_toml.clone(),
                    },
                    self.node_id,
                    true,
                    |text| {
                        candidate = Some(checker(text)?);
                        Ok(())
                    },
                );
                match outcome {
                    crate::layout::EditOutcome::Applied(pair) => {
                        self.layout_editing_disabled = None;
                        self.last_layout_error = None;
                        let layout = candidate.expect("applied layout was parsed");
                        let document = self.install_applied_layout(layout, pair, source);
                        self.send_layout_to(
                            from,
                            transport::convert::LayoutMessage::TopologyApplied {
                                revision: pair.revision,
                                request_nonce: edit.request_nonce,
                            },
                        )
                        .await;
                        self.publish_layout(document).await;
                    }
                    crate::layout::EditOutcome::Conflict(current) => {
                        tracing::warn!(
                            peer = %from,
                            expected_revision = base.revision,
                            expected_author = ?base.author.map(|id| identity::fingerprint(&id)),
                            applied_revision = current.revision,
                            applied_author = ?current.author.map(|id| identity::fingerprint(&id)),
                            "layout edit refused"
                        );
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
                let expected = self
                    .layout_requests
                    .get(&request_nonce)
                    .map(|request| request.expected);
                let applied = self.layout_state.applied();
                let detail = refusal_detail(code, layout_revision);
                self.last_layout_error = Some((code.as_str_name().to_owned(), detail.clone()));
                tracing::warn!(
                    peer = %from,
                    code = code.as_str_name(),
                    expected_revision = ?expected.map(|pair| pair.revision),
                    expected_author = ?expected
                        .and_then(|pair| pair.author)
                        .map(|id| identity::fingerprint(&id)),
                    local_applied_revision = applied.revision,
                    local_applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                    coordinator_applied_revision = layout_revision,
                    detail,
                    "layout edit refused"
                );
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

    /// Makes the accepted document the configuration source for routing, GUI, and persistence.
    fn install_applied_layout(
        &mut self,
        mut layout: config::TopologyConfig,
        pair: crate::layout::Pair,
        source: LayoutSource,
    ) -> protocol::wire::Topology {
        let author = pair.author.unwrap_or(self.node_id);
        let max_seen = self.layout_state.max_seen();
        layout.revision = pair.revision;
        layout.author = identity::fingerprint(&author);
        layout.max_seen_revision = max_seen.revision;
        layout.max_seen_author = max_seen
            .author
            .map_or_else(String::new, |id| identity::fingerprint(&id));
        self.layout = layout;
        self.layout_store.observe(&self.layout_state, &self.layout);
        self.router.set_topology_revision(pair.revision);
        self.rebuild_topology();

        let document = match &self.topology {
            Some(live) => crate::topology::wire_from_live(
                live,
                &self.layout,
                pair.revision,
                author,
                self.node_id,
            ),
            None => {
                crate::topology::wire_document(&self.layout, pair.revision, author, self.node_id)
            }
        };
        self.applied_wire = Some(document.clone());
        tracing::info!(
            source = source.source,
            connection_id = ?source.connection_id.map(u128::from_be_bytes),
            peer = ?source.peer,
            revision = pair.revision,
            author = %author,
            "layout applied by the coordinator"
        );
        document
    }

    /// Republishes a document the coordinator has just adopted as `max_seen + 1` under itself.
    ///
    /// A divergent local edit stays pending: it was based on the revision this document
    /// supersedes, so applying it would silently revert the other side's work.
    async fn republish_layout(&mut self) {
        if self.layout_state.pending().is_some() {
            tracing::warn!(
                "a pending layout edit stays aside while the winning revision is republished"
            );
        }
        let published = self.layout_state.republish(self.node_id);
        let layout = self.layout.clone();
        let wire = self.install_applied_layout(layout, published, LayoutSource::AUTO);
        tracing::info!(
            source = "auto",
            connection_id = tracing::field::Empty,
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
        expected: crate::layout::Pair,
    ) -> (u64, oneshot::Receiver<ctl::Response>) {
        let now = self.now_ms();
        prune_layout_requests(&mut self.layout_requests, now);
        // `.max(1)` keeps 0 out of the wire: it means «this frame is not a request».
        self.next_layout_nonce = self.next_layout_nonce.wrapping_add(1).max(1);
        let nonce = self.next_layout_nonce;
        let (verdict, receiver) = oneshot::channel();
        self.layout_requests.insert(
            nonce,
            LayoutRequest {
                layout_toml,
                expected,
                verdict,
                at_ms: self.now_ms(),
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
    ///
    /// `base` is the pair the editor wrote the document against (audit F1): the slot keeps it
    /// for the later `layout apply`, which re-sends the edit through the coordinator's CAS.
    fn hold_layout_pending(&mut self, base: crate::layout::Pair, layout_toml: &str) {
        self.layout_state.hold_pending(base, layout_toml.to_owned());
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
    async fn layout_apply(&mut self, source: LayoutSource) -> Handled {
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
            let applied = self.layout_state.applied();
            let Some(owner) = self.router.ownership().owner().copied() else {
                tracing::warn!(
                    expected_revision = base_pair.revision,
                    expected_author = ?base_pair.author.map(|id| identity::fingerprint(&id)),
                    applied_revision = applied.revision,
                    applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                    "layout edit refused"
                );
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the pending edit waits".to_owned(),
                });
            };
            tracing::info!(
                owner = %owner,
                expected_revision = base_pair.revision,
                expected_author = ?base_pair.author.map(|id| identity::fingerprint(&id)),
                applied_revision = applied.revision,
                applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                "forwarding layout edit to coordinator"
            );
            let (nonce, verdict) = self.hold_layout_request(layout_toml.clone(), base_pair);
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
        let checker = self.layout_checker();
        let mut candidate = None;
        let outcome = self
            .layout_state
            .apply_edit(&edit, self.node_id, true, |text| {
                candidate = Some(checker(text)?);
                Ok(())
            });
        Handled::Reply(match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                self.release_layout_pending();
                let layout = candidate.expect("applied pending layout was parsed");
                let document = self.install_applied_layout(layout, pair, source);
                self.publish_layout(document).await;
                ctl::Response::Status(format!("applied revision={}", pair.revision))
            }
            crate::layout::EditOutcome::Conflict(current) => {
                tracing::warn!(
                    expected_revision = edit.expected.revision,
                    expected_author = ?edit.expected.author.map(|id| identity::fingerprint(&id)),
                    applied_revision = current.revision,
                    applied_author = ?current.author.map(|id| identity::fingerprint(&id)),
                    "layout edit refused"
                );
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
    async fn layout_edit(&mut self, path: &str, source: LayoutSource) -> Handled {
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
        self.layout_edit_document(document_text, applied, source)
            .await
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
        source: LayoutSource,
    ) -> Handled {
        let is_coordinator = self.router.ownership().is_owner();
        if let Some(reason) = self.layout_editing_disabled.clone() {
            self.hold_layout_pending(expected, &document_text);
            return Handled::Reply(ctl::Response::Error {
                code: "NO_COORDINATOR".to_owned(),
                detail: format!("{reason}; the edit is kept as pending"),
            });
        }
        if !is_coordinator {
            // A follower sends the edit to the coordinator and answers with the verdict that
            // comes back; only a partition keeps it pending (`docs/05-protocol.md`).
            let applied = self.layout_state.applied();
            let Some(owner) = self.router.ownership().owner().copied() else {
                self.hold_layout_pending(expected, &document_text);
                tracing::warn!(
                    expected_revision = expected.revision,
                    expected_author = ?expected.author.map(|id| identity::fingerprint(&id)),
                    applied_revision = applied.revision,
                    applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                    "layout edit refused"
                );
                return Handled::Reply(ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the edit is kept as pending".to_owned(),
                });
            };
            // The edit occupies the pending slot until the verdict says it was applied, so a
            // refusal or a lost answer leaves the edit — slot and file — for the user's reload.
            self.hold_layout_pending(expected, &document_text);
            tracing::info!(
                owner = %owner,
                expected_revision = expected.revision,
                expected_author = ?expected.author.map(|id| identity::fingerprint(&id)),
                applied_revision = applied.revision,
                applied_author = ?applied.author.map(|id| identity::fingerprint(&id)),
                "forwarding layout edit to coordinator"
            );
            let (nonce, verdict) = self.hold_layout_request(document_text.clone(), expected);
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
        let checker = self.layout_checker();
        let mut candidate = None;
        let outcome = self
            .layout_state
            .apply_edit(&edit, self.node_id, is_coordinator, |text| {
                candidate = Some(checker(text)?);
                Ok(())
            });
        Handled::Reply(match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                let layout = candidate.expect("applied layout was parsed");
                let document = self.install_applied_layout(layout, pair, source);
                self.publish_layout(document).await;
                ctl::Response::Status(format!("applied revision={}", pair.revision))
            }
            crate::layout::EditOutcome::Conflict(current) => {
                tracing::warn!(
                    expected_revision = edit.expected.revision,
                    expected_author = ?edit.expected.author.map(|id| identity::fingerprint(&id)),
                    applied_revision = current.revision,
                    applied_author = ?current.author.map(|id| identity::fingerprint(&id)),
                    "layout edit refused"
                );
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
                if self.transition_retry.reasons().any(|(failed_peer, _)| {
                    failed_peer == peer
                        || (*peer == self.node_id && *code == fr::ErrorCode::CaptureFailed)
                }) {
                    tracing::debug!(peer = %peer, ?code, "peer refused during transition backoff");
                } else {
                    tracing::warn!(peer = %peer, ?code, "peer refused");
                }
            }
            fr::Notice::LeaseMismatch {
                peer,
                received,
                expected,
            } => tracing::warn!(
                peer = %peer,
                received = %lease_hex(received),
                expected = %expected.as_ref().map_or_else(|| "none".to_owned(), lease_hex),
                "lease mismatch"
            ),
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
                self.layout_connections.insert(peer, connection_id);
                tracing::info!(
                    peer = %peer,
                    name = hello.display_name.as_str(),
                    can_own = hello.can_own,
                    receiver_ready = hello.capabilities.receiver_ready(),
                    "link up"
                );
                if hello.displays_revision > 0 && !hello.displays.is_empty() {
                    self.peer_displays
                        .insert(peer, (hello.displays_revision, hello.displays.clone()));
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
                // `EnterPrepare` with `NOT_OWNER`. Every node states its current view
                // on link-up, including a follower that remembers a newer serial for
                // this owner. Only the actual owner marks that state as granted.
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
                if let Some(owner) = ownership.owner().copied() {
                    self.send_to(
                        peer,
                        fr::Message::OwnerState {
                            serial: ownership.serial(),
                            owner: Some(owner),
                            granted: ownership.is_owner(),
                        },
                    )
                    .await;
                }
            }
            LinkEvent::Message { peer, message } => match message {
                transport::convert::Inbound::Focus(message) => {
                    static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                    if tracing::enabled!(tracing::Level::DEBUG)
                        && !matches!(
                            message,
                            fr::Message::LeaseChallenge { .. }
                                | fr::Message::LeaseRenew { .. }
                                | fr::Message::LeaseStatus { .. }
                        )
                        && LOG_RATE.allow()
                    {
                        tracing::debug!(
                            peer = %peer,
                            message = routing::message_name(&message),
                            "peer -> core"
                        );
                    }
                    // Dry fault injection only: a live link whose receiver refuses
                    // every prepare, without touching desktop input.
                    if self.agent.is_dry()
                        && std::env::var_os("LWS_DRY_REFUSE_PREPARE").is_some()
                        && matches!(message, fr::Message::EnterPrepare { .. })
                    {
                        self.send_to(
                            peer,
                            fr::Message::ProtocolError {
                                code: fr::ErrorCode::CaptureFailed,
                                related_seq: 0,
                            },
                        )
                        .await;
                        return;
                    }
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
                self.layout_connections.remove(&peer);
                tracing::warn!(peer = %peer, ?reason, "link down");
                if let Some(clipboard) = self.clipboard.as_mut() {
                    clipboard.on_link_down(peer).await;
                }
                // The manifest of a peer that just went away is the *last known* one, and it
                // stays: the map keeps showing the computer, offline, with the screens it had.
                // Only a revoke forgets it (there the peer itself is gone).
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
        // marked it on the heartbeat, so nothing to do here but handle it. It also
        // clears the "we just woke up" flag: an agent that answers is alive, and a later
        // loss is its own.
        self.agent_restart_circuit.on_frame();
        let Some(message) = frame.msg else {
            return Ok(());
        };
        match (&message, self.router.lease().receiver()) {
            (Msg::InjectResult(result), Some(lease))
                if result.lease_id.as_slice() == lease.lease_id.as_slice() =>
            {
                self.input_diagnostics.for_lease(lease.lease_id);
                self.input_diagnostics.applied = result.applied_edges;
            }
            _ => {}
        }
        // The observation rides on the agent's capture, and these frames are the agent
        // saying that capture is gone: the emergency combination and a receiver takeover
        // release everything before reporting, and a capture failure or overflow ends it.
        // When core is already `Local` the FSM emits no `EndCapture` for any of them, so
        // this is the only place that can forget the observation. The next tick re-arms it
        // while input is still ours, which is what makes the next crossing work.
        if matches!(
            message,
            Msg::Emergency(_) | Msg::LocalInput(_) | Msg::Error(_)
        ) {
            self.disarm_observation();
        }
        static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
        if tracing::enabled!(tracing::Level::DEBUG) && LOG_RATE.allow() {
            tracing::debug!(frame = routing::agent_frame_name(&message), "agent -> core");
        }
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
                    if !self.portal_failure && !self.agent_restart_circuit.is_open() {
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
            AgentSignal::Captured(batch) => {
                if let Some(lease) = self.router.lease().source() {
                    if lease.state == fr::SourceState::Remote {
                        self.input_diagnostics.for_lease(lease.lease_id);
                        self.input_diagnostics.hook = batch.hook_edges;
                        for event in batch.events.iter().filter_map(routing::input_event_of) {
                            self.input_diagnostics.captured.record(&event);
                        }
                    }
                }
                if batch.at_edge {
                    self.on_portal_activation(batch.activation.as_ref()).await;
                    // The activation seeds local_pointer itself. Do not treat the same
                    // pointer as a second Windows-style motion sample or forward it.
                    return Ok(());
                }
                let events = batch.events;
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
                if displays.is_empty() {
                    self.rebuild_topology();
                    return Ok(());
                }
                let revision = if report.local_revision > self.local_hello.displays_revision {
                    report.local_revision
                } else if displays != self.local_hello.displays {
                    self.local_hello.displays_revision.saturating_add(1)
                } else {
                    self.local_hello.displays_revision
                };
                self.agent_displays = report.displays;
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
                crate::topology::capture_position(
                    topology,
                    &self.node_id,
                    pointer.x,
                    pointer.y,
                    self.local_pointer.as_ref(),
                    remote,
                )
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

    async fn release_portal_capture(&mut self, end: protocol::ipc::EndCapture) {
        self.observe_armed = false;
        if let Err(error) = self.agent.send(Msg::EndCapture(end)).await {
            tracing::warn!(%error, "could not return the portal activation to local input");
        }
    }

    async fn on_portal_activation(&mut self, activation: Option<&protocol::ipc::PortalActivation>) {
        // A repeated activation must not replace an owner lease already preparing or
        // forwarding. Followers always release their native capture, even as receivers.
        if self.router.is_owner() && self.router.focus() != fr::FocusState::Local {
            return;
        }
        match portal_activation_plan(
            self.topology.as_ref(),
            &self.node_id,
            self.router.is_owner(),
            self.router.focus(),
            activation,
        ) {
            Ok((position, transition)) => {
                if self.manager.handle(&transition.target.node).await.is_none()
                    || self
                        .transition_retry
                        .blocked(&transition.target.node, self.now_ms())
                {
                    self.release_portal_capture(protocol::ipc::EndCapture {
                        lease_id: Vec::new(),
                        replay_buffered_locally: true,
                    })
                    .await;
                    return;
                }
                self.local_pointer = Some(position);
                self.remote_pointer = Some(transition.target_position.clone());
                self.remote_node = Some(transition.target.node);
                self.feed(fr::Input::Local(fr::LocalInput::SwitchRequest {
                    trigger: fr::SwitchTrigger::Edge,
                    target: transition.target.node,
                    entry_position: transition.target_position,
                    topology_revision: self.router.topology_revision(),
                }))
                .await;
                // The FSM can reject a crossing (held buttons, missing capabilities,
                // offline target). Native activation still needs a local return.
                if self.router.lease().source().is_none() {
                    self.remote_pointer = None;
                    self.remote_node = None;
                    tracing::info!("portal crossing refused; returning input locally");
                    self.release_portal_capture(protocol::ipc::EndCapture {
                        lease_id: Vec::new(),
                        replay_buffered_locally: true,
                    })
                    .await;
                }
            }
            Err(end) => {
                tracing::info!(
                    owner = self.router.is_owner(),
                    has_position =
                        activation.is_some_and(|activation| activation.position.is_some()),
                    "portal activation has no owner-local configured link; releasing capture"
                );
                self.release_portal_capture(end).await;
            }
        }
    }

    /// Sends one edge-effect frame. Cosmetic: a failure is the agent loss path's business.
    async fn send_edgefx(&mut self, frame: Option<protocol::ipc::EdgeFx>) {
        if let Some(frame) = frame {
            let _ = self.agent.send(Msg::EdgeFx(frame)).await;
        }
    }

    /// One pointer sample on the owner while `Local`: at most ~30 Hz of work, and only a
    /// frame when the pointer is within reach of a linked edge.
    async fn sample_edgefx(&mut self, position: &geometry::Position) {
        let now = self.now_ms();
        if !self.edgefx.wants_sample(now) {
            return;
        }
        let near = self
            .topology
            .as_ref()
            .and_then(|topology| crate::edgefx::proximity(topology, &self.node_id, position));
        let frame = match near {
            Some((update, display)) => self.edgefx.push(update, &display, now),
            None => self.edgefx.out_of_reach(now),
        };
        self.send_edgefx(frame).await;
    }

    /// The edge effects a command to the agent implies: `Crossed` when this node starts
    /// sending input away, `Entered` at the first pointer position of a receiver lease,
    /// `Left` when either lease ends.
    async fn edgefx_for_command(&mut self, command: &fr::AgentCommand) {
        let now = self.now_ms();
        let update = match command {
            fr::AgentCommand::BeginCapture {
                kind: input_model::CaptureKind::Remote,
                ..
            } => Some((edgefx::Update::Crossed(None), String::new())),
            fr::AgentCommand::PrepareInject { .. } => {
                self.edgefx.await_entry();
                None
            }
            fr::AgentCommand::Inject { events, .. } => {
                if !events
                    .iter()
                    .any(|event| matches!(event, InputEvent::PointerAbsolute(_)))
                    || !self.edgefx.take_entry_wanted()
                {
                    return;
                }
                let first = events.iter().find_map(|event| match event {
                    InputEvent::PointerAbsolute(position) => Some(position),
                    _ => None,
                });
                match (first, self.topology.as_ref()) {
                    (Some(position), Some(topology)) => crate::edgefx::entry_in_desktop(
                        topology,
                        &self.node_id,
                        position.x,
                        position.y,
                    ),
                    _ => None,
                }
            }
            fr::AgentCommand::EndCapture { .. } | fr::AgentCommand::ReleaseSession { .. } => {
                self.edgefx.cancel_entry();
                Some((edgefx::Update::Left, String::new()))
            }
            _ => None,
        };
        if let Some((update, display)) = update {
            let frame = self.edgefx.push(update, &display, now);
            self.send_edgefx(frame).await;
        }
    }

    /// Asks the agent to watch the local pointer, once, for the node that owns input.
    ///
    /// An observation has no lease and no suppression, so its `lease_id` is empty — which
    /// is also why the FSM ignores the `CaptureReady` that answers it
    /// (`routing::agent_signal`: an empty lease id is not a transition).
    async fn arm_observation(&mut self) {
        if !self.router.is_owner() || self.router.focus() != fr::FocusState::Local {
            // Somewhere else the observation is not ours to hold: a transition replaces
            // the capture, and losing ownership takes the local pointer away. Forgetting
            // it here is what makes the next return to `Local` (or regaining ownership)
            // arm a fresh one rather than latch a stale `true`.
            self.disarm_observation();
            return;
        }
        if self.observe_armed {
            return;
        }
        let command = fr::AgentCommand::BeginCapture {
            kind: input_model::CaptureKind::Observe,
            // An observation has no lease; `agent_command` sends an empty `lease_id` for
            // it, so its `CaptureReady` answer is ignored by the FSM.
            lease_id: [0u8; 16],
            self_timeout_ms: 0,
        };
        let frame = self.agent_frame(&command);
        match self.agent.send(frame).await {
            Ok(()) => {
                self.observe_armed = true;
                tracing::info!("asked the agent to observe the local pointer");
            }
            Err(error) => tracing::warn!(%error, "cannot arm the local observation"),
        }
    }

    /// Forgets the armed observation: the agent's capture it rode on is gone.
    ///
    /// Called from every path that tears that capture down, so `observe_armed` follows the
    /// agent's actual capture state instead of latching `true` forever (which left the
    /// next crossing unable to start until core was restarted).
    fn disarm_observation(&mut self) {
        if self.observe_armed {
            self.observe_armed = false;
            tracing::debug!("the local observation was torn down; it re-arms while we own input");
        }
    }

    /// Arms the band of the edge the pointer just came home through, and places the local
    /// pointer inside it.
    ///
    /// `step` is the scaled captured delta that carried the pointer out of the peer's
    /// display: its dominant axis and sign are the edge, and `input.edge_hysteresis_px` is
    /// the band. Without this a pointer that came home sitting on the edge resolved a fresh
    /// crossing on the next sample — the `Local`/`Remote` oscillation at the boundary.
    fn arm_return_guard(&mut self, step: geometry::Point, entry: &geometry::Position) {
        // The step's sign is the direction the pointer came home *in*, which is the inward
        // direction on this side; the entry point is where it came home.
        let armed_at = if step.x.abs() >= step.y.abs() {
            entry.x
        } else {
            entry.y
        };
        let Some(guard) =
            crate::topology::ReturnGuard::after_return(step, armed_at, self.edge_hysteresis_px)
        else {
            return;
        };
        self.local_pointer = Some(guard.entry(entry));
        self.edge_guard = Some(guard);
        tracing::debug!(
            axis = ?guard.axis(),
            hysteresis = guard.hysteresis(),
            "the edge stays disarmed until the pointer leaves its band"
        );
    }

    /// `Local`: a step that leaves a display through a link endpoint opens the transition.
    async fn track_local(&mut self, position: &geometry::Position, motion: geometry::Point) {
        // A pointer still inside the band of the edge it came home through is not a
        // crossing, however hard it is pushed at that edge (`input.edge_hysteresis_px`).
        if let Some(guard) = self.edge_guard {
            if guard.holds(position) {
                return;
            }
            self.edge_guard = None;
        }
        // Only the owner opens leases, so only the owner reacts to an edge; a follower
        // watches its own pointer without acting on it.
        if !self.router.is_owner() || self.router.focus() != fr::FocusState::Local {
            return;
        }
        self.sample_edgefx(position).await;
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
        if self.manager.handle(&target).await.is_none()
            || self.transition_retry.blocked(&target, self.now_ms())
        {
            return;
        }
        let inward = geometry::Point::new(-motion.x, -motion.y);
        let armed_at = if motion.x.abs() >= motion.y.abs() {
            position.x
        } else {
            position.y
        };
        self.pending_edge_guard =
            crate::topology::ReturnGuard::after_return(inward, armed_at, self.edge_hysteresis_px);
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
        if self.router.focus() != fr::FocusState::Preparing {
            self.edge_guard = self.pending_edge_guard.take().or(self.edge_guard);
        }
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
        let (advanced, crossing) = crate::topology::remote_step(topology, &node, &position, step);
        let Some(transition) = crossing else {
            self.remote_pointer = Some(advanced);
            return;
        };
        let target = transition.target.node;
        let entry_position = transition.target_position;
        let topology_revision = self.router.topology_revision();
        let mut return_effect = None;
        if target == self.node_id {
            // The reverse direction of the link we came in on: back to Local, with the
            // reason the FSM takes from the trigger (`RETURN_EDGE`).
            tracing::info!("the pointer came back over the entry edge; returning to Local");
            self.remote_pointer = None;
            self.remote_node = None;
            // Disarm the edge the pointer came home through until it leaves the band: a
            // pointer that came home sitting on the edge would otherwise resolve a fresh
            // crossing on the next sample (see `arm_return_guard`).
            self.arm_return_guard(step, &entry_position);
            return_effect = self.topology.as_ref().and_then(|topology| {
                crate::edgefx::entry(
                    topology,
                    &self.node_id,
                    &entry_position.display_id,
                    entry_position.x,
                    entry_position.y,
                )
            });
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
        if let Some((update, display)) = return_effect {
            let frame = self.edgefx.push(update, &display, self.now_ms());
            self.send_edgefx(frame).await;
        }
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
        let report = self
            .last_input_batch_log
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
        let counts = report.then(|| input_event_counts(&events));
        let lease = match self.router.lease().source() {
            Some(lease) if lease.state == fr::SourceState::Remote => (lease.peer, lease.lease_id),
            _ => {
                if report {
                    let (keys, pointers, buttons, scrolls) = counts.unwrap_or_default();
                    self.last_input_batch_log = Some(Instant::now());
                    tracing::debug!(
                        events = events.len(),
                        keys,
                        pointers,
                        buttons,
                        scrolls,
                        "captured input outside a remote source lease; dropped"
                    );
                }
                return;
            }
        };
        let (peer, lease_id) = lease;
        // A Remote lease with no estimate (a manual switch that could not resolve a display, a
        // chained link, a layout that became unusable) still has to send positions the
        // receiver can use: seed the estimate on its primary display and integrate the
        // captured deltas from there (batch-2 audit).
        if self.remote_pointer.is_none() {
            self.remote_pointer = self.receiver_start_position(peer);
            self.remote_node = Some(peer);
            match self.remote_pointer.as_ref() {
                Some(seed) => tracing::warn!(
                    peer = %peer,
                    display = seed.display_id.as_str(),
                    "a remote lease had no estimate; it was seeded on the peer's primary display"
                ),
                None => tracing::warn!(
                    peer = %peer,
                    "a remote lease has no estimate and no known display for the peer"
                ),
            }
        }
        // The peer receives *its* display's local coordinates: the tracker's virtual
        // position, not the source's node-space numbers (see `topology::forward_events`).
        let events = crate::topology::forward_events(events, self.remote_pointer.as_ref());
        if report {
            let (keys, pointers, buttons, scrolls) = counts.unwrap_or_default();
            self.last_input_batch_log = Some(Instant::now());
            tracing::info!(
                peer = %peer,
                message = "InputBatch",
                events = events.len(),
                keys,
                pointers,
                buttons,
                scrolls,
                "core -> peer"
            );
        }
        let message = fr::Message::InputBatch {
            lease_id,
            // Stamped over by the transport (the wire defines the batch sequence as
            // the envelope sequence).
            input_batch_seq: 0,
            events,
        };
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
        self.edgefx.set_enabled(self.appearance.edge_effects);
        // The agent process is going away with whatever capture it held: a restarted agent
        // starts with none, so the observation must be armed again once it is back.
        self.disarm_observation();
        // The agent's last frames: the portal grants go where they always did, and nothing
        // else is thrown away (audit F7) — a `Released`/`DeadlineExpired` in the tail is what
        // finishes a release the FSM is waiting for, instead of the release guard.
        let (grants, rest) = split_agent_tail(self.agent.drain_agent_tail().await);
        for changed in grants {
            self.on_portal_grant(changed).await;
        }
        for frame in rest {
            if let Err(error) = Box::pin(self.on_agent_frame(frame)).await {
                tracing::warn!(%error, "an agent frame from the tail could not be handled");
            }
        }
        self.agent_lost_reported = true;
        self.agent.stop().await;
        self.feed(fr::Input::Agent(fr::AgentEvent::AgentLost)).await;
        if self.agent_restart_circuit.take_suspend() {
            // The machine was suspended: the frozen agent is not a broken one, so this
            // loss is not the agent's and is not counted towards the circuit.
            tracing::warn!("the agent was silent across a suspend; not counting it as a loss");
        } else if self.agent_restart_circuit.record_loss(self.wall_ms()) {
            self.open_restart_circuit("agent lost three times within 60 seconds");
            return;
        }
        tracing::warn!("agent lost; scheduling a RECOVERY restart");
        self.agent_restart_due = Some(Instant::now() + self.agent_restart_backoff);
        self.agent_restart_backoff = next_restart_backoff(self.agent_restart_backoff);
    }

    /// The `RECOVERY` restart from core's conservative ledger.
    async fn restart_agent(&mut self) {
        if self.agent_restart_circuit.is_open() {
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
                if self.agent_restart_circuit.record_loss(self.wall_ms()) {
                    self.open_restart_circuit("agent restart failed three times within 60 seconds");
                } else {
                    self.agent_restart_due = Some(Instant::now() + self.agent_restart_backoff);
                    self.agent_restart_backoff = next_restart_backoff(self.agent_restart_backoff);
                }
            }
        }
    }

    fn open_restart_circuit(&mut self, reason: &str) {
        self.agent_restart_due = None;
        self.agent_degraded_reason = format!(
            "Ввод остановлен: {reason}; повтор через {} мин или «Повторить»",
            RESTART_CIRCUIT_COOLDOWN.as_secs() / 60
        );
        tracing::error!(reason, "agent restart circuit breaker opened");
    }
    async fn retry_agent(&mut self) -> protocol::ipc::OpResult {
        self.agent_restart_circuit.close();
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
        let connection_id = match identity::random16() {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(%error, "cannot assign a control connection identity");
                return;
            }
        };
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
        let response = self
            .handle_request(&line, LayoutSource::local("ctl", connection_id))
            .await;
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

    async fn handle_request(&mut self, line: &str, source: LayoutSource) -> Handled {
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
            ctl::Request::Revoke(peer) => self.revoke_peer_ref(&peer).await.into(),
            ctl::Request::Claim => self.claim().await.into(),
            ctl::Request::Abandon => self.abandon().await.into(),
            ctl::Request::LayoutShow => ctl::Response::Status(self.layout_line()).into(),
            ctl::Request::LayoutEdit(path) => self.layout_edit(&path, source).await,
            ctl::Request::LayoutApply => self.layout_apply(source).await,
            ctl::Request::LayoutAuto => self.layout_auto(source).await,
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
        // A manual switch has no edge to cross, so the entry point comes from the receiver's
        // own manifest: the centre of its primary display. This is not cosmetic — the
        // `EnterPrepare` position's `display_id` is what every later `InputBatch` of the lease
        // is stamped with (`transport::convert::batch_display_after`), so the neutral empty
        // position this used to carry blanked the whole pointer stream for the lease, and the
        // receiver refuses (or drops) batches whose display is empty (batch-2 audit).
        let entry_position = match self.receiver_start_position(target) {
            Some(position) => position,
            None => {
                tracing::warn!(
                    peer = %target,
                    "no display is known for the switch target; the transition starts without \
                     a display id"
                );
                geometry::Position::new(String::new(), 0.0, 0.0)
            }
        };
        if !entry_position.display_id.as_str().is_empty() {
            // The captured deltas integrate from here for the whole lease.
            self.remote_pointer = Some(entry_position.clone());
            self.remote_node = Some(target);
        }
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

    /// Where a transition to `peer` starts when there is no edge to cross (batch-2 audit).
    ///
    /// The peer's live manifest first (`Hello`/`DisplaysChanged`), then the configured
    /// snapshot — the same two sources `rebuild_topology` assembles the document from, but
    /// this works even when the document is unusable, which is exactly when a manual switch
    /// has nothing else to go on.
    fn receiver_start_position(&self, peer: NodeId) -> Option<geometry::Position> {
        if let Some((_, displays)) = self.peer_displays.get(&peer) {
            if let Some(position) = crate::topology::start_position(displays) {
                return Some(position);
            }
        }
        let configured = crate::topology::snapshot_peers(&self.layout, self.node_id.as_bytes());
        let displays = configured
            .into_iter()
            .find(|(node, _)| *node == peer)
            .map(|(_, displays)| displays)?;
        crate::topology::start_position(&displays)
    }

    /// `revoke <peer>` over the control channel: the GUI's revoke, with a peer reference
    /// resolved here (audit F16).
    ///
    /// The trust store, the pin set the TLS verifiers read and the live links are all this
    /// core's, so the revoke has to happen here — a CLI that edits `trust.toml` behind a
    /// running core leaves the peer trusted in memory and its link up.
    async fn revoke_peer_ref(&mut self, peer: &crate::command::PeerRef) -> ctl::Response {
        let target = match crate::command::resolve_peer(&self.trust, peer) {
            Ok(target) => target,
            Err(detail) => {
                return ctl::Response::Error {
                    code: "UNKNOWN_PEER".to_owned(),
                    detail,
                }
            }
        };
        let result = self.revoke_peer(target.as_bytes()).await;
        if result.ok {
            ctl::Response::Status(format!("revoked {}", crate::command::node_id_hex(&target)))
        } else {
            ctl::Response::Error {
                code: result.error_code,
                detail: "the peer is not in the trust store".to_owned(),
            }
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
        let disabled = self.transition_disabled_reason();
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
            refusal: if disabled.is_empty() {
                self.refusal.as_deref().unwrap_or("")
            } else {
                disabled.as_str()
            },
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
                "sas={} peer={} ceremony={} address={}",
                prompt.sas_code,
                prompt.peer_name,
                crate::command::hex16(&prompt.ceremony_id),
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
    /// The applied pair the editor based the document on.
    expected: crate::layout::Pair,
    /// Where the connection that asked is waiting.
    verdict: oneshot::Sender<ctl::Response>,
    /// When the request was registered, on the loop's `now_ms` clock.
    at_ms: u64,
}

/// How long a layout request may wait for its verdict (batch-2 audit).
///
/// The verdict travels one link round trip; a request whose answer has not arrived in this
/// long is not going to get one (the peer died mid-CAS, or the answer was dropped as
/// nobody's), and the entry — with its copy of the whole document — used to stay in the map
/// for the life of the process.
const LAYOUT_REQUEST_TTL_MS: u64 = 30_000;

/// Keeps the layout requests a verdict can still arrive for (batch-2 audit).
///
/// Live means the connection that asked is still there (`verdict` open) **and** the request is
/// younger than [`LAYOUT_REQUEST_TTL_MS`]. Returns how many were dropped.
fn prune_layout_requests(requests: &mut HashMap<u64, LayoutRequest>, now_ms: u64) -> usize {
    let before = requests.len();
    requests.retain(|_, request| {
        !request.verdict.is_closed()
            && now_ms.saturating_sub(request.at_ms) <= LAYOUT_REQUEST_TTL_MS
    });
    before - requests.len()
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

/// Splits the frames a dying agent left behind (audit F7).
///
/// The portal grants are the ones core persists and must be reported before the channel is
/// dropped; **every other frame keeps its normal meaning** and is handed to the session —
/// throwing the tail away left the FSM waiting for a `Released` that had already arrived, so
/// the transition only finished when the release guard expired. An empty frame carries no
/// message and is dropped here, which is what the live loop does with it too.
fn split_agent_tail(
    frames: Vec<protocol::ipc::AgentFrame>,
) -> (
    Vec<protocol::ipc::PortalGrantChanged>,
    Vec<protocol::ipc::AgentFrame>,
) {
    let mut grants = Vec::new();
    let mut rest = Vec::new();
    for frame in frames {
        match frame.msg {
            Some(Msg::PortalGrant(changed)) => grants.push(changed),
            Some(_) => rest.push(frame),
            None => {}
        }
    }
    (grants, rest)
}

/// Revalidates an edited layout document (KVM-072: the coordinator validates the whole
/// document; ADR-013 rejected merging rows for exactly that reason).
///
/// The document is the `[topology]` shape, so it parses as the same struct the configuration
/// uses; a document with neither displays nor links is refused, because an empty layout would
/// silently disable every transition.
fn parse_layout_toml(layout_toml: &str) -> Result<config::TopologyConfig, String> {
    let parsed: config::TopologyConfig =
        toml::from_str(layout_toml).map_err(|error| format!("layout_toml: {error}"))?;
    if parsed.displays.is_empty() && parsed.links.is_empty() {
        return Err("layout_toml: a document with neither displays nor links".to_owned());
    }
    Ok(parsed)
}

#[cfg(test)]
mod layout_request_tests {
    use super::*;

    fn request(at_ms: u64) -> (LayoutRequest, oneshot::Receiver<ctl::Response>) {
        let (verdict, receiver) = oneshot::channel();
        (
            LayoutRequest {
                layout_toml: "links = []".to_owned(),
                expected: crate::layout::Pair::NONE,
                verdict,
                at_ms,
            },
            receiver,
        )
    }

    /// Batch-2 audit: a layout request whose verdict cannot arrive is dropped, so the map (and
    /// its copy of the whole document) cannot grow for the life of the process.
    ///
    /// The connection giving up (`verdict` closed) and the TTL expiring are both checked: a
    /// peer that died mid-CAS, or an answer dropped as nobody's, left the entry behind
    /// forever.
    #[test]
    fn a_layout_request_whose_verdict_cannot_arrive_is_dropped() {
        let now = 100_000u64;
        let mut requests = HashMap::new();
        let (fresh, _alive) = request(now - LAYOUT_REQUEST_TTL_MS);
        let (stale, _alive) = request(now - LAYOUT_REQUEST_TTL_MS - 1);
        let (abandoned, gone) = request(now);
        drop(gone);
        requests.insert(1, fresh);
        requests.insert(2, stale);
        requests.insert(3, abandoned);

        assert_eq!(
            prune_layout_requests(&mut requests, now),
            2,
            "the stale request and the abandoned one go"
        );
        assert_eq!(requests.len(), 1);
        assert!(
            requests.contains_key(&1),
            "the request at the ttl boundary is still waiting for its verdict"
        );
    }
}

#[cfg(test)]
mod agent_tail_tests {
    use super::*;
    use protocol::ipc::agent_frame::Msg;

    fn frame(msg: Msg) -> protocol::ipc::AgentFrame {
        protocol::ipc::AgentFrame {
            msg: Some(msg),
            ..Default::default()
        }
    }

    /// Audit F7: the frames an agent left behind are never thrown away.
    ///
    /// `drain_portal_grants` kept only the portal grants, so a `Released`/`DeadlineExpired`
    /// that arrived just before the agent died was consumed and dropped: the FSM waited for a
    /// release it had already been sent, and only the release guard finished the transition.
    #[test]
    fn a_dying_agents_tail_keeps_every_non_portal_frame() {
        let (grants, rest) = split_agent_tail(vec![
            frame(Msg::PortalGrant(
                protocol::ipc::PortalGrantChanged::default(),
            )),
            frame(Msg::Released(protocol::ipc::Released::default())),
            frame(Msg::DeadlineExpired(
                protocol::ipc::DeadlineExpired::default(),
            )),
            frame(Msg::Pong(protocol::ipc::Pong { nonce: 7 })),
        ]);
        assert_eq!(grants.len(), 1, "the portal grant is reported");
        assert_eq!(
            rest.len(),
            3,
            "the released, the deadline and the pong keep their meaning: {rest:?}"
        );
        assert!(
            rest.iter()
                .any(|frame| matches!(frame.msg, Some(Msg::Released(_)))),
            "the release the FSM is waiting for survives"
        );
    }
}

#[cfg(test)]
mod timer_tests {
    use super::*;
    use ipc::clock::ManualClock;

    #[test]
    fn losses_spread_over_the_window_in_wall_time_do_not_open_the_circuit() {
        // The three losses of `target/live-e2e/agent-lost-20261013T1800Z`: 137 s and
        // 158 s apart on the wall clock (296 s in all), with the machine asleep in
        // between. `CLOCK_MONOTONIC` — the clock the old window was measured on — barely
        // moved across those sleeps (20 s and 23 s of it, from the logs), which is what
        // made the old code read them as "three losses within 60 seconds". The losses are
        // recorded directly here, so the test is about the window's clock and not about
        // the suspend classification (that is `a_suspend_is_not_a_loss_of_the_agent`).
        let mut clock = ManualClock::new();
        let mut watch = RestartWatch::new();
        assert!(!watch.record_loss(clock.wall_ms()));
        clock.suspend(137_000);
        assert!(!watch.record_loss(clock.wall_ms()));
        clock.suspend(158_000);
        assert!(
            !watch.record_loss(clock.wall_ms()),
            "296 s of wall time passed between the three losses"
        );
        assert!(!watch.is_open());
    }

    #[test]
    fn losses_inside_the_window_open_the_circuit_and_the_cool_down_closes_it() {
        let mut clock = ManualClock::new();
        let mut watch = RestartWatch::new();
        assert!(!watch.record_loss(clock.wall_ms()));
        clock.advance(10_000);
        assert!(!watch.record_loss(clock.wall_ms()));
        clock.advance(10_000);
        assert!(
            watch.record_loss(clock.wall_ms()),
            "three in twenty seconds"
        );
        assert!(watch.is_open());

        // Still inside the cool-down: nothing has closed it yet.
        clock.advance(RESTART_CIRCUIT_COOLDOWN.as_millis() as u64 - 1);
        assert!(!watch.close_if_cooled(clock.wall_ms()));
        assert!(watch.is_open(), "still inside the cool-down");

        // One millisecond later it closes itself, so the agent is retried.
        clock.advance(1);
        assert!(watch.close_if_cooled(clock.wall_ms()));
        assert!(!watch.is_open());
        assert!(
            !watch.close_if_cooled(clock.wall_ms()),
            "closing a closed circuit is a no-op"
        );

        // The losses went with it: a fresh pair does not re-open it.
        assert!(!watch.record_loss(clock.wall_ms()));
        clock.advance(1_000);
        assert!(!watch.record_loss(clock.wall_ms()));
        assert!(!watch.is_open());
    }

    #[test]
    fn a_suspend_is_not_a_loss_of_the_agent() {
        // `Session::observe_clock` samples the two clocks once a turn and calls
        // `on_suspend`; `Session::on_agent_lost` then declines to count the loss. Three
        // such losses must not read as a crash loop.
        let mut clock = ManualClock::new();
        let mut detector = SuspendDetector::new(
            clock.monotonic_ms(),
            clock.wall_ms(),
            SUSPEND_THRESHOLD.as_millis() as u64,
        );
        let mut watch = RestartWatch::new();
        for _ in 0..RESTART_CIRCUIT_FAILURES {
            clock.suspend(130_000);
            assert!(
                detector
                    .observe(clock.monotonic_ms(), clock.wall_ms())
                    .is_some_and(|slept| slept >= 130_000),
                "wall time ran while the monotonic clock did not"
            );
            watch.on_suspend();
            let opened = !watch.take_suspend() && watch.record_loss(clock.wall_ms());
            assert!(!opened, "the silence across a suspend is not the agent's");
            // Ordinary time again, and the agent answers.
            clock.advance(1_000);
            assert_eq!(
                detector.observe(clock.monotonic_ms(), clock.wall_ms()),
                None
            );
            watch.on_frame();
        }
        assert!(!watch.is_open(), "three suspends are not three losses");

        // The same three losses *without* a suspend behind them are the agent's.
        let mut clock = ManualClock::new();
        let mut watch = RestartWatch::new();
        for _ in 0..RESTART_CIRCUIT_FAILURES {
            clock.advance(1_000);
            watch.record_loss(clock.wall_ms());
        }
        assert!(watch.is_open());
    }

    #[test]
    fn restart_policy_stops_retries_instead_of_spawning_a_ten_second_storm() {
        let mut clock = ManualClock::new();
        let mut watch = RestartWatch::new();
        let mut backoff = RESTART_BACKOFF;
        let mut automatic_spawns = 0;
        while !watch.record_loss(clock.wall_ms()) && clock.monotonic_ms() < 10_000 {
            clock.advance(backoff.as_millis() as u64);
            automatic_spawns += 1;
            backoff = next_restart_backoff(backoff);
        }
        assert_eq!(automatic_spawns, 2);
        assert!(clock.monotonic_ms() < 10_000);
        assert_eq!(backoff, Duration::from_secs(2));
        assert!(watch.is_open(), "the third loss inside the window stops it");
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

fn source_owner_may_capture_injected(dev_flag: bool, is_owner: bool) -> bool {
    dev_flag && is_owner
}

#[cfg(test)]
mod dev_capture_tests {
    use super::source_owner_may_capture_injected;

    #[test]
    fn external_injection_requires_both_dev_flag_and_source_ownership() {
        assert!(!source_owner_may_capture_injected(false, true));
        assert!(!source_owner_may_capture_injected(true, false));
        assert!(source_owner_may_capture_injected(true, true));
    }

    #[test]
    fn portal_activation_on_a_follower_releases_without_a_source_transition() {
        let node = geometry::NodeId::new([1; 32]);
        let peer = config::peer_reference(&[2; 32]);
        let config = config::Config::from_str(&format!(
            r#"
config_version = 2
[[topology.displays]]
node = "self"
id = "local"
logical = [0, 0, 1920, 1080]
primary = true
[[topology.displays]]
node = "{peer}"
id = "remote"
logical = [0, 0, 1920, 1080]
primary = true
[[topology.links]]
id = "right"
from = {{ node = "self", display = "local", edge = "right", start = 0, end = 1080 }}
to = {{ node = "{peer}", display = "remote", edge = "left", start = 0, end = 1080 }}
bidirectional = true
"#
        ))
        .expect("valid configuration");
        let local = crate::topology::snapshot_displays(&config.topology, node.as_bytes());
        let peers = crate::topology::snapshot_peers(&config.topology, node.as_bytes());
        let topology = crate::topology::build(&crate::topology::Documents {
            local_node_id: *node.as_bytes(),
            topology: &config.topology,
            local_displays: &local,
            peer_displays: &peers,
        })
        .expect("configured link")
        .topology;
        let activation = protocol::ipc::PortalActivation {
            position: Some(protocol::ipc::PointerAbs {
                display_id: String::new(),
                x: 1919.0,
                y: 540.0,
            }),
            edge: protocol::ipc::PortalEdge::Right as i32,
        };
        assert!(super::portal_activation_plan(
            Some(&topology),
            &node,
            true,
            focus_router::FocusState::Local,
            Some(&activation)
        )
        .is_ok());
        let action = super::portal_activation_plan(
            Some(&topology),
            &node,
            false,
            focus_router::FocusState::Local,
            Some(&activation),
        );
        assert_eq!(
            action,
            Err(protocol::ipc::EndCapture {
                lease_id: Vec::new(),
                replay_buffered_locally: true,
            })
        );
    }
}
