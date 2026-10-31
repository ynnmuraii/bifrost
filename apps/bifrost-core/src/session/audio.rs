//! Optional audio control actor. The input Session only updates bounded channels/watch state.
//! Native IPC and config writes never run on the KVM dispatch path. Every media lease is
//! renewed by this actor, not by the supervisor's independent IPC heartbeat.
mod wire;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use audio_model::{
    focus::{FocusPolicy, Owner, Snapshot},
    jitter::Profile,
    keys, session as fsm, status as model,
};
use geometry::NodeId;
use protocol::{audio_worker as worker, ipc as gui, wire as net};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{timeout, Instant};
use transport::{AuxPurpose, DerivedKey, LinkManager};

use crate::audio::{
    AudioHandle, AudioSupervisor, AudioSupervisorConfig, AudioSupervisorError, AudioSupervisorEvent,
};
use crate::gui::{Outgoing, Request};

const COMMANDS: usize = 32;
const TICK: Duration = Duration::from_millis(25);
const REQUEST_TIMEOUT_MS: u64 = 10_000;
type Code = &'static str;

pub(super) fn is_request(message: &gui::gui_frame::Msg) -> bool {
    use gui::gui_frame::Msg;
    matches!(
        message,
        Msg::GetAudioStatus(_)
            | Msg::SetAudio(_)
            | Msg::StartAudio(_)
            | Msg::StopAudio(_)
            | Msg::SetMicrophoneMute(_)
            | Msg::SetPtt(_)
            | Msg::ListAudioDevices(_)
            | Msg::SetAudioPeerGain(_)
            | Msg::SendMediaCommand(_)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct CommittedFocus {
    pub owner: Option<(NodeId, u64)>,
    pub active: Option<NodeId>,
    pub lease: Option<[u8; 16]>,
    pub committed: bool,
}

#[derive(Clone)]
struct Peer {
    parent: fsm::Parent,
    name: String,
    remote: SocketAddr,
    support: fsm::Support,
    trusted: bool,
    hello: Option<fsm::Hello>,
    focus_support: bool,
    focus_sent_revision: u64,
    focus_request_at: Option<u64>,
    focus_reply_at: Option<u64>,
    focus_reply: bool,
}
#[derive(Clone, Default)]
struct Links {
    peers: BTreeMap<NodeId, Peer>,
    trusted: BTreeSet<NodeId>,
    overflow: u64,
}
struct Command {
    parent: fsm::Parent,
    message: Result<net::AudioControl, protocol::validate::AudioViolation>,
}

pub(super) struct Coordinator {
    commands: mpsc::Sender<Command>,
    gui_commands: mpsc::Sender<Request>,
    links: watch::Sender<Links>,
    focus: watch::Sender<CommittedFocus>,
    caps: watch::Receiver<Vec<i32>>,
    stopping: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Coordinator {
    pub(super) fn spawn(
        config: config::AudioConfig,
        local: NodeId,
        manager: Arc<LinkManager>,
        paths: config::Paths,
        listen: SocketAddr,
        test_dry: bool,
        events: broadcast::Sender<Outgoing>,
    ) -> Self {
        let (commands, rx) = mpsc::channel(COMMANDS);
        let (gui_commands, gui_rx) = mpsc::channel(16);
        let (links, link_rx) = watch::channel(Links::default());
        let (focus_tx, focus_rx) = watch::channel(CommittedFocus::default());
        let (caps_tx, caps) = watch::channel(Vec::new());
        let (stopping, stop_rx) = watch::channel(false);
        let focus_policy = FocusPolicy::new(
            u64::from(config.focus_debounce_ms),
            u64::from(config.focus_ramp_ms),
            background_percent(&config),
        );
        let actor = Actor {
            config,
            local,
            manager,
            paths,
            listen,
            test_dry,
            events,
            commands: rx,
            gui_commands: gui_rx,
            links: link_rx,
            stopping: stop_rx,
            focus_rx,
            focus_view: CommittedFocus::default(),
            focus_policy,
            focus_revision: 0,
            focus_snapshot: None,
            focus_received: false,
            caps: caps_tx,
            model: fsm::ControlPlane::new(*local.as_bytes()),
            peers: BTreeMap::new(),
            admitted: BTreeSet::new(),
            records: BTreeMap::new(),
            effects: VecDeque::new(),
            scratch_effects: Vec::new(),
            retry: VecDeque::new(),
            worker_epoch: 0,
            worker: None,
            worker_events: None,
            ready: None,
            revision: 1,
            consent_revision: 1,
            epoch: Instant::now(),
            overflow: 0,
            last_status: None,
            dirty: true,
            configuration_error: None,
            worker_failed: false,
            pending_starts: BTreeMap::new(),
            deferred_starts: BTreeMap::new(),
        };
        let task = tokio::spawn(actor.run());
        Self {
            commands,
            gui_commands,
            links,
            focus: focus_tx,
            caps,
            stopping,
            task: Some(task),
        }
    }
    pub(super) fn focus(&self, view: CommittedFocus) {
        self.focus.send_if_modified(|current| {
            if *current == view {
                false
            } else {
                *current = view;
                true
            }
        });
    }
    pub(super) fn trusted(&self, store: &identity::TrustStore) {
        let trusted: BTreeSet<_> = store
            .peers
            .iter()
            .filter(|peer| peer.is_active())
            .map(|peer| peer.node_id)
            .collect();
        self.links.send_if_modified(|links| {
            if links.trusted == trusted {
                return false;
            }
            for (id, peer) in &mut links.peers {
                peer.trusted = trusted.contains(id);
            }
            links.trusted = trusted;
            true
        });
    }
    pub(super) fn link_up(
        &self,
        peer: NodeId,
        generation: [u8; 16],
        connection_id: [u8; 16],
        hello: &transport::convert::PeerInfo,
        remote: SocketAddr,
        trusted: bool,
    ) {
        self.links.send_modify(|links| {
            if trusted {
                links.trusted.insert(peer);
            }
            links.peers.insert(
                peer,
                Peer {
                    parent: fsm::Parent {
                        peer: *peer.as_bytes(),
                        connection_id,
                        generation,
                    },
                    name: hello.display_name.clone(),
                    remote,
                    trusted,
                    support: fsm::Support {
                        control: hello.audio_control,
                        system_source: hello.system_audio_source,
                        listener: hello.audio_listener,
                    },
                    hello: None,
                    focus_support: hello.audio_focus_report,
                    focus_sent_revision: 0,
                    focus_request_at: None,
                    focus_reply_at: None,
                    focus_reply: false,
                },
            );
        });
    }
    pub(super) fn link_down(&self, peer: NodeId, generation: [u8; 16]) {
        self.links.send_modify(|links| {
            if links
                .peers
                .get(&peer)
                .is_some_and(|p| p.parent.generation == generation)
            {
                links.peers.remove(&peer);
            }
        });
    }
    pub(super) fn capabilities_changed(&self, peer: NodeId, capabilities: &[i32]) {
        self.links.send_modify(|links| {
            if let Some(peer) = links.peers.get_mut(&peer) {
                peer.support = support(capabilities);
                peer.focus_support =
                    capabilities.contains(&(net::Capability::AudioFocusReportV1 as i32));
            }
        });
    }
    pub(super) fn revoke(&self, peer: NodeId) {
        self.links.send_modify(|links| {
            links.trusted.remove(&peer);
            if let Some(peer) = links.peers.get_mut(&peer) {
                peer.trusted = false;
            }
        });
    }
    fn parent(&self, peer: NodeId, connection: [u8; 16]) -> Option<fsm::Parent> {
        self.links
            .borrow()
            .peers
            .get(&peer)
            .filter(|p| p.trusted && p.parent.connection_id == connection)
            .map(|p| p.parent)
    }
    fn enqueue(&self, command: Command) {
        if self.commands.try_send(command).is_err() {
            // Lifecycle is lossless: overflow revokes existing audio authority via watch,
            // rather than dropping a Down/revocation behind normal metadata.
            self.links
                .send_modify(|links| links.overflow = links.overflow.saturating_add(1));
        }
    }
    pub(super) fn receive(&self, peer: NodeId, connection: [u8; 16], message: net::AudioControl) {
        if let Some(parent) = self.parent(peer, connection) {
            self.enqueue(Command {
                parent,
                message: Ok(message),
            });
        }
    }
    pub(super) fn rejected(
        &self,
        peer: NodeId,
        connection: [u8; 16],
        violation: protocol::validate::AudioViolation,
    ) {
        if let Some(parent) = self.parent(peer, connection) {
            self.enqueue(Command {
                parent,
                message: Err(violation),
            });
        }
    }
    pub(super) fn submit(&self, request: Request) -> Result<(), (u64, mpsc::Sender<Outgoing>)> {
        self.gui_commands.try_send(request).map_err(|error| {
            let request = error.into_inner();
            (request.request_id, request.out)
        })
    }
    pub(super) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.caps.changed().await
    }
    pub(super) fn capabilities(&self) -> Vec<i32> {
        self.caps.borrow().clone()
    }
    pub(super) async fn shutdown(&mut self) {
        self.stopping.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        self.stopping.send_replace(true);
    }
}

struct Worker {
    handle: AudioHandle,
    generation: u64,
    alive: bool,
    model_ready: bool,
    pid: u32,
    probe_ack: u64,
}
struct Materials {
    forward: DerivedKey,
    reverse: DerivedKey,
    forward_nonce: DerivedKey,
    reverse_nonce: DerivedKey,
}
impl Materials {
    fn into_wire(self) -> (worker::DirectionMaterial, worker::DirectionMaterial) {
        // The protobuf owners erase their Vec fields on drop. DerivedKey owners are
        // erased here, before the validated private write; no root or raw array persists.
        (
            worker::DirectionMaterial {
                key: self.forward.expose_secret().to_vec(),
                nonce_prefix: self.forward_nonce.expose_secret()[..4].to_vec(),
            },
            worker::DirectionMaterial {
                key: self.reverse.expose_secret().to_vec(),
                nonce_prefix: self.reverse_nonce.expose_secret()[..4].to_vec(),
            },
        )
    }
}
struct Record {
    view: fsm::SessionView,
    agreement: fsm::Agreement,
    role: model::AudioRole,
    material: Option<Materials>,
    start_request_id: Option<u64>,
    health: model::AudioHealth,
    health_state: model::StreamHealth,
    capture_active: Option<bool>,
    sending: Option<bool>,
    applied_gain: Option<(u32, bool)>,
}
struct Pending {
    request: Request,
    deadline: u64,
}
struct Actor {
    config: config::AudioConfig,
    local: NodeId,
    manager: Arc<LinkManager>,
    paths: config::Paths,
    listen: SocketAddr,
    test_dry: bool,
    events: broadcast::Sender<Outgoing>,
    commands: mpsc::Receiver<Command>,
    gui_commands: mpsc::Receiver<Request>,
    links: watch::Receiver<Links>,
    stopping: watch::Receiver<bool>,
    focus_rx: watch::Receiver<CommittedFocus>,
    focus_view: CommittedFocus,
    focus_policy: FocusPolicy,
    focus_revision: u64,
    focus_snapshot: Option<Snapshot>,
    focus_received: bool,
    caps: watch::Sender<Vec<i32>>,
    model: fsm::ControlPlane,
    peers: BTreeMap<NodeId, Peer>,
    admitted: BTreeSet<NodeId>,
    records: BTreeMap<NodeId, Record>,
    worker_epoch: u64,
    effects: VecDeque<fsm::Effect>,
    scratch_effects: Vec<fsm::Effect>,
    retry: VecDeque<(fsm::Parent, fsm::Control)>,
    worker: Option<Worker>,
    worker_events: Option<mpsc::Receiver<AudioSupervisorEvent>>,
    ready: Option<worker::Ready>,
    revision: u64,
    consent_revision: u32,
    epoch: Instant,
    overflow: u64,
    last_status: Option<gui::AudioStatus>,
    dirty: bool,
    configuration_error: Option<Code>,
    worker_failed: bool,
    pending_starts: BTreeMap<[u8; 16], Pending>,
    deferred_starts: BTreeMap<NodeId, Pending>,
}
impl Actor {
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }
    fn current(&self, parent: fsm::Parent) -> bool {
        self.links
            .borrow()
            .peers
            .get(&NodeId::new(parent.peer))
            .is_some_and(|p| p.trusted && p.support.control && p.parent == parent)
    }
    fn role(&self) -> model::AudioRole {
        match self.config.role {
            config::AudioRole::Listener => model::AudioRole::Listener,
            _ => model::AudioRole::Source,
        }
    }
    fn allowed(&self, peer: NodeId) -> bool {
        permitted(&self.config, peer)
    }
    fn feed(&mut self, input: fsm::Input) -> Result<(), Code> {
        self.scratch_effects.clear();
        let now = self.now();
        let result = self
            .model
            .handle(now, input, &mut self.scratch_effects)
            .map_err(model_code);
        self.dirty |= self
            .scratch_effects
            .iter()
            .any(|effect| matches!(effect, fsm::Effect::PublishStatus));
        self.effects.extend(self.scratch_effects.drain(..));
        self.sync_views();
        result
    }
    fn sync_views(&mut self) {
        for (peer, record) in &mut self.records {
            if let Some(view) = self.model.session(*peer.as_bytes()) {
                if view.session == record.view.session && view.parent == record.view.parent {
                    self.dirty |= record.view != view;
                    record.view = view;
                }
            }
        }
    }
    fn configure(&mut self) -> Result<(), Code> {
        self.focus_policy.configure(
            self.now(),
            u64::from(self.config.focus_debounce_ms),
            u64::from(self.config.focus_ramp_ms),
            background_percent(&self.config),
        );
        let role = match self.config.role {
            config::AudioRole::Off => None,
            config::AudioRole::Source => Some(model::AudioRole::Source),
            config::AudioRole::Listener => Some(model::AudioRole::Listener),
        };
        self.feed(fsm::Input::Configure(fsm::Settings {
            enabled: self.config.enabled,
            role,
            group: self.config.group_id.map(|id| id.0),
            listener: self.config.listener_node_id.map(|id| id.0),
            port: self.config.media_port,
        }))?;
        self.consent_revision = self
            .consent_revision
            .checked_add(1)
            .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE)?;
        let peers: Vec<_> = self.peers.keys().copied().collect();
        for peer in peers {
            let _ = self.feed(fsm::Input::LocalConsent {
                peer: *peer.as_bytes(),
                allowed: self.allowed(peer),
                revision: self.consent_revision,
            });
        }
        Ok(())
    }
    fn publish_caps(&self) {
        let mut bits = [0; 4];
        bits[0] = net::Capability::AudioFocusReportV1 as i32;
        let mut len = 1;
        if self.config.enabled {
            bits[len] = net::Capability::AudioControlV1 as i32;
            len += 1;
            if self
                .worker
                .as_ref()
                .is_some_and(|worker| worker.alive && worker.model_ready)
            {
                if available(self.ready.as_ref(), worker::Capability::SystemCapture) {
                    bits[len] = net::Capability::SystemAudioSource as i32;
                    len += 1;
                }
                if available(self.ready.as_ref(), worker::Capability::Render) {
                    bits[len] = net::Capability::AudioListener as i32;
                    len += 1;
                }
            }
        }
        let bits = &bits[..len];
        self.caps.send_if_modified(|old| {
            if old.as_slice() == bits {
                false
            } else {
                old.clear();
                old.extend_from_slice(bits);
                true
            }
        });
    }
    async fn run(mut self) {
        let _ = self.configure();
        self.publish_caps();
        self.publish();
        self.sync_links();
        self.drain().await;
        if self.config.enabled {
            let _ = self.ensure_worker().await;
            self.drain().await;
        }
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let attached = self.worker_events.is_some();
            let ticking = self.config.enabled || self.pending_focus();
            tokio::select! {
                _ = self.stopping.changed() => break,
                changed = self.focus_rx.changed() => {
                    if changed.is_err() { break; }
                    self.sync_focus();
                }
                changed = self.links.changed() => {
                    if changed.is_err() { break; }
                    self.dirty = true;
                    self.sync_links();
                }
                _ = tick.tick(), if ticking => self.tick().await,
                event = async { match self.worker_events.as_mut() { Some(events) => events.recv().await, None => std::future::pending().await } }, if attached => {
                    self.dirty = true;
                    if let Some(event) = event { self.on_worker(event).await; }
                    else {
                        self.worker_failed = true;
                        self.worker_lost();
                        self.worker = None;
                        self.worker_events = None;
                    }
                }
                command = self.commands.recv() => {
                    let Some(Command { parent, message }) = command else { break; };
                    self.dirty = true;
                    match message {
                        Ok(message) => self.on_network(parent,message),
                        Err(violation) if self.current(parent) => {
                            let reason = match violation.stop_code() {
                                net::AudioStopCode::UnsupportedFormat => fsm::Reason::UnsupportedFormat,
                                net::AudioStopCode::Disabled => fsm::Reason::Disabled,
                                net::AudioStopCode::RateLimited => fsm::Reason::RateLimited,
                                _ => fsm::Reason::ProtocolViolation,
                            };
                            let _ = self.feed(fsm::Input::Stop { peer:parent.peer,reason });
                        }
                        Err(_) => {}
                    }
                }
                request = self.gui_commands.recv() => {
                    let Some(request) = request else { break; };
                    self.dirty = true;
                    self.on_gui(request).await;
                }
            }
            self.drain().await;
            self.service_focus().await;
            self.apply_gains().await;
            self.publish_caps();
            self.publish();
            self.service_deferred().await;
        }
        self.stop_all(fsm::Reason::UserStop);
        self.drain().await;
        let _ = self.stop_worker().await;
    }
    fn sync_links(&mut self) {
        let snapshot = self.links.borrow_and_update().clone();
        if snapshot.overflow != self.overflow {
            self.overflow = snapshot.overflow;
            self.stop_all(fsm::Reason::RateLimited);
        }
        let removed: Vec<_> = self
            .peers
            .iter()
            .filter(|(id, p)| {
                snapshot
                    .peers
                    .get(id)
                    .is_none_or(|next| next.parent != p.parent)
            })
            .map(|(id, p)| (*id, p.parent))
            .collect();
        for (id, parent) in removed {
            let _ = self.feed(fsm::Input::PeerDown(parent));
            if self.focus_view.owner.is_some_and(|(owner, _)| owner == id) {
                self.focus_received = false;
                let now = self.now();
                // A restarted owner may retain its ownership serial, but not its revision.
                // Old-parent messages are rejected before reaching this fresh namespace.
                self.focus_policy.set_owner(now, None);
                self.focus_policy.set_owner(
                    now,
                    self.focus_view.owner.map(|(node, serial)| Owner {
                        node: *node.as_bytes(),
                        serial,
                    }),
                );
            }
            if let Some(record) = self.records.get_mut(&id) {
                record.view.phase = fsm::Phase::Failed;
                record.view.reason = Some(fsm::Reason::ParentLost);
                record.material = None;
                record.capture_active = None;
                record.sending = None;
            }
            self.peers.remove(&id);
            self.admitted.remove(&id);
        }
        for (id, mut peer) in snapshot.peers {
            let support_changed = self
                .peers
                .get(&id)
                .is_none_or(|previous| previous.support != peer.support);
            let trust_changed = self
                .peers
                .get(&id)
                .is_none_or(|previous| previous.trusted != peer.trusted);
            if let Some(previous) = self.peers.get(&id).filter(|p| p.parent == peer.parent) {
                peer.hello = previous.hello;
                peer.focus_sent_revision = previous.focus_sent_revision;
                peer.focus_request_at = previous.focus_request_at;
                peer.focus_reply_at = previous.focus_reply_at;
                peer.focus_reply = previous.focus_reply;
            }
            if self.admitted.contains(&id) {
                if !peer.trusted && trust_changed {
                    let _ = self.feed(fsm::Input::RevokePeer(*id.as_bytes()));
                } else if peer.trusted && support_changed {
                    let _ = self.feed(fsm::Input::PeerSupport {
                        parent: peer.parent,
                        support: peer.support,
                    });
                }
            } else if peer.support.control
                && peer.trusted
                && self
                    .feed(fsm::Input::PeerUp {
                        parent: peer.parent,
                        support: peer.support,
                        trusted: peer.trusted,
                        local_allowed: self.allowed(id),
                        consent_revision: self.consent_revision,
                    })
                    .is_ok()
            {
                self.admitted.insert(id);
                if self.configuration_error.is_none() {
                    if let Some(hello) = peer.hello {
                        let _ = self.feed(fsm::Input::Receive {
                            parent: peer.parent,
                            message: fsm::Control::Hello(hello),
                        });
                    }
                }
            }
            self.peers.insert(id, peer);
        }
        if let Some((owner, _)) = self.focus_view.owner {
            if owner != self.local
                && !self
                    .peers
                    .get(&owner)
                    .is_some_and(|p| p.trusted && p.focus_support)
            {
                self.focus_received = false;
                self.focus_policy.unknown(self.now());
            }
        }
    }
    async fn save(&self, config: config::AudioConfig) -> Result<(), Code> {
        let path = self.paths.config_file();
        tokio::task::spawn_blocking(move || config::audio::save_audio_locked(&path, config))
            .await
            .map_err(|_| ipc::audio::codes::AUDIO_CONFIG_WRITE)?
            .map(|_| ())
            .map_err(|_| ipc::audio::codes::AUDIO_CONFIG_WRITE)
    }
    async fn drain(&mut self) {
        while let Some(effect) = self.effects.pop_front() {
            match effect {
                fsm::Effect::PublishStatus => self.publish(),
                fsm::Effect::PublishConfiguration(settings) => {
                    if self.configuration_error.is_some() {
                        continue;
                    }
                    // FSM enabled is the activation gate; persisted enabled is explicit user
                    // opt-in and must survive pending Source group discovery.
                    if settings.group.is_some()
                        && settings.group != self.config.group_id.map(|id| id.0)
                    {
                        let mut next = self.config.clone();
                        next.group_id = settings.group.map(config::audio::HexId);
                        if self.save(next.clone()).await.is_ok() {
                            self.config = next;
                            let _ = self.configure();
                        } else {
                            self.configuration_error = Some(ipc::audio::codes::AUDIO_CONFIG_WRITE);
                            let _ = self.feed(fsm::Input::Configure(fsm::Settings::default()));
                            self.fail_pending(ipc::audio::codes::AUDIO_CONFIG_WRITE)
                                .await;
                        }
                    }
                }
                fsm::Effect::MintListenerGroup => {
                    if self.configuration_error.is_some() {
                        continue;
                    }
                    if !self.config.enabled || self.config.role != config::AudioRole::Listener {
                        continue;
                    }
                    if let Ok(group) = identity::random16() {
                        let mut next = self.config.clone();
                        next.group_id = Some(config::audio::HexId(group));
                        if self.save(next.clone()).await.is_ok() {
                            self.config = next;
                            let _ = self.feed(fsm::Input::ListenerGroupMinted(group));
                        } else {
                            self.configuration_error = Some(ipc::audio::codes::AUDIO_CONFIG_WRITE);
                            let _ = self.feed(fsm::Input::Configure(fsm::Settings::default()));
                            self.fail_pending(ipc::audio::codes::AUDIO_CONFIG_WRITE)
                                .await;
                        }
                    }
                }
                fsm::Effect::Send { parent, message } => self.send_control(parent, message).await,
                fsm::Effect::GenerateSourceNonce { parent, session } => {
                    if self.current(parent) {
                        if let Some(nonce) = nonce() {
                            let _ = self.feed(fsm::Input::SourceNonce {
                                parent,
                                session,
                                nonce,
                            });
                        }
                    }
                }
                fsm::Effect::GenerateListenerOffer { parent, request } => {
                    if self.current(parent) {
                        if let (Ok(session), Some(listener_nonce)) = (identity::random16(), nonce())
                        {
                            let format = format(request.profile);
                            let _ = self.feed(fsm::Input::Start {
                                parent,
                                session,
                                listener_nonce,
                                format,
                                format_sha256: wire::format_sha256(format),
                            });
                        }
                    }
                }
                fsm::Effect::StartRequestResult {
                    request_id, result, ..
                } => {
                    self.retry.retain(|(_, message)| {
                        !matches!(message,
                        fsm::Control::StartRequest(request) if request.request_id == request_id)
                    });
                    if let Some(pending) = self.pending_starts.remove(&request_id) {
                        reply(&pending.request, result.map(|_| ()).map_err(reason_code)).await;
                    }
                }
                fsm::Effect::DeriveKeys {
                    parent,
                    worker_generation,
                    agreement,
                } => {
                    if !self.current(parent)
                        || self.worker.as_ref().is_none_or(|w| {
                            !w.alive
                                || !w.model_ready
                                || generation(w.generation, self.worker_epoch) != worker_generation
                        })
                    {
                        continue;
                    }
                    let peer = NodeId::new(parent.peer);
                    let material = self.derive(parent, &agreement).await;
                    let Some(view) = self.model.session(parent.peer) else {
                        continue;
                    };
                    if let Some(material) = material {
                        if self.records.len() >= model::MAX_AUDIO_SESSIONS
                            && !self.records.contains_key(&peer)
                        {
                            self.records.retain(|_, r| {
                                !matches!(r.view.phase, fsm::Phase::Stopped | fsm::Phase::Failed)
                            });
                        }
                        if self.records.len() >= model::MAX_AUDIO_SESSIONS
                            && !self.records.contains_key(&peer)
                        {
                            let _ = self.feed(fsm::Input::ChildFailed {
                                view,
                                reason: fsm::Reason::RateLimited,
                            });
                            continue;
                        }
                        self.records.insert(
                            peer,
                            Record {
                                view,
                                agreement,
                                role: self.role(),
                                material: Some(material),
                                start_request_id: None,
                                health: Default::default(),
                                health_state: model::StreamHealth::Buffering,
                                capture_active: None,
                                sending: None,
                                applied_gain: None,
                            },
                        );
                        let mut transcript_sha256 = [0; 32];
                        transcript_sha256.copy_from_slice(
                            ring::digest::digest(
                                &ring::digest::SHA256,
                                &agreement.transcript_bytes(),
                            )
                            .as_ref(),
                        );
                        let _ = self.feed(fsm::Input::KeysReady {
                            view,
                            transcript_sha256,
                        });
                    } else {
                        let _ = self.feed(fsm::Input::ChildFailed {
                            view,
                            reason: fsm::Reason::ParentLost,
                        });
                    }
                }
                fsm::Effect::PrepareChild { view, .. } => {
                    if self.current(view.parent)
                        && self
                            .records
                            .get(&NodeId::new(view.parent.peer))
                            .is_some_and(|r| r.view.session == view.session && r.material.is_some())
                    {
                        let _ = self.feed(fsm::Input::ChildPrepared(view));
                    }
                }
                fsm::Effect::StartChild { view } => self.start_child(view).await,
                fsm::Effect::InvalidateSession { view } => {
                    self.retry.retain(|(parent, message)| {
                        *parent != view.parent || control_session(message) != Some(view.session)
                    });
                    if let Some(record) = self
                        .records
                        .get_mut(&NodeId::new(view.parent.peer))
                        .filter(|r| r.view.session == view.session)
                    {
                        record.material = None;
                        record.capture_active = None;
                        record.sending = None;
                    }
                }
                fsm::Effect::StopChild { view } => {
                    if let Some(worker) = self.worker.as_ref().filter(|w| {
                        w.alive
                            && generation(w.generation, self.worker_epoch) == view.worker_generation
                    }) {
                        let _ = worker.handle.stop_session(view.session).await;
                    }
                }
                fsm::Effect::RenewChild {
                    view,
                    authorized_for_ms,
                } => {
                    if self.current(view.parent) {
                        if let Some(worker) = &self.worker {
                            if worker.alive
                                && worker.model_ready
                                && generation(worker.generation, self.worker_epoch)
                                    == view.worker_generation
                            {
                                let _ = worker
                                    .handle
                                    .renew_session(view.session, authorized_for_ms)
                                    .await;
                            }
                        }
                    }
                }
            }
        }
        self.sync_views();
    }
    async fn send_control(&mut self, parent: fsm::Parent, message: fsm::Control) {
        if !self.config.enabled || !self.current(parent) {
            return;
        }
        supersede_snapshot(&mut self.retry, parent, &message);
        if self.retry.iter().any(|(p, _)| *p == parent) {
            enqueue_retry(&mut self.retry, parent, message);
            return;
        }
        let Ok(encoded) = wire::encode_control(
            &message,
            self.role(),
            self.config.media_port,
            parent.connection_id,
        ) else {
            return;
        };
        if self
            .manager
            .send_audio_control(&NodeId::new(parent.peer), encoded)
            .await
            .is_err()
        {
            // A local capability announcement becomes usable only after its writer ACK.
            // Keep bounded metadata for that transient, never bypass the wire gate.
            enqueue_retry(&mut self.retry, parent, message);
        }
    }
    async fn derive(&self, parent: fsm::Parent, agreement: &fsm::Agreement) -> Option<Materials> {
        if agreement.offer.parent_connection_id != parent.connection_id || !self.current(parent) {
            return None;
        }
        let secret = self
            .manager
            .auxiliary_secret(&NodeId::new(parent.peer))
            .await?;
        let binding = secret.binding();
        if binding.connection_id != parent.connection_id
            || binding.generation != parent.generation
            || binding.peer != NodeId::new(parent.peer)
        {
            return None;
        }
        let context = agreement.context().encode();
        let material = Materials {
            forward: secret.derive(AuxPurpose::AudioForwardKey, &context).ok()?,
            reverse: secret.derive(AuxPurpose::AudioReverseKey, &context).ok()?,
            forward_nonce: secret
                .derive(AuxPurpose::AudioForwardNonce, &context)
                .ok()?,
            reverse_nonce: secret
                .derive(AuxPurpose::AudioReverseNonce, &context)
                .ok()?,
        };
        self.current(parent).then_some(material)
    }
    async fn start_child(&mut self, view: fsm::SessionView) {
        if !self.current(view.parent) {
            return;
        }
        let peer_id = NodeId::new(view.parent.peer);
        let Some(peer) = self.peers.get(&peer_id) else {
            return;
        };
        let remote = peer.remote;
        let Some(record) = self.records.get_mut(&peer_id).filter(|r| {
            r.view.session == view.session && r.view.worker_generation == view.worker_generation
        }) else {
            return;
        };
        let Some(material) = record.material.take() else {
            return;
        };
        let role = record.role;
        let agreement = record.agreement;
        let (forward, reverse) = material.into_wire();
        let capture = if role == model::AudioRole::Source {
            Some(worker::CaptureSpec {
                kind: if cfg!(windows) {
                    worker::CaptureKind::SystemExcludingOwnProcessTree as i32
                } else {
                    worker::CaptureKind::Monitor as i32
                },
                device: if cfg!(windows) {
                    None
                } else {
                    Some(match &self.config.capture_device {
                        config::SystemCaptureSelector::DefaultMonitor {} => {
                            device(&config::AudioDeviceSelector::default())
                        }
                        config::SystemCaptureSelector::StableId { id } => worker::DeviceSelection {
                            selector: Some(worker::device_selection::Selector::DeviceId(
                                id.clone(),
                            )),
                        },
                    })
                },
            })
        } else {
            None
        };
        let start = worker::StartSession {
            session_id: view.session.to_vec(),
            role: match role {
                model::AudioRole::Source => worker::Role::Source as i32,
                model::AudioRole::Listener => worker::Role::Listener as i32,
            },
            profile: match agreement.offer.format.profile {
                Profile::Ethernet => worker::Profile::Ethernet as i32,
                Profile::Wifi => worker::Profile::Wifi as i32,
            },
            capture,
            render: (role == model::AudioRole::Listener)
                .then(|| device(&self.config.output_device)),
            local_bind_ip: self.listen.ip().to_string(),
            local_udp_port: u32::from(if role == model::AudioRole::Listener {
                agreement.offer.listener_port
            } else {
                agreement.source_port
            }),
            peer_ip: remote.ip().to_string(),
            peer_udp_port: u32::from(if role == model::AudioRole::Listener {
                agreement.source_port
            } else {
                agreement.offer.listener_port
            }),
            forward: Some(forward),
            reverse: Some(reverse),
            local_scope_id: scope(self.listen),
            peer_scope_id: scope(remote),
            initial_authorization_ms: fsm::CHILD_TTL_MS,
        };
        let result = match &self.worker {
            Some(worker)
                if worker.alive
                    && worker.model_ready
                    && generation(worker.generation, self.worker_epoch)
                        == view.worker_generation =>
            {
                worker.handle.start_session(start).await
            }
            _ => return,
        };
        if let Ok(request_id) = &result {
            if let Some(record) = self.records.get_mut(&peer_id) {
                record.start_request_id = Some(*request_id);
                record.applied_gain = None;
            }
        }
        if result.is_err() {
            let _ = self.feed(fsm::Input::ChildFailed {
                view,
                reason: fsm::Reason::DeviceUnavailable,
            });
        } else if !self.current(view.parent) {
            if let Some(worker) = &self.worker {
                let _ = worker.handle.stop_session(view.session).await;
            }
        }
        // Native SessionEvent::Started, never this write ACK, calls ChildStarted.
    }
    async fn ensure_worker(&mut self) -> Result<(), Code> {
        if !self.config.enabled {
            return Err(ipc::audio::codes::AUDIO_UNAVAILABLE);
        }
        if self.worker.is_some() {
            return Ok(());
        }
        let program = std::env::current_exe().map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?;
        self.worker_failed = true;
        self.worker_epoch = self
            .worker_epoch
            .checked_add(1)
            .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE)?;
        let (handle, mut events) = AudioSupervisor::start(AudioSupervisorConfig {
            program,
            private_dir: self
                .paths
                .runtime_dir()
                .unwrap_or(self.paths.state_dir())
                .join("audio"),
            backend: if self.test_dry {
                worker::BackendKind::Dry
            } else {
                worker::BackendKind::Native
            },
        })
        .await
        .map_err(supervisor_code)?;
        let first = timeout(Duration::from_secs(1), events.recv())
            .await
            .map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?
            .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE)?;
        let AudioSupervisorEvent::Ready {
            generation: gen,
            pid,
            ready,
        } = first
        else {
            return Err(ipc::audio::codes::AUDIO_UNAVAILABLE);
        };
        self.worker = Some(Worker {
            handle: handle.clone(),
            generation: gen,
            alive: true,
            model_ready: false,
            pid,
            probe_ack: 0,
        });
        let _ = ready;
        self.ready = None;
        self.worker_events = Some(events);
        self.worker_failed = false;
        let _ = handle.probe().await.map_err(supervisor_code)?;
        Ok(())
    }
    fn probed(&mut self, gen: u64, request_id: u64, ready: worker::Ready) {
        let Some(worker) = self
            .worker
            .as_mut()
            .filter(|w| w.alive && w.generation == gen && request_id > w.probe_ack)
        else {
            return;
        };
        worker.probe_ack = request_id;
        let first = !worker.model_ready;
        let capture = available(Some(&ready), worker::Capability::SystemCapture);
        let render = available(Some(&ready), worker::Capability::Render);
        let input = if first {
            fsm::Input::WorkerUp {
                generation: generation(gen, self.worker_epoch),
                capture,
                render,
            }
        } else {
            fsm::Input::WorkerCapabilities {
                generation: generation(gen, self.worker_epoch),
                capture,
                render,
            }
        };
        if self.feed(input).is_err() {
            return;
        }
        if let Some(worker) = self.worker.as_mut() {
            worker.model_ready = true;
        }
        self.ready = Some(ready);
        self.dirty = true;
        self.publish_caps();
    }
    fn worker_lost(&mut self) {
        if let Some(worker) = &mut self.worker {
            let gen = worker.generation;
            worker.model_ready = false;
            worker.alive = false;
            let _ = self.feed(fsm::Input::WorkerLost {
                generation: generation(gen, self.worker_epoch),
            });
        }
        self.ready = None;
        self.publish_caps();
        for record in self.records.values_mut() {
            record.material = None;
            record.capture_active = None;
            record.sending = None;
        }
    }
    async fn stop_worker(&mut self) -> Result<(), Code> {
        self.worker_lost();
        let Some(worker) = self.worker.take() else {
            self.worker_events = None;
            return Ok(());
        };
        let result = worker.handle.shutdown().await;
        drop(worker);
        if result.is_err() {
            if let Some(events) = self.worker_events.as_mut() {
                timeout(Duration::from_secs(6), async {
                    while events.recv().await.is_some() {}
                })
                .await
                .map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?;
            }
        }
        self.worker_events = None;
        Ok(())
    }
    async fn on_worker(&mut self, event: AudioSupervisorEvent) {
        match event {
            AudioSupervisorEvent::Ready {
                generation: gen,
                pid,
                ready: _,
            } => {
                if self.worker.as_ref().is_none_or(|w| gen <= w.generation) {
                    return;
                }
                self.worker_lost();
                if let Some(worker) = self.worker.as_mut() {
                    worker.generation = gen;
                    worker.alive = true;
                    worker.pid = pid;
                    worker.probe_ack = 0;
                }
                if self.config.enabled {
                    if let Some(worker) = &self.worker {
                        let _ = worker.handle.probe().await;
                    }
                }
            }
            AudioSupervisorEvent::Probed {
                generation: gen,
                request_id,
                ready,
            } => {
                self.probed(gen, request_id, ready);
            }
            AudioSupervisorEvent::ChildExited {
                generation: gen,
                pid,
                restartable,
            } => {
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.generation == gen && w.pid == pid)
                {
                    self.worker_lost();
                    if !restartable {
                        self.worker_failed = true;
                        self.worker_events = None;
                        self.worker = None;
                    }
                }
            }
            AudioSupervisorEvent::Failed {
                generation: gen,
                session_id,
                reason,
            } => {
                if let Some(id) = session_id {
                    if let Some(view) = self
                        .records
                        .values()
                        .find(|r| {
                            r.view.session == id
                                && r.view.worker_generation == generation(gen, self.worker_epoch)
                        })
                        .map(|r| r.view)
                    {
                        let reason = if reason == AudioSupervisorError::Timeout {
                            fsm::Reason::Timeout
                        } else {
                            fsm::Reason::WorkerLost
                        };
                        let _ = self.feed(fsm::Input::ChildFailed { view, reason });
                    }
                } else if self.worker.as_ref().is_some_and(|w| w.generation == gen) {
                    self.worker_lost();
                }
            }
            AudioSupervisorEvent::SessionEvent {
                generation: gen,
                event,
            } => {
                let Ok(id) = <[u8; 16]>::try_from(event.session_id.as_slice()) else {
                    return;
                };
                let Some((peer, view)) = self
                    .records
                    .iter()
                    .find(|(_, r)| {
                        r.view.session == id
                            && r.view.worker_generation == generation(gen, self.worker_epoch)
                    })
                    .map(|(p, r)| (*p, r.view))
                else {
                    if event.kind == worker::EventKind::Started as i32 {
                        if let Some(worker) = self
                            .worker
                            .as_ref()
                            .filter(|w| w.alive && w.generation == gen)
                        {
                            let _ = worker.handle.stop_session(id).await;
                        }
                    }
                    return;
                };
                match worker::EventKind::try_from(event.kind) {
                    Ok(worker::EventKind::Started) => {
                        let port_matches = self.records.get(&peer).is_some_and(|record| {
                            event.local_udp_port
                                == u32::from(if record.role == model::AudioRole::Listener {
                                    record.agreement.offer.listener_port
                                } else {
                                    record.agreement.source_port
                                })
                        });
                        let valid = port_matches
                            && view.phase == fsm::Phase::Committed
                            && self
                                .worker
                                .as_ref()
                                .is_some_and(|w| w.alive && w.model_ready && w.generation == gen)
                            && self.current(view.parent);
                        if !valid {
                            if let Some(worker) = &self.worker {
                                let _ = worker.handle.stop_session(id).await;
                            }
                            return;
                        }
                        if self
                            .records
                            .get(&peer)
                            .is_some_and(|record| record.role == model::AudioRole::Listener)
                        {
                            let (gain, muted) = self.gain(peer);
                            let applied = match &self.worker {
                                Some(worker) => {
                                    worker.handle.set_gain(id, gain, muted).await.is_ok()
                                }
                                None => false,
                            };
                            if !applied {
                                let _ = self.feed(fsm::Input::ChildFailed {
                                    view,
                                    reason: fsm::Reason::WorkerLost,
                                });
                                return;
                            }
                            if let Some(record) = self.records.get_mut(&peer) {
                                record.applied_gain = Some((gain, muted));
                            }
                        }
                        // Establish Listener gain before Commit can start the Source.
                        if !self.current(view.parent)
                            || self.feed(fsm::Input::ChildStarted(view)).is_err()
                        {
                            if let Some(worker) = &self.worker {
                                let _ = worker.handle.stop_session(id).await;
                            }
                        }
                    }
                    Ok(worker::EventKind::Stopped) => {
                        if view.phase == fsm::Phase::Stopping {
                            let _ = self.feed(fsm::Input::ChildStopped(view));
                        } else if !matches!(
                            view.phase,
                            fsm::Phase::Stopped | fsm::Phase::Failed | fsm::Phase::DevicePaused
                        ) {
                            let reason = if event.reason == worker::StopReason::PathTimeout as i32 {
                                fsm::Reason::Timeout
                            } else {
                                fsm::Reason::WorkerLost
                            };
                            let _ = self.feed(fsm::Input::ChildFailed { view, reason });
                        }
                        if let Some(record) = self.records.get_mut(&peer) {
                            record.capture_active = Some(false);
                            record.sending = Some(false);
                        }
                    }
                    Ok(worker::EventKind::PathValidated) => {
                        if self.current(view.parent) {
                            let _ = self.feed(fsm::Input::PathReady(view));
                        }
                    }
                    Ok(worker::EventKind::DeviceInvalidated) => {
                        let _ = self.feed(fsm::Input::DeviceInvalidated(view));
                    }
                    Ok(worker::EventKind::RotationRequired) => {
                        let _ = self.feed(fsm::Input::ChildFailed {
                            view,
                            reason: fsm::Reason::Timeout,
                        });
                    }
                    Ok(worker::EventKind::Degraded) => {
                        let _ = self.feed(fsm::Input::Buffering(view));
                    }
                    _ => {}
                }
            }
            AudioSupervisorEvent::Health {
                generation: gen,
                health,
            } => {
                if self
                    .worker
                    .as_ref()
                    .is_none_or(|w| w.generation != gen || !w.alive || !w.model_ready)
                {
                    return;
                }
                for health in health.sessions {
                    let Ok(id) = <[u8; 16]>::try_from(health.session_id.as_slice()) else {
                        continue;
                    };
                    let Some(peer) = self
                        .records
                        .iter()
                        .find(|(_, r)| {
                            r.view.session == id
                                && r.view.worker_generation == generation(gen, self.worker_epoch)
                        })
                        .map(|(p, _)| *p)
                    else {
                        continue;
                    };
                    let Some(record) = self.records.get_mut(&peer) else {
                        continue;
                    };
                    if let Some(c) = health.counters {
                        record.health = model::AudioHealth {
                            received_packets: c.received_packets,
                            lost_frames: c.lost_frames,
                            late_frames: c.late_frames,
                            concealed_frames: c.concealed_frames,
                            underruns: c.underruns,
                            discontinuities: c.discontinuities,
                            limiter_frames: c.limiter_frames,
                        };
                    }
                    record.health_state = match worker::StreamHealth::try_from(health.state) {
                        Ok(worker::StreamHealth::Ready) => model::StreamHealth::Ready,
                        Ok(worker::StreamHealth::Degraded) => model::StreamHealth::Degraded,
                        _ => model::StreamHealth::Buffering,
                    };
                    record.capture_active = health.capture_active;
                    record.sending = health.sending;
                    let view = record.view;
                    if record.health_state == model::StreamHealth::Ready
                        && view.phase == fsm::Phase::Rebuffering
                    {
                        let _ = self.feed(fsm::Input::PathReady(view));
                    } else if record.health_state != model::StreamHealth::Ready
                        && view.phase == fsm::Phase::Active
                    {
                        let _ = self.feed(fsm::Input::Buffering(view));
                    }
                }
            }
            AudioSupervisorEvent::BackendEvent {
                generation: gen,
                event,
            } => {
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == gen)
                {
                    let relevant = event.direction
                        == match self.role() {
                            model::AudioRole::Source => worker::Direction::Capture as i32,
                            model::AudioRole::Listener => worker::Direction::Render as i32,
                        };
                    if relevant
                        && matches!(
                            worker::BackendEventKind::try_from(event.kind),
                            Ok(worker::BackendEventKind::Invalidated
                                | worker::BackendEventKind::Removed
                                | worker::BackendEventKind::FormatChanged)
                        )
                    {
                        let views: Vec<_> = self.records.values().map(|r| r.view).collect();
                        for view in views {
                            let _ = self.feed(fsm::Input::DeviceInvalidated(view));
                        }
                    }
                    if let Some(worker) = &self.worker {
                        let _ = worker.handle.probe().await;
                    }
                }
            }
            AudioSupervisorEvent::CommandRejected {
                generation: gen,
                error,
                request_id,
            } => {
                if let Ok(id) = <[u8; 16]>::try_from(error.session_id.as_slice()) {
                    if let Some(view) = self
                        .records
                        .values()
                        .find(|r| {
                            r.view.session == id
                                && r.view.worker_generation == generation(gen, self.worker_epoch)
                                && r.start_request_id.is_some_and(|start| request_id >= start)
                                && !matches!(r.view.phase, fsm::Phase::Stopped | fsm::Phase::Failed)
                        })
                        .map(|r| r.view)
                    {
                        let _ = self.feed(fsm::Input::ChildFailed {
                            view,
                            reason: fsm::Reason::DeviceUnavailable,
                        });
                    }
                }
            }
        }
    }
    async fn tick(&mut self) {
        self.sync_links();
        if !self.config.enabled {
            return;
        }
        if let Some(worker) = &self.worker {
            if worker.alive && worker.model_ready {
                // The supervisor validates child Pongs and owns its liveness timeout. This
                // refreshes model supervision only; native capture/send facts come from Health.
                let _ = self.feed(fsm::Input::WorkerHeartbeat {
                    generation: generation(worker.generation, self.worker_epoch),
                });
            }
        }
        let _ = self.feed(fsm::Input::Tick);
        let retry = std::mem::take(&mut self.retry);
        for (parent, message) in retry {
            self.send_control(parent, message).await;
        }
        if self.worker.is_none() && !self.worker_failed {
            let _ = self.ensure_worker().await;
        }
        let now = self.now();
        let expired: Vec<_> = self
            .pending_starts
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(p) = self.pending_starts.remove(&id) {
                reply(&p.request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            }
        }
    }
    fn follow_focus(&self) -> bool {
        self.config.enabled
            && self.config.role == config::AudioRole::Listener
            && self.config.mode == config::AudioMode::FollowFocus
    }
    fn focus_members(&self) -> Option<([[u8; 32]; 4], usize)> {
        let mut members = [[0; 32]; 4];
        members[0] = *self.local.as_bytes();
        let mut len = 1;
        for (id, _) in self.peers.iter().filter(|(_, peer)| peer.trusted) {
            *members.get_mut(len)? = *id.as_bytes();
            len += 1;
        }
        Some((members, len))
    }
    fn refresh_local_focus(&mut self) {
        self.focus_snapshot = None;
        let Some((node, serial)) = self.focus_view.owner.filter(|(id, _)| *id == self.local) else {
            return;
        };
        let now = self.now();
        let Some(revision) = self.focus_revision.checked_add(1) else {
            self.focus_policy.unknown(now);
            return;
        };
        self.focus_revision = revision;
        let snapshot = Snapshot {
            owner: Owner {
                node: *node.as_bytes(),
                serial,
            },
            revision,
            active: self.focus_view.active.map(|id| *id.as_bytes()),
            lease: self.focus_view.lease,
            committed: self.focus_view.committed,
        };
        if let Some((members, len)) = self.focus_members() {
            if self
                .focus_policy
                .accept(now, snapshot.owner.node, snapshot, &members[..len])
            {
                self.focus_snapshot = Some(snapshot);
                self.focus_received = true;
                return;
            }
        }
        self.focus_policy.unknown(now);
    }
    fn sync_focus(&mut self) {
        let view = *self.focus_rx.borrow_and_update();
        if view == self.focus_view {
            return;
        }
        let now = self.now();
        if view.owner != self.focus_view.owner {
            self.focus_received = false;
            self.focus_snapshot = None;
        }
        self.focus_view = view;
        let owner = view.owner.map(|(node, serial)| Owner {
            node: *node.as_bytes(),
            serial,
        });
        self.focus_policy.set_owner(now, owner);
        if owner.is_some_and(|o| o.node == *self.local.as_bytes()) {
            self.refresh_local_focus();
        } else if owner.is_none() {
            self.focus_policy.unknown(now);
        }
        self.dirty = true;
    }
    fn on_focus_network(&mut self, parent: fsm::Parent, message: &net::AudioControl) {
        self.sync_focus();
        if !self
            .peers
            .get(&NodeId::new(parent.peer))
            .is_some_and(|p| p.trusted && p.focus_support && p.parent == parent)
            || !self
                .links
                .borrow()
                .peers
                .get(&NodeId::new(parent.peer))
                .is_some_and(|p| p.trusted && p.focus_support && p.parent == parent)
        {
            return;
        }
        match wire::decode_focus(message) {
            Ok(wire::FocusControl::Snapshot(snapshot)) => {
                if self.config.enabled
                    && self.config.role == config::AudioRole::Listener
                    && self
                        .focus_view
                        .owner
                        .is_some_and(|(owner, _)| *owner.as_bytes() == parent.peer)
                {
                    if let Some((members, len)) = self.focus_members() {
                        if self.focus_policy.accept(
                            self.now(),
                            parent.peer,
                            snapshot,
                            &members[..len],
                        ) {
                            self.focus_received = true;
                            self.dirty = true;
                        }
                    }
                }
            }
            Ok(wire::FocusControl::Request(_)) => {
                if self
                    .focus_view
                    .owner
                    .is_some_and(|(owner, _)| owner == self.local)
                {
                    let now = self.now();
                    if let Some(peer) = self.peers.get_mut(&NodeId::new(parent.peer)) {
                        if elapsed_second(peer.focus_reply_at, now) {
                            peer.focus_reply = true;
                            peer.focus_reply_at = Some(now);
                            tracing::debug!(peer = %NodeId::new(parent.peer), "audio focus snapshot request received");
                        }
                    }
                }
            }
            Err(_) => {}
        }
    }
    fn pending_focus(&self) -> bool {
        self.focus_view
            .owner
            .is_some_and(|(owner, _)| owner == self.local)
            && self.focus_snapshot.is_some_and(|snapshot| {
                self.peers.values().any(|peer| {
                    peer.trusted
                        && peer.focus_support
                        && (peer.focus_reply || peer.focus_sent_revision != snapshot.revision)
                })
            })
    }

    async fn service_focus(&mut self) {
        self.sync_focus();
        let now = self.now();
        let previous_primary = self.focus_policy.primary();
        self.focus_policy.tick(now);
        self.dirty |= previous_primary != self.focus_policy.primary();
        let local_owner = self
            .focus_view
            .owner
            .is_some_and(|(owner, _)| owner == self.local);
        let owner = self.focus_view.owner.map(|(id, _)| id);
        let group = self.config.group_id.map(|id| id.0);
        let request =
            self.config.enabled && self.config.role == config::AudioRole::Listener && !local_owner;
        if local_owner
            && self.peers.values().any(|peer| {
                peer.trusted
                    && peer.focus_support
                    && (peer.focus_reply || peer.focus_sent_revision == 0)
            })
        {
            self.refresh_local_focus();
        }
        for (id, peer) in &mut self.peers {
            if !peer.trusted || !peer.focus_support {
                continue;
            }
            if local_owner {
                if let Some(snapshot) = self.focus_snapshot {
                    if (peer.focus_reply || peer.focus_sent_revision != snapshot.revision)
                        && self
                            .manager
                            .send_audio_control(id, wire::encode_focus(snapshot))
                            .await
                            .is_ok()
                    {
                        peer.focus_sent_revision = snapshot.revision;
                        peer.focus_reply = false;
                    }
                }
            } else if request
                && owner == Some(*id)
                && (!self.focus_received || peer.focus_request_at.is_none())
                && elapsed_second(peer.focus_request_at, now)
            {
                if let Some(group) = group {
                    peer.focus_request_at = Some(now);
                    if self
                        .manager
                        .send_audio_control(id, wire::encode_focus_request(group))
                        .await
                        .is_ok()
                    {
                        tracing::debug!(peer = %id, "audio focus snapshot requested");
                    }
                }
            }
        }
    }
    async fn apply_gains(&mut self) {
        if !self.config.enabled || self.config.role != config::AudioRole::Listener {
            return;
        }
        let Some(worker) = self.worker.as_ref().filter(|w| w.alive && w.model_ready) else {
            return;
        };
        for (peer, record) in &mut self.records {
            if record.role != model::AudioRole::Listener
                || !matches!(
                    record.view.phase,
                    fsm::Phase::Active | fsm::Phase::Rebuffering
                )
            {
                continue;
            }
            let (gain, muted) = self
                .config
                .peers
                .iter()
                .find(|p| p.node_id.0 == *peer.as_bytes())
                .map_or((100, false), |p| (p.gain_percent, p.muted));
            let factor = if self.config.mode == config::AudioMode::FollowFocus {
                self.focus_policy.gain_percent(*peer.as_bytes())
            } else {
                100
            };
            let target = (effective_gain(gain, factor), muted);
            if record.applied_gain != Some(target)
                && worker
                    .handle
                    .set_gain(record.view.session, target.0, target.1)
                    .await
                    .is_ok()
            {
                record.applied_gain = Some(target);
                self.dirty = true;
            }
        }
    }

    fn on_network(&mut self, parent: fsm::Parent, message: net::AudioControl) {
        if wire::is_focus(&message) {
            self.on_focus_network(parent, &message);
            return;
        }
        if !self.config.enabled || self.configuration_error.is_some() || !self.current(parent) {
            return;
        }
        let peer = NodeId::new(parent.peer);
        let remote_role = self
            .peers
            .get(&peer)
            .and_then(|p| p.hello)
            .map(|h| h.role)
            .unwrap_or(match self.role() {
                model::AudioRole::Source => model::AudioRole::Listener,
                model::AudioRole::Listener => model::AudioRole::Source,
            });
        // Hello announces a role transition on an existing authenticated link;
        // the previous cached role must not prevent learning its replacement.
        let remote_role = if let Some(net::audio_control::Body::Hello(hello)) = &message.body {
            match net::AudioNodeRole::try_from(hello.role) {
                Ok(net::AudioNodeRole::Source) => model::AudioRole::Source,
                Ok(net::AudioNodeRole::Listener) => model::AudioRole::Listener,
                _ => return,
            }
        } else {
            remote_role
        };
        match wire::decode_control(&message, remote_role, parent.connection_id) {
            Ok(wire::DecodedControl::Control { message, .. }) => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    match message {
                        fsm::Control::Hello(mut hello) => {
                            if let Some(previous) = p.hello {
                                let same_scope = previous.role == hello.role
                                    && previous.group == hello.group
                                    && previous.listener == hello.listener;
                                if same_scope {
                                    if hello.allowed
                                        && hello.consent_revision <= previous.consent_revision
                                    {
                                        hello.allowed = previous.allowed;
                                    }
                                    hello.consent_revision =
                                        hello.consent_revision.max(previous.consent_revision);
                                }
                            }
                            p.hello = Some(hello);
                        }
                        fsm::Control::Consent {
                            allowed, revision, ..
                        } => {
                            if let Some(hello) = &mut p.hello {
                                if !allowed || revision > hello.consent_revision {
                                    hello.allowed = allowed;
                                }
                                hello.consent_revision = hello.consent_revision.max(revision);
                            }
                        }
                        _ => {}
                    }
                }
                let _ = self.feed(fsm::Input::Receive { parent, message });
            }
            Ok(wire::DecodedControl::Report(_)) => {}
            Err(_) => {
                let _ = self.feed(fsm::Input::Stop {
                    peer: parent.peer,
                    reason: fsm::Reason::ProtocolViolation,
                });
            }
        }
    }
    fn stop_all(&mut self, reason: fsm::Reason) {
        let peers: Vec<_> = self.peers.keys().copied().collect();
        for peer in peers {
            let _ = self.feed(fsm::Input::Stop {
                peer: *peer.as_bytes(),
                reason,
            });
        }
        self.retry.clear();
    }
    async fn fail_pending(&mut self, code: Code) {
        for (_, p) in std::mem::take(&mut self.pending_starts) {
            reply(&p.request, Err(code)).await;
        }
        for (_, p) in std::mem::take(&mut self.deferred_starts) {
            reply(&p.request, Err(code)).await;
        }
    }
    async fn on_gui(&mut self, request: Request) {
        use gui::gui_frame::Msg;
        match &request.message {
            Msg::GetAudioStatus(_) => {
                if let Some(status) = self.snapshot() {
                    answer(&request, Msg::AudioStatus(status)).await;
                } else {
                    reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                }
            }
            Msg::SetAudio(set) => {
                let result = self.set_config(set).await;
                self.drain().await;
                self.publish_caps();
                self.publish();
                reply(&request, result).await;
            }
            Msg::StartAudio(_) => self.start_request(request).await,
            Msg::StopAudio(stop) => {
                let result = if stop.stop_all {
                    self.stop_all(fsm::Reason::UserStop);
                    Ok(())
                } else if let (Ok(peer), Ok(id)) = (
                    <[u8; 32]>::try_from(stop.peer_node_id.as_slice()),
                    <[u8; 16]>::try_from(stop.session_id.as_slice()),
                ) {
                    if self
                        .records
                        .get(&NodeId::new(peer))
                        .is_some_and(|r| r.view.session == id)
                    {
                        self.feed(fsm::Input::Stop {
                            peer,
                            reason: fsm::Reason::UserStop,
                        })
                    } else {
                        Err(ipc::audio::codes::AUDIO_STALE_MEDIA)
                    }
                } else {
                    Err(ipc::audio::codes::AUDIO_UNAVAILABLE)
                };
                self.drain().await;
                self.publish();
                reply(&request, result).await;
            }
            Msg::SetAudioPeerGain(set) => {
                let result = self.set_gain(set).await;
                self.publish();
                reply(&request, result).await;
            }
            Msg::ListAudioDevices(list) => {
                if !self.config.enabled {
                    reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                    return;
                }
                let direction = match gui::AudioDeviceKind::try_from(list.kind) {
                    Ok(gui::AudioDeviceKind::Output) => worker::Direction::Render,
                    Ok(gui::AudioDeviceKind::Capture) => worker::Direction::Capture,
                    _ => {
                        reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                        return;
                    }
                };
                let result = match &self.worker {
                    Some(worker) => worker.handle.list_devices(direction).await,
                    _ => {
                        reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                        return;
                    }
                };
                match result {
                    Ok(devices) => {
                        let devices: Vec<_> = devices
                            .into_iter()
                            .map(|d| gui::AudioDevice {
                                id: d.id,
                                name: d.name,
                                is_default: d.is_default,
                                available: true,
                            })
                            .collect();
                        let page = gui::AudioDevices {
                            kind: list.kind,
                            revision: self.revision,
                            devices,
                            complete: true,
                        };
                        answer(&request, Msg::AudioDevices(page)).await;
                    }
                    Err(_) => {
                        reply(&request, Err(ipc::audio::codes::AUDIO_DEVICE_UNAVAILABLE)).await
                    }
                }
            }
            Msg::SetMicrophoneMute(_) | Msg::SetPtt(_) | Msg::SendMediaCommand(_) => {
                reply(&request, Err(ipc::audio::codes::AUDIO_UNSUPPORTED_COMMAND)).await
            }
            _ => reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await,
        }
    }
    async fn set_config(&mut self, set: &gui::SetAudio) -> Result<(), Code> {
        if set.expected_revision != self.revision {
            return Err(ipc::audio::codes::AUDIO_REVISION_CONFLICT);
        }
        let trusted: BTreeSet<_> = self
            .links
            .borrow()
            .trusted
            .iter()
            .map(|id| config::audio::HexId(*id.as_bytes()))
            .collect();
        let mut next = wire::decode_config(
            set.config
                .as_ref()
                .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE)?,
            &self.config,
            *self.local.as_bytes(),
            &trusted,
            model::AudioProfile::Ethernet,
        )
        .map_err(|error| match error {
            wire::Error::Unsupported => ipc::audio::codes::AUDIO_UNSUPPORTED_COMMAND,
            _ => ipc::audio::codes::AUDIO_ROLE_CONFLICT,
        })?;
        if next.enabled && (self.listen.ip().is_unspecified() || self.listen.ip().is_multicast()) {
            return Err(ipc::audio::codes::AUDIO_DEVICE_UNAVAILABLE);
        }
        if next.enabled && next.role == config::AudioRole::Listener && next.group_id.is_none() {
            next.group_id = Some(config::audio::HexId(
                identity::random16().map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?,
            ));
        }
        let previous = self.config.clone();
        let scope_changed = next.role != previous.role
            || next.group_id != previous.group_id
            || next.listener_node_id != previous.listener_node_id
            || next.media_port != previous.media_port
            || next.profile != previous.profile
            || next.output_device != previous.output_device
            || next.capture_device != previous.capture_device
            || !next.enabled;
        if scope_changed {
            self.stop_all(fsm::Reason::ConsentRequired);
        } else {
            let withdrawn: Vec<_> = self
                .peers
                .keys()
                .copied()
                .filter(|peer| permitted(&previous, *peer) && !permitted(&next, *peer))
                .collect();
            for peer in withdrawn {
                let _ = self.feed(fsm::Input::Stop {
                    peer: *peer.as_bytes(),
                    reason: fsm::Reason::ConsentRequired,
                });
            }
        }
        self.drain().await;
        if let Err(code) = self.save(next.clone()).await {
            self.configuration_error = Some(code);
            let _ = self.feed(fsm::Input::Configure(fsm::Settings::default()));
            self.drain().await;
            let _ = self.stop_worker().await;
            self.fail_pending(code).await;
            return Err(code);
        }
        self.config = next;
        self.configuration_error = None;
        self.worker_failed = false;
        self.publish_caps();
        if scope_changed {
            self.records.clear();
            let _ = self.stop_worker().await;
        }
        if let Err(error) = self.configure() {
            self.config = previous;
            if let Err(code) = self.save(self.config.clone()).await {
                self.configuration_error = Some(code);
                let _ = self.feed(fsm::Input::Configure(fsm::Settings::default()));
                self.drain().await;
                let _ = self.stop_worker().await;
                self.fail_pending(code).await;
                return Err(code);
            }
            let _ = self.configure();
            self.publish_caps();
            return Err(error);
        }
        self.drain().await;
        if !self.config.enabled {
            self.fail_pending(ipc::audio::codes::AUDIO_UNAVAILABLE)
                .await;
            self.stop_worker().await?;
            self.records.clear();
        } else {
            let _ = self.ensure_worker().await;
        }
        Ok(())
    }
    async fn start_request(&mut self, request: Request) {
        let gui::gui_frame::Msg::StartAudio(start) = &request.message else {
            return;
        };
        if let Some(code) = self.configuration_error {
            reply(&request, Err(code)).await;
            return;
        }
        if !self.config.enabled {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        }
        if start.expected_revision != self.revision {
            reply(&request, Err(ipc::audio::codes::AUDIO_REVISION_CONFLICT)).await;
            return;
        }
        let Ok(id) = <[u8; 32]>::try_from(start.peer_node_id.as_slice()) else {
            reply(&request, Err(ipc::audio::codes::AUDIO_ROLE_CONFLICT)).await;
            return;
        };
        let peer = NodeId::new(id);
        if !self.allowed(peer) {
            reply(&request, Err(ipc::audio::codes::AUDIO_CONSENT_REQUIRED)).await;
            return;
        }
        if self.config.group_id.is_none() {
            reply(&request, Err(ipc::audio::codes::AUDIO_ROLE_CONFLICT)).await;
            return;
        }
        if self.deferred_starts.contains_key(&peer)
            || self.pending_starts.values().any(|p| {
                matches!(&p.request.message, gui::gui_frame::Msg::StartAudio(start)
                if start.peer_node_id.as_slice() == peer.as_bytes())
            })
        {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        }
        self.worker_failed = false;
        if let Err(code) = self.ensure_worker().await {
            reply(&request, Err(code)).await;
            return;
        }
        if !self.start_ready(peer) {
            self.deferred_starts.insert(
                peer,
                Pending {
                    request,
                    deadline: self.now() + REQUEST_TIMEOUT_MS,
                },
            );
            return;
        }
        self.start_now(request, self.now() + REQUEST_TIMEOUT_MS)
            .await;
    }
    fn start_ready(&self, peer: NodeId) -> bool {
        self.config.enabled
            && self.configuration_error.is_none()
            && self
                .worker
                .as_ref()
                .is_some_and(|w| w.alive && w.model_ready)
            && self.admitted.contains(&peer)
            && self.peers.get(&peer).is_some_and(|p| {
                self.current(p.parent)
                    && p.hello.is_some_and(|hello| {
                        peer_setup_ready(self.role(), hello)
                            && hello.group == self.config.group_id.map(|group| group.0)
                    })
                    && match self.role() {
                        model::AudioRole::Listener => {
                            p.support.system_source
                                && available(self.ready.as_ref(), worker::Capability::Render)
                        }
                        model::AudioRole::Source => {
                            p.support.listener
                                && available(self.ready.as_ref(), worker::Capability::SystemCapture)
                        }
                    }
            })
    }
    async fn start_now(&mut self, request: Request, deadline: u64) {
        let gui::gui_frame::Msg::StartAudio(start) = &request.message else {
            return;
        };
        let Ok(id) = <[u8; 32]>::try_from(start.peer_node_id.as_slice()) else {
            return;
        };
        let peer = NodeId::new(id);
        let Some(parent) = self.peers.get(&peer).map(|p| p.parent) else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        };
        let profile = if start.profile == gui::AudioProfile::Wifi as i32 {
            Profile::Wifi
        } else {
            Profile::Ethernet
        };
        let result = if self.role() == model::AudioRole::Listener {
            match (identity::random16(), nonce()) {
                (Ok(session), Some(listener_nonce)) => {
                    let format = format(profile);
                    self.feed(fsm::Input::Start {
                        parent,
                        session,
                        listener_nonce,
                        format,
                        format_sha256: wire::format_sha256(format),
                    })
                }
                _ => Err(ipc::audio::codes::AUDIO_UNAVAILABLE),
            }
        } else {
            let Ok(request_id) = identity::random16() else {
                reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                return;
            };
            self.pending_starts
                .insert(request_id, Pending { request, deadline });
            let result = self.feed(fsm::Input::RequestStart {
                parent,
                request_id,
                kind: keys::StreamKind::System,
                profile,
            });
            self.drain().await;
            if let Err(code) = result {
                if let Some(pending) = self.pending_starts.remove(&request_id) {
                    reply(&pending.request, Err(code)).await;
                }
            }
            return;
        };
        self.drain().await;
        self.publish();
        reply(&request, result).await;
    }
    async fn service_deferred(&mut self) {
        let ready: Vec<_> = self
            .deferred_starts
            .iter()
            .filter(|(peer, p)| self.now() >= p.deadline || self.start_ready(**peer))
            .map(|(p, _)| *p)
            .collect();
        for peer in ready {
            if let Some(pending) = self.deferred_starts.remove(&peer) {
                if self.now() >= pending.deadline {
                    reply(&pending.request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
                } else {
                    self.start_now(pending.request, pending.deadline).await;
                }
            }
        }
    }
    fn gain(&self, peer: NodeId) -> (u32, bool) {
        let (gain, muted) = self
            .config
            .peers
            .iter()
            .find(|p| p.node_id.0 == *peer.as_bytes())
            .map_or((100, false), |p| (p.gain_percent, p.muted));
        let factor = if self.follow_focus() {
            self.focus_policy.gain_percent(*peer.as_bytes())
        } else {
            100
        };
        (effective_gain(gain, factor), muted)
    }
    async fn set_gain(&mut self, set: &gui::SetAudioPeerGain) -> Result<(), Code> {
        if set.expected_revision != self.revision {
            return Err(ipc::audio::codes::AUDIO_REVISION_CONFLICT);
        }
        if self.role() != model::AudioRole::Listener {
            return Err(ipc::audio::codes::AUDIO_ROLE_CONFLICT);
        }
        let id = <[u8; 32]>::try_from(set.peer_node_id.as_slice())
            .map_err(|_| ipc::audio::codes::AUDIO_ROLE_CONFLICT)?;
        let peer = NodeId::new(id);
        if !self.links.borrow().trusted.contains(&peer) {
            return Err(ipc::audio::codes::AUDIO_CONSENT_REQUIRED);
        }
        let mut next = self.config.clone();
        if let Some(pref) = next.peers.iter_mut().find(|p| p.node_id.0 == id) {
            pref.gain_percent = set.gain_percent;
            pref.muted = set.muted;
        } else {
            if next.peers.len() >= 3 {
                return Err(ipc::audio::codes::AUDIO_UNAVAILABLE);
            }
            next.peers.push(config::AudioPeerConfig {
                node_id: config::audio::HexId(id),
                allow_media_control: false,
                gain_percent: set.gain_percent,
                muted: set.muted,
            });
        }
        self.save(next.clone()).await?;
        self.config = next;
        self.apply_gains().await;
        Ok(())
    }
    fn snapshot(&self) -> Option<gui::AudioStatus> {
        let role = self.role();
        let mut status = model::AudioStatus {
            revision: self.revision,
            enabled: self.config.enabled,
            role,
            mode: match self.config.mode {
                config::AudioMode::MixAll => model::AudioMode::MixAll,
                config::AudioMode::FollowFocus => model::AudioMode::FollowFocus,
            },
            ..Default::default()
        };
        status.backend = backend(self.ready.as_ref());
        let mut observations = Vec::new();
        for (peer, record) in &self.records {
            if record.role != role {
                continue;
            }
            let remote = self
                .peers
                .get(peer)
                .and_then(|p| p.hello)
                .is_some_and(|h| h.allowed)
                && self.current(record.view.parent);
            let local = self.allowed(*peer) && self.current(record.view.parent);
            let (gain_percent, muted) = self.gain(*peer);
            let mut consent = model::SessionConsent::default();
            match role {
                model::AudioRole::Source => {
                    consent.local.allow_system_send = local;
                    consent.remote.allow_system_receive = remote;
                }
                model::AudioRole::Listener => {
                    consent.local.allow_system_receive = local;
                    consent.remote.allow_system_send = remote;
                }
            }
            let name = self
                .peers
                .get(peer)
                .map_or_else(|| peer.to_string(), |p| p.name.clone());
            status.sessions.push(model::SessionStatus {
                session_id: record.view.session,
                peer: *peer.as_bytes(),
                source_name: name,
                local_role: role,
                state: phase(record.view.phase),
                profile: match record.agreement.offer.format.profile {
                    Profile::Ethernet => model::AudioProfile::Ethernet,
                    Profile::Wifi => model::AudioProfile::Wifi,
                },
                health: record.health,
                health_state: record.health_state,
                latency_estimate_ms: None,
                gain_percent: u8::try_from(gain_percent).ok()?,
                muted,
                consent,
                media_info: None,
                focus_primary: self.follow_focus()
                    && self.focus_policy.primary() == Some(*peer.as_bytes()),
            });
            observations.push(wire::SessionObservation {
                session: record.view.session,
                peer: *peer.as_bytes(),
                capture_active: record.capture_active,
                sending: record.sending,
            });
        }
        let config = wire::encode_config(&self.config, model::AudioProfile::Ethernet).ok()?;
        let mut result = wire::encode_status(
            &status,
            config,
            gui::AudioMicrophoneState::default(),
            &observations,
        )
        .ok()?;
        result.revision = self.revision;
        Some(result)
    }
    fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(mut status) = self.snapshot() else {
            tracing::warn!("cannot publish validated audio snapshot");
            return;
        };
        if self.last_status.as_ref() == Some(&status) {
            return;
        }
        self.revision = self.revision.saturating_add(1);
        status.revision = self.revision;
        self.last_status = Some(status.clone());
        let _ = self
            .events
            .send((0, gui::gui_frame::Msg::AudioStatus(status)));
    }
}

