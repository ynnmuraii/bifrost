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
    router: fr::FocusRouter,
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
    /// Set by `quit` on the control channel.
    stopping: bool,
    /// The agent reported `AgentLost` for the current crash and is being restarted.
    agent_restart_due: Option<Instant>,
    /// Whether core told the FSM about the current agent loss.
    agent_lost_reported: bool,
}

impl Session {
    /// Builds the loop from its parts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: fr::FocusRouter,
        manager: Arc<LinkManager>,
        link_events: mpsc::Receiver<LinkEvent>,
        agent: Supervisor,
        ctl: ctl::Server,
        watchdog: Watchdog,
        node_id: NodeId,
        backend: String,
        trust: identity::TrustStore,
    ) -> Self {
        Session {
            router,
            manager,
            link_events,
            agent,
            ctl,
            timers: Timers::default(),
            context: routing::AgentContext::default(),
            watchdog,
            started: Instant::now(),
            ping_nonce: 0,
            node_id,
            backend,
            trust,
            stopping: false,
            agent_restart_due: None,
            agent_lost_reported: false,
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
                Step::Tick => {}
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
                Effect::Notice(notice) => self.log_notice(&notice),
            }
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
            }
            LinkEvent::Message { peer, message } => {
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
        match routing::agent_signal(&message, &mut self.context) {
            AgentSignal::Fsm(event) => {
                if matches!(event, fr::AgentEvent::AgentReady) {
                    self.agent.mark_ready();
                    self.agent_lost_reported = false;
                    self.agent_restart_due = None;
                    self.watchdog.on_frame(now);
                }
                self.feed(fr::Input::Agent(event)).await;
            }
            AgentSignal::Local(local) => self.feed(fr::Input::Local(local)).await,
            AgentSignal::Captured(events) => self.forward_captured(events).await,
            AgentSignal::Displays(report) => {
                tracing::info!(
                    displays = report.displays.len(),
                    revision = report.local_revision,
                    "agent display report"
                );
            }
            AgentSignal::Portal { interface, detail } => {
                tracing::info!(
                    interface = interface.as_str(),
                    detail = detail.as_str(),
                    "portal"
                );
            }
            AgentSignal::Ignored => {}
        }
        Ok(())
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
    async fn on_control(&mut self, mut stream: ctl::ClientStream) {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let read = {
            let mut reader = tokio::io::BufReader::new(&mut stream);
            match reader.read_line(&mut line).await {
                Ok(read) => read,
                Err(error) => {
                    tracing::warn!(%error, "cannot read a control request");
                    return;
                }
            }
        };
        if read == 0 {
            return;
        }
        let response = self.handle_request(&line).await;
        let reply = response.render();
        if let Err(error) = ctl::serve_line(&mut stream, &reply).await {
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
            ctl::Request::Quit => {
                self.stopping = true;
                ctl::Response::Stopping
            }
            ctl::Request::Switch(peer) => self.switch_to(&peer).await,
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

    fn next(&self) -> Option<(fr::Timer, u64)> {
        self.deadlines
            .iter()
            .next()
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
