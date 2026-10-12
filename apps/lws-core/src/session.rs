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

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use focus_router as fr;
use geometry::NodeId;
use input_model::InputEvent;
use ipc::Watchdog;
use protocol::ipc::agent_frame::Msg;
use tokio::sync::mpsc;
use transport::{LinkEvent, LinkManager};

use crate::agent::Supervisor;
use crate::command::PeerRef;
use crate::ctl;
use crate::ownership_store::OwnershipStore;
use crate::routing::{self, AgentSignal, Effect};

/// How long the loop sleeps when nothing else has a deadline.
///
/// It bounds how long a dead agent child can go unnoticed; the IPC heartbeat and the
/// FSM timers are all shorter than this in the default configuration.
const IDLE_TICK: Duration = Duration::from_millis(50);
/// Backoff before a crashed agent is respawned.
const RESTART_BACKOFF: Duration = Duration::from_millis(500);

/// Everything one `lws-core` process owns while it runs.
pub struct Session {
    router: crate::ids::Router,
    manager: Arc<LinkManager>,
    link_events: mpsc::Receiver<LinkEvent>,
    agent: Supervisor,
    ctl: ctl::Server,
    timers: Timers,
    context: routing::AgentContext,
    watchdog: Watchdog,
    started: Instant,
    ping_nonce: u64,
    node_id: NodeId,
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
    /// Set by `quit` on the control channel.
    stopping: bool,
    /// The agent reported `AgentLost` for the current crash and is being restarted.
    agent_restart_due: Option<Instant>,
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
        watchdog: Watchdog,
        node_id: NodeId,
        backend: String,
        trust: identity::TrustStore,
        layout: config::TopologyConfig,
        pointer_gain: f64,
        ownership_store: OwnershipStore,
        layout_store: crate::layout_store::LayoutStore,
    ) -> Self {
        let layout_state = layout_store.restored();
        let mut session = Session {
            router,
            manager,
            link_events,
            agent,
            ctl,
            timers: Timers::default(),
            refusal: None,
            context: routing::AgentContext::default(),
            watchdog,
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
            stopping: false,
            agent_restart_due: None,
            agent_lost_reported: false,
            topology: None,
            layout,
            pointer_gain,
            agent_displays: Vec::new(),
            local_pointer: None,
            remote_pointer: None,
            remote_node: None,
            pending_target: None,
            observe_armed: false,
        };
        session.rebuild_topology();
        session
    }

    /// Assembles the layout document from everything known right now.
    ///
    /// The local manifest is the agent's report when there is one and the
    /// `[[topology.displays]]` snapshot otherwise; the peers' manifests come from the same
    /// snapshot until their `Hello` carries displays. A document that fails validation is
    /// logged and dropped, which disables edge-triggered transitions instead of
    /// mis-resolving them.
    fn rebuild_topology(&mut self) {
        let snapshot = crate::topology::snapshot_displays(&self.layout, self.node_id.as_bytes());
        let displays = if self.agent_displays.is_empty() {
            snapshot
        } else {
            self.agent_displays.clone()
        };
        let peers = crate::topology::snapshot_peers(&self.layout, self.node_id.as_bytes());
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
                return Ok(());
            }

            let now = self.now_ms();
            let deadline = self.instant_of(self.next_deadline(now));
            let attached = self.agent.is_attached();
            let step = {
                tokio::select! {
                    event = self.link_events.recv() => Step::Link(event),
                    frame = self.agent.recv(), if attached => Step::Agent(frame),
                    client = self.ctl.accept() => Step::Control(client),
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
                Step::Tick => self.arm_observation().await,
            }
        }
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
            if self.watchdog.heartbeat_due(now) {
                self.ping_nonce = self.ping_nonce.wrapping_add(1);
                let ping = Msg::Ping(protocol::ipc::Ping {
                    nonce: self.ping_nonce,
                });
                if let Err(error) = self.agent.send(ping).await {
                    tracing::warn!(%error, "cannot ping the agent");
                    self.on_agent_lost().await;
                } else {
                    self.watchdog.on_heartbeat(now);
                }
            }
            if self.agent.child_exited() || self.watchdog.silence_exceeded(now) {
                tracing::warn!(
                    silence_ms = self.watchdog.silence_ms(now),
                    "the agent stopped answering"
                );
                self.on_agent_lost().await;
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
            let heartbeat = now_ms.saturating_add(self.watchdog.next_heartbeat_in_ms(now_ms));
            deadline = deadline.min(heartbeat);
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
                            },
                        )
                        .await;
                    }
                    crate::layout::TopologyOutcome::Invalid(detail) => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::InvalidTopology,
                            &detail,
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
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::LayoutConflict,
                            &format!("expected revision {} is applied", current.revision),
                        )
                        .await;
                    }
                    crate::layout::EditOutcome::Invalid(detail) => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::InvalidTopology,
                            &detail,
                        )
                        .await;
                    }
                    crate::layout::EditOutcome::NotCoordinator => {
                        self.send_layout_error(
                            from,
                            protocol::wire::ErrorCode::NoCoordinator,
                            "this node is not the coordinator",
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
            LayoutMessage::Error { code } => {
                // The editor's answer: `layout show` reports it until a revision moves.
                self.last_layout_error = Some((
                    code.as_str_name().to_owned(),
                    format!("the coordinator refused the edit ({})", code.as_str_name()),
                ));
                tracing::warn!(peer = %from, code = code.as_str_name(), "layout edit refused");
            }
            LayoutMessage::TopologyApplied { revision } => {
                tracing::info!(peer = %from, revision, "the peer applied the layout revision");
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
    ) {
        tracing::warn!(peer = %peer, code = code.as_str_name(), detail, "layout refused for a peer");
        // «The last refusal this node saw *or sent*»: a refusal this node emits is as much a
        // fact about the document as one it received, and `layout show` reports it either way.
        self.last_layout_error = Some((code.as_str_name().to_owned(), detail.to_owned()));
        self.send_layout_to(peer, transport::convert::LayoutMessage::Error { code })
            .await;
    }

    /// `layout apply`: apply the pending edit, which only the coordinator can do.
    async fn layout_apply(&mut self) -> ctl::Response {
        let (base_pair, layout_toml) = match self.layout_state.pending() {
            Some(toml) => (
                self.layout_state
                    .pending_base()
                    .unwrap_or(self.layout_state.applied()),
                toml.to_owned(),
            ),
            None => {
                return ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail: "no pending layout edit".to_owned(),
                }
            }
        };
        if !self.router.ownership().is_owner() {
            // The pending edit belongs to the coordinator's CAS: a follower forwards it and
            // keeps it pending until the send lands.
            let Some(owner) = self.router.ownership().owner().copied() else {
                return ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the pending edit waits".to_owned(),
                };
            };
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
                });
            if !self.send_layout_to(owner, message).await {
                return ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "the coordinator is unreachable; the pending edit waits".to_owned(),
                };
            }
            // Sent: the coordinator runs the CAS with the base the edit names, and replicates
            // the new revision back (or refuses it by name).
            self.layout_state.take_pending();
            return ctl::Response::Status(format!(
                "sent to the coordinator ({}), based on revision {}",
                identity::fingerprint(&owner),
                base_pair.revision
            ));
        }
        let edit = crate::layout::Edit {
            expected: base_pair,
            editor: self.node_id,
            layout_toml,
        };
        let outcome = self
            .layout_state
            .apply_edit(&edit, self.node_id, true, validate_layout_toml);
        match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                self.layout_state.take_pending();
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
        }
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
    /// conflict instead of guessing what the caller meant.
    async fn layout_edit(&mut self, path: &str) -> ctl::Response {
        let is_coordinator = self.router.ownership().is_owner();
        let document_text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                return ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail: format!("cannot read {path}: {error}"),
                }
            }
        };
        // The document is needed twice on the follower path: inside the frame and, if the
        // coordinator turns out to be unreachable, in `layout.pending.toml`.

        let edit = crate::layout::Edit {
            expected: self.layout_state.applied(),
            editor: self.node_id,
            layout_toml: document_text.clone(),
        };
        if let Some(reason) = self.layout_editing_disabled.clone() {
            self.layout_state.hold_pending(edit.layout_toml.clone());
            self.layout_store.write_pending(&edit.layout_toml);
            return ctl::Response::Error {
                code: "NO_COORDINATOR".to_owned(),
                detail: format!("{reason}; the edit is kept as pending"),
            };
        }
        if !is_coordinator {
            // A follower sends the edit to the coordinator and waits for the answer; only a
            // partition or a conflict leaves it pending (`docs/05-protocol.md`).
            let Some(owner) = self.router.ownership().owner().copied() else {
                self.layout_state.hold_pending(edit.layout_toml.clone());
                self.layout_store.write_pending(&edit.layout_toml);
                return ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "no owner is known; the edit is kept as pending".to_owned(),
                };
            };
            let applied = self.layout_state.applied();
            let message =
                transport::convert::LayoutMessage::LayoutEdit(protocol::wire::LayoutEdit {
                    expected_revision: applied.revision,
                    expected_author: applied
                        .author
                        .map_or_else(Vec::new, |id| id.as_bytes().to_vec()),
                    author_node_id: self.node_id.as_bytes().to_vec(),
                    layout_toml: document_text.clone().into_bytes(),
                });
            if self.manager.handle(&owner).await.is_none() {
                self.layout_state.hold_pending(document_text.clone());
                self.layout_store.write_pending(&document_text);
                return ctl::Response::Error {
                    code: "NO_COORDINATOR".to_owned(),
                    detail: "the coordinator is unreachable; the edit is kept as pending"
                        .to_owned(),
                };
            }
            self.send_layout_to(owner, message).await;
            return ctl::Response::Status(format!(
                "sent to the coordinator ({}), based on revision {}",
                identity::fingerprint(&owner),
                self.layout_state.applied().revision
            ));
        }
        let outcome =
            self.layout_state
                .apply_edit(&edit, self.node_id, is_coordinator, validate_layout_toml);
        match outcome {
            crate::layout::EditOutcome::Applied(pair) => {
                self.layout_store.observe(&self.layout_state);
                self.router.set_topology_revision(pair.revision);
                tracing::info!(
                    revision = pair.revision,
                    author = identity::fingerprint(&self.node_id).as_str(),
                    "layout applied by the coordinator"
                );
                ctl::Response::Status(format!(
                    "applied revision={} author={}",
                    pair.revision,
                    identity::fingerprint(&self.node_id)
                ))
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
        }
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
            LinkEvent::Up { peer, hello } => {
                tracing::info!(
                    peer = %peer,
                    name = hello.display_name.as_str(),
                    can_own = hello.can_own,
                    receiver_ready = hello.capabilities.receiver_ready(),
                    "link up"
                );
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
            },
            LinkEvent::Down { peer, reason } => {
                tracing::warn!(peer = %peer, ?reason, "link down");
                self.feed(fr::Input::LinkDown { peer }).await;
            }
        }
    }

    async fn on_agent_frame(&mut self, frame: protocol::ipc::AgentFrame) -> anyhow::Result<()> {
        let now = self.now_ms();
        self.watchdog.on_frame(now);
        let Some(message) = frame.msg else {
            return Ok(());
        };
        tracing::info!(frame = routing::agent_frame_name(&message), "agent -> core");
        // `status` shows the last thing the agent had to report; the note is derived
        // from the frame and never contains a token (`routing::agent_note`).
        if let Some(note) = routing::agent_note(&message) {
            self.refusal = Some(note);
        }
        match routing::agent_signal(&message, &mut self.context) {
            AgentSignal::Fsm(event) => {
                if matches!(event, fr::AgentEvent::AgentReady) {
                    self.agent.mark_ready();
                    self.agent_lost_reported = false;
                    self.agent_restart_due = None;
                    self.watchdog.on_frame(now);
                    // A new session starts with a clean slate; the previous crash's
                    // error is not this session's state.
                    self.refusal = None;
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
                // The report replaces the snapshot for this node, so the document is
                // assembled again: a hot-plug can add or remove a link endpoint.
                self.agent_displays = report.displays.clone();
                self.rebuild_topology();
            }
            AgentSignal::Portal { interface, detail } => {
                tracing::info!(
                    interface = interface.as_str(),
                    detail = detail.as_str(),
                    "portal"
                );
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
        self.agent_lost_reported = true;
        self.agent.mark_lost();
        tracing::warn!("agent lost; telling the FSM and restarting in RECOVERY");
        self.feed(fr::Input::Agent(fr::AgentEvent::AgentLost)).await;
        self.restart_agent().await;
    }

    /// The `RECOVERY` restart from core's conservative ledger.
    async fn restart_agent(&mut self) {
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
                // The fresh `NORMAL` agent reports `AgentReady`, which clears
                // `Blocked` and finishes the recovery (S4). Its channel is new, so the
                // heartbeat starts from now.
                self.watchdog = Watchdog::for_core_silence_ms(
                    self.watchdog.silence_limit_ms() as u32,
                    self.now_ms(),
                )
                .unwrap_or(self.watchdog);
            }
            Err(error) => {
                tracing::error!(%error, "agent restart failed; retrying");
                self.agent_restart_due = Some(Instant::now() + RESTART_BACKOFF);
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
        let reply = response.render();
        if let Err(error) = ctl::write_line(&mut stream, &reply).await {
            tracing::warn!(%error, "cannot answer a control request");
        }
    }

    async fn handle_request(&mut self, line: &str) -> ctl::Response {
        let request = match ctl::Request::parse(line) {
            Ok(request) => request,
            Err(detail) => {
                return ctl::Response::Error {
                    code: "BAD_REQUEST".to_owned(),
                    detail,
                }
            }
        };
        tracing::info!(?request, "control request");
        match request {
            ctl::Request::Ping => ctl::Response::Pong,
            ctl::Request::Status => ctl::Response::Status(self.status_line().await),
            ctl::Request::ResetPortals => self.reset_portals().await,
            ctl::Request::Quit => {
                self.stopping = true;
                ctl::Response::Stopping
            }
            ctl::Request::Switch(peer) => self.switch_to(&peer).await,
            ctl::Request::Claim => self.claim().await,
            ctl::Request::Abandon => self.abandon().await,
            ctl::Request::LayoutShow => ctl::Response::Status(self.layout_line()),
            ctl::Request::LayoutEdit(path) => self.layout_edit(&path).await,
            ctl::Request::LayoutApply => self.layout_apply().await,
            ctl::Request::LayoutDiscard => {
                let dropped = self.layout_state.take_pending().is_some();
                tracing::info!(dropped, "pending layout edit discarded");
                ctl::Response::Status(format!(
                    "pending={}",
                    if dropped { "discarded" } else { "none" }
                ))
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

/// What one turn of the loop produced.
enum Step {
    Link(Option<LinkEvent>),
    Agent(Result<protocol::ipc::AgentFrame, String>),
    Control(Result<ctl::ClientStream, ipc::IpcError>),
    Tick,
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
}