fn support(bits: &[i32]) -> fsm::Support {
    fsm::Support {
        control: bits.contains(&(net::Capability::AudioControlV1 as i32)),
        system_source: bits.contains(&(net::Capability::SystemAudioSource as i32)),
        listener: bits.contains(&(net::Capability::AudioListener as i32)),
    }
}
fn generation(value: u64, epoch: u64) -> [u8; 16] {
    let mut out = [0; 16];
    out[..8].copy_from_slice(&epoch.to_be_bytes());
    out[8..].copy_from_slice(&value.to_be_bytes());
    out
}
fn available(ready: Option<&worker::Ready>, cap: worker::Capability) -> bool {
    ready.is_some_and(|r| {
        r.probed
            && r.capabilities.iter().any(|c| {
                c.capability == cap as i32
                    && c.availability == worker::Availability::Available as i32
            })
    })
}
fn nonce() -> Option<[u8; 32]> {
    let mut value = [0; 32];
    value[..16].copy_from_slice(&identity::random16().ok()?);
    value[16..].copy_from_slice(&identity::random16().ok()?);
    Some(value)
}
fn format(profile: Profile) -> keys::Format {
    keys::Format {
        codec: keys::Codec::Opus,
        sample_rate: 48000,
        channels: 2,
        frame_samples: if profile == Profile::Wifi { 960 } else { 480 },
        bitrate: if profile == Profile::Wifi {
            64000
        } else {
            96000
        },
        fec: profile == Profile::Wifi,
        aead: keys::Aead::ChaCha20Poly1305,
        profile,
        max_datagram: 1200,
    }
}
fn device(selection: &config::AudioDeviceSelector) -> worker::DeviceSelection {
    worker::DeviceSelection {
        selector: Some(match selection {
            config::AudioDeviceSelector::StableId { id } => {
                worker::device_selection::Selector::DeviceId(id.clone())
            }
            config::AudioDeviceSelector::FollowDefault { role } => {
                worker::device_selection::Selector::FollowDefault(match role {
                    config::DefaultDeviceRole::Multimedia => worker::DefaultRole::Multimedia as i32,
                    config::DefaultDeviceRole::Communications => {
                        worker::DefaultRole::Communications as i32
                    }
                })
            }
        }),
    }
}
fn scope(address: SocketAddr) -> u32 {
    match address {
        SocketAddr::V4(_) => 0,
        SocketAddr::V6(v6) => v6.scope_id(),
    }
}
fn model_code(error: fsm::Error) -> Code {
    match error {
        fsm::Error::Refused(reason) => reason_code(reason),
        fsm::Error::InvalidSettings => ipc::audio::codes::AUDIO_ROLE_CONFLICT,
        _ => ipc::audio::codes::AUDIO_UNAVAILABLE,
    }
}
fn reason_code(reason: fsm::Reason) -> Code {
    use ipc::audio::codes as c;
    match reason {
        fsm::Reason::ConsentRequired | fsm::Reason::Revoked => c::AUDIO_CONSENT_REQUIRED,
        fsm::Reason::RoleConflict => c::AUDIO_ROLE_CONFLICT,
        fsm::Reason::UnsupportedFormat => c::AUDIO_UNSUPPORTED_BACKEND,
        fsm::Reason::DeviceUnavailable => c::AUDIO_DEVICE_UNAVAILABLE,
        fsm::Reason::VirtualMicMissing => c::AUDIO_VIRTUAL_MIC_MISSING,
        fsm::Reason::Timeout => c::AUDIO_NO_UDP_PATH,
        _ => c::AUDIO_UNAVAILABLE,
    }
}
fn phase(phase: fsm::Phase) -> model::SessionState {
    match phase {
        fsm::Phase::Offered => model::SessionState::Preparing,
        fsm::Phase::Answered => model::SessionState::AwaitCommit,
        fsm::Phase::Committed => model::SessionState::PathValidation,
        fsm::Phase::Active => model::SessionState::Running,
        fsm::Phase::Rebuffering => model::SessionState::Rebuffering,
        fsm::Phase::DevicePaused => model::SessionState::DevicePaused,
        fsm::Phase::Stopping => model::SessionState::Stopping,
        fsm::Phase::Stopped => model::SessionState::Idle,
        fsm::Phase::Failed => model::SessionState::Failed,
    }
}
async fn answer(request: &Request, message: gui::gui_frame::Msg) {
    let _ = timeout(
        Duration::from_millis(100),
        request.out.send((request.request_id, message)),
    )
    .await;
}
async fn reply(request: &Request, result: Result<(), Code>) {
    answer(
        request,
        gui::gui_frame::Msg::OpResult(gui::OpResult {
            ok: result.is_ok(),
            error_code: result.err().unwrap_or("").to_owned(),
        }),
    )
    .await;
}
fn backend(ready: Option<&worker::Ready>) -> Vec<model::CapabilityAvailability> {
    [
        (
            model::BackendCapability::SystemCapture,
            worker::Capability::SystemCapture,
        ),
        (model::BackendCapability::Render, worker::Capability::Render),
        (
            model::BackendCapability::EndpointMute,
            worker::Capability::EndpointMute,
        ),
        (
            model::BackendCapability::GlobalPtt,
            worker::Capability::GlobalPtt,
        ),
        (
            model::BackendCapability::MediaSessions,
            worker::Capability::MediaSessions,
        ),
    ]
    .into_iter()
    .map(|(capability, native)| {
        let availability = if let Some(c) = ready.filter(|r| r.probed).and_then(|r| {
            r.capabilities
                .iter()
                .find(|c| c.capability == native as i32)
        }) {
            let reason = match worker::UnavailableReason::try_from(c.reason) {
                Ok(worker::UnavailableReason::ServerUnavailable) => {
                    model::UnavailableReason::ServerUnavailable
                }
                Ok(worker::UnavailableReason::PermissionDenied) => {
                    model::UnavailableReason::PermissionDenied
                }
                Ok(worker::UnavailableReason::DeviceGone) => model::UnavailableReason::DeviceGone,
                Ok(worker::UnavailableReason::ProcessExclusionUnavailable) => {
                    model::UnavailableReason::ProcessExclusionUnavailable
                }
                _ => model::UnavailableReason::NotSupportedByPlatform,
            };
            match worker::Availability::try_from(c.availability) {
                Ok(worker::Availability::Available) => model::Availability::Available,
                Ok(worker::Availability::Unsupported) => model::Availability::Unsupported(reason),
                _ => model::Availability::Unavailable(reason),
            }
        } else {
            model::Availability::Unavailable(model::UnavailableReason::WorkerUnavailable)
        };
        model::CapabilityAvailability {
            capability,
            availability,
        }
    })
    .collect()
}

fn elapsed_second(previous: Option<u64>, now: u64) -> bool {
    previous.is_none_or(|at| now.saturating_sub(at) >= 1000)
}
fn effective_gain(user: u32, focus: u32) -> u32 {
    user.min(100) * focus.min(100) / 100
}
fn background_percent(config: &config::AudioConfig) -> u32 {
    if config.background_mute {
        0
    } else {
        (100.0 * 10.0_f32.powf(config.background_gain_db / 20.0))
            .round()
            .clamp(0.0, 100.0) as u32
    }
}

fn permitted(config: &config::AudioConfig, peer: NodeId) -> bool {
    let id = config::audio::HexId(*peer.as_bytes());
    config.enabled
        && match config.role {
            config::AudioRole::Source => {
                config.listener_node_id == Some(id) && config.allow_send_to.contains(&id)
            }
            config::AudioRole::Listener => config.allow_system_audio_from.contains(&id),
            config::AudioRole::Off => false,
        }
}

fn supervisor_code(error: AudioSupervisorError) -> Code {
    match error {
        AudioSupervisorError::WorkerRejected(code)
            if code == worker::ErrorCode::Unsupported as i32 =>
        {
            ipc::audio::codes::AUDIO_UNSUPPORTED_BACKEND
        }
        AudioSupervisorError::WorkerRejected(_) => ipc::audio::codes::AUDIO_DEVICE_UNAVAILABLE,
        _ => ipc::audio::codes::AUDIO_UNAVAILABLE,
    }
}

fn enqueue_retry(
    queue: &mut VecDeque<(fsm::Parent, fsm::Control)>,
    parent: fsm::Parent,
    message: fsm::Control,
) {
    if queue.len() < 12 && !queue.iter().any(|(p, m)| *p == parent && *m == message) {
        queue.push_back((parent, message));
    }
}
fn supersede_snapshot(
    queue: &mut VecDeque<(fsm::Parent, fsm::Control)>,
    parent: fsm::Parent,
    message: &fsm::Control,
) {
    if matches!(message, fsm::Control::Hello(_)) {
        // A resolved snapshot must never be followed by obsolete discovery gossip.
        queue.retain(|(p, m)| {
            *p != parent || !matches!(m, fsm::Control::Hello(_) | fsm::Control::Consent { .. })
        });
    }
}
fn control_session(message: &fsm::Control) -> Option<[u8; 16]> {
    match message {
        fsm::Control::Offer(offer) => Some(offer.session),
        fsm::Control::Answer(answer) => Some(answer.session),
        fsm::Control::Commit { session, .. }
        | fsm::Control::Stop { session, .. }
        | fsm::Control::Keepalive { session, .. } => Some(*session),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_multiplier_preserves_user_gain_and_bounds() {
        assert_eq!(effective_gain(80, 100), 80);
        assert_eq!(effective_gain(80, 13), 10);
        assert_eq!(effective_gain(80, 0), 0);
        assert_eq!(effective_gain(100, 100), 100);
        let mut config = config::AudioConfig::default();
        assert_eq!(background_percent(&config), 13);
        config.background_mute = true;
        assert_eq!(background_percent(&config), 0);
    }

    #[test]
    fn focus_requests_are_bounded_per_parent() {
        assert!(elapsed_second(None, 0));
        assert!(!elapsed_second(Some(100), 1099));
        assert!(elapsed_second(Some(100), 1100));
        assert!(!elapsed_second(Some(100), 99));
    }

    fn parent(value: u8) -> fsm::Parent {
        fsm::Parent {
            peer: [value; 32],
            connection_id: [value; 16],
            generation: [value; 16],
        }
    }

    fn hello(group: Option<[u8; 16]>) -> fsm::Control {
        fsm::Control::Hello(fsm::Hello {
            role: model::AudioRole::Source,
            group,
            listener: [2; 32],
            available: true,
            allowed: true,
            consent_revision: 1,
        })
    }

    #[test]
    fn resolved_snapshot_replaces_groupless_retry_before_following_consent() {
        let parent = parent(1);
        let old = hello(None);
        let resolved = hello(Some([3; 16]));
        let consent = fsm::Control::Consent {
            group: [3; 16],
            revision: 2,
            allowed: true,
        };
        let mut queue = VecDeque::new();
        enqueue_retry(&mut queue, parent, old);
        enqueue_retry(&mut queue, parent, consent);
        supersede_snapshot(&mut queue, parent, &resolved);
        assert!(queue.is_empty());
        // The resolved write fails too; its following Consent must wait behind it.
        enqueue_retry(&mut queue, parent, resolved);
        enqueue_retry(&mut queue, parent, consent);
        let retry = std::mem::take(&mut queue);
        for (parent, message) in retry {
            supersede_snapshot(&mut queue, parent, &message);
            enqueue_retry(&mut queue, parent, message);
        }
        assert_eq!(
            queue,
            VecDeque::from([(parent, resolved), (parent, consent)])
        );
    }

    #[test]
    fn snapshot_coalescing_preserves_other_parent_fifo() {
        let first = parent(1);
        let second = parent(2);
        let old = hello(None);
        let resolved = hello(Some([3; 16]));
        let consent = fsm::Control::Consent {
            group: [3; 16],
            revision: 2,
            allowed: false,
        };
        let mut queue = VecDeque::new();
        enqueue_retry(&mut queue, first, old);
        enqueue_retry(&mut queue, second, old);
        enqueue_retry(&mut queue, second, consent);
        supersede_snapshot(&mut queue, first, &resolved);
        enqueue_retry(&mut queue, first, resolved);
        assert_eq!(
            queue,
            VecDeque::from([(second, old), (second, consent), (first, resolved),])
        );
    }

    #[test]
    fn fresh_supervisor_cannot_reuse_model_worker_generation() {
        assert_ne!(generation(1, 1), generation(1, 2));
        assert_ne!(generation(1, 1), generation(2, 1));
        assert_eq!(&generation(7, 3)[8..], &7u64.to_be_bytes());
    }
}

fn peer_setup_ready(local_role: model::AudioRole, hello: fsm::Hello) -> bool {
    match local_role {
        model::AudioRole::Listener => hello.role == model::AudioRole::Source,
        model::AudioRole::Source => hello.role == model::AudioRole::Listener && hello.available,
    }
}

#[test]
fn source_hello_does_not_require_the_listener_only_available_flag() {
    let mut hello = fsm::Hello {
        role: model::AudioRole::Source,
        group: Some([1; 16]),
        listener: [2; 32],
        available: false,
        allowed: true,
        consent_revision: 1,
    };
    assert!(peer_setup_ready(model::AudioRole::Listener, hello));
    assert!(!peer_setup_ready(model::AudioRole::Source, hello));
    hello.role = model::AudioRole::Listener;
    assert!(!peer_setup_ready(model::AudioRole::Source, hello));
    hello.available = true;
    assert!(peer_setup_ready(model::AudioRole::Source, hello));
}
