//! Session-scoped, memory-only now-playing relay and correlated native commands.
use super::*;
use audio_model::media::{Refusal, Relay, Scope};

const PENDING_LIMIT: usize = 16;
const USED_LIMIT: usize = 256;
const COMMAND_TIMEOUT_MS: u64 = 3_000;

struct GuiPending {
    request: Request,
    view: fsm::SessionView,
    deadline: u64,
}
struct NativePending {
    view: fsm::SessionView,
    request: [u8; 16],
    deadline: u64,
}
// Quarantined facts: never installed in a Relay until local path validation.
struct DeferredInfo {
    view: fsm::SessionView,
    deadline: u64,
    message: net::MediaInfo,
}
#[derive(Default)]
struct RequestHistory {
    scope: Option<(fsm::SessionView, u64, u64)>,
    ids: BTreeSet<[u8; 16]>,
}
impl RequestHistory {
    // Call only after authorization against the current native facts.
    fn admit(
        &mut self,
        view: fsm::SessionView,
        token: u64,
        revision: u64,
        id: [u8; 16],
    ) -> Result<bool, ()> {
        let scope = (view, token, revision);
        if self.scope != Some(scope) {
            self.ids.clear();
            self.scope = Some(scope);
        }
        if self.ids.contains(&id) {
            return Ok(false);
        }
        if self.ids.len() >= USED_LIMIT {
            return Err(());
        }
        self.ids.insert(id);
        Ok(true)
    }
}
#[derive(Default)]
pub(super) struct State {
    relays: BTreeMap<NodeId, Relay>,
    watch_generation: Option<[u8; 16]>,
    watch_dirty: bool,
    native: Option<(u64, Option<model::MediaInfo>)>,
    sent: BTreeMap<NodeId, (fsm::SessionView, u64, bool)>,
    permissions: BTreeMap<NodeId, (fsm::SessionView, bool)>,
    deferred: BTreeMap<NodeId, DeferredInfo>,
    received: u64,
    gui: BTreeMap<[u8; 16], GuiPending>,
    native_pending: BTreeMap<u64, NativePending>,
    used: BTreeMap<NodeId, RequestHistory>,
}

impl Actor {
    pub(super) fn media_capable(&self) -> bool {
        self.config.enabled
            && match self.config.role {
                config::AudioRole::Listener => true,
                config::AudioRole::Source => {
                    available(self.ready.as_ref(), worker::Capability::MediaSessions)
                }
                config::AudioRole::Off => false,
            }
    }
    fn media_scope(&self, peer: NodeId) -> Option<Scope> {
        self.media_candidate_scope(peer)
            .filter(|scope| scope.live())
    }
    fn media_candidate_scope(&self, peer: NodeId) -> Option<Scope> {
        let record = self.records.get(&peer)?;
        let worker = self.worker.as_ref().filter(|w| w.alive && w.model_ready)?;
        let remote = self.peers.get(&peer)?;
        let capable = self.media_capable()
            && remote.media_support
            && self
                .links
                .borrow()
                .peers
                .get(&peer)
                .is_some_and(|p| p.media_support);
        let scope = Scope {
            view: record.view,
            current_parent: self.current(record.view.parent) && remote.parent == record.view.parent,
            consented: self.allowed(peer)
                && remote.hello.is_some_and(|h| {
                    h.allowed
                        && h.group == self.config.group_id.map(|g| g.0)
                        && h.role != record.role
                }),
            capable,
            worker_generation: generation(worker.generation, self.worker_epoch),
        };
        candidate_scope(scope).then_some(scope)
    }
    pub(super) fn media_info(&self, peer: NodeId) -> Option<model::MediaInfo> {
        let scope = self.media_scope(peer)?;
        self.media
            .relays
            .get(&peer)
            .filter(|r| r.view() == Some(scope.view))?
            .info()
            .cloned()
    }
    pub(super) fn media_allowed(&self, peer: NodeId) -> bool {
        self.media_scope(peer)
            .is_some_and(|scope| self.media.permissions.get(&peer) == Some(&(scope.view, true)))
    }
    async fn media_send(&self, parent: fsm::Parent, body: net::audio_control::Body) -> bool {
        if !self
            .media_scope(NodeId::new(parent.peer))
            .is_some_and(|s| s.view.parent == parent)
        {
            return false;
        }
        self.manager
            .send_audio_control(
                &NodeId::new(parent.peer),
                net::AudioControl {
                    control_version: 1,
                    body: Some(body),
                },
            )
            .await
            .is_ok()
    }
    async fn media_ack(
        &self,
        view: fsm::SessionView,
        request: [u8; 16],
        result: net::AudioControlResult,
    ) {
        self.media_send(
            view.parent,
            net::audio_control::Body::MediaAck(net::MediaAck {
                request_id: request.to_vec(),
                session_id: view.session.to_vec(),
                result: result as i32,
            }),
        )
        .await;
    }
    pub(super) async fn media_tick(&mut self) {
        let now = self.now();
        while let Some((peer, scope)) = self.media.deferred.iter().find_map(|(peer, pending)| {
            let scope = self
                .media_candidate_scope(*peer)
                .filter(|s| pending.matches(*s, now));
            match scope {
                Some(scope) if !scope.live() => None,
                _ => Some((*peer, scope)),
            }
        }) {
            if let Some(pending) = self.media.deferred.remove(&peer) {
                if let Some(scope) = scope {
                    self.media_receive_info(peer, scope, &pending.message);
                }
            }
        }
        let scopes: [Option<(NodeId, Scope)>; fsm::MAX_PEERS] = {
            let mut live = self
                .records
                .keys()
                .filter_map(|p| self.media_scope(*p).map(|s| (*p, s)));
            std::array::from_fn(|_| live.next())
        };
        let find_scope = |peer: &NodeId| {
            scopes
                .iter()
                .flatten()
                .find(|(p, _)| p == peer)
                .map(|(_, s)| s)
        };
        let previous_count = self.media.relays.len();
        self.media
            .relays
            .retain(|peer, _| find_scope(peer).is_some());
        self.dirty |= previous_count != self.media.relays.len();
        self.media.sent.retain(|peer, _| find_scope(peer).is_some());
        self.media
            .permissions
            .retain(|peer, (view, _)| find_scope(peer).is_some_and(|s| s.view == *view));
        for (peer, scope) in scopes.iter().flatten() {
            let relay = self.media.relays.entry(*peer).or_default();
            self.dirty |= relay.view() != Some(scope.view);
            relay.scope(Some(*scope));
        }
        self.media.used.retain(|peer, history| {
            self.links.borrow().peers.get(peer).is_some_and(|p| {
                p.trusted
                    && history
                        .scope
                        .is_some_and(|(view, _, _)| p.parent == view.parent)
            })
        });
        let desired = if self.config.role == config::AudioRole::Source
            && scopes.iter().any(Option::is_some)
        {
            self.worker
                .as_ref()
                .map(|w| generation(w.generation, self.worker_epoch))
        } else {
            None
        };
        if self.media.watch_dirty || self.media.watch_generation != desired {
            self.media.native = None;
            self.media.sent.clear();
            if let Some(worker) = &self.worker {
                if worker.alive {
                    match worker.handle.set_media_watch(desired.is_some()).await {
                        Ok(_) => {
                            self.media.watch_generation = desired;
                            self.media.watch_dirty = false;
                        }
                        Err(error) => {
                            if self.native_privacy_failure(&error) {
                                return;
                            }
                        }
                    }
                }
            }
            self.dirty = true;
        }
        let now = self.now();
        while let Some(id) = self.media.gui.iter().find_map(|(id, p)| {
            (now >= p.deadline
                || !find_scope(&NodeId::new(p.view.parent.peer)).is_some_and(|s| s.view == p.view))
            .then_some(*id)
        }) {
            if let Some(pending) = self.media.gui.remove(&id) {
                reply(&pending.request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            }
        }
        while let Some(id) = self.media.native_pending.iter().find_map(|(id, p)| {
            (now >= p.deadline
                || !find_scope(&NodeId::new(p.view.parent.peer)).is_some_and(|s| s.view == p.view))
            .then_some(*id)
        }) {
            if let Some(pending) = self.media.native_pending.remove(&id) {
                let result = if now >= pending.deadline {
                    net::AudioControlResult::Timeout
                } else {
                    net::AudioControlResult::Cancelled
                };
                self.media_ack(pending.view, pending.request, result).await;
            }
        }
        if let Some((revision, info)) = self.media.native.as_ref() {
            for (peer, scope) in scopes.into_iter().flatten() {
                let allowed = self
                    .config
                    .peers
                    .iter()
                    .any(|p| p.node_id.0 == *peer.as_bytes() && p.allow_media_control);
                if self.config.role != config::AudioRole::Source
                    || self.media.sent.get(&peer) == Some(&(scope.view, *revision, allowed))
                {
                    continue;
                }
                if let Some(relay) = self.media.relays.get_mut(&peer) {
                    self.dirty |= relay.update(
                        scope.view.parent,
                        scope.view.session,
                        *revision,
                        info.clone(),
                    );
                }
                let message = encode_info(scope.view.session, *revision, info.as_ref(), allowed);
                if self
                    .media_send(
                        scope.view.parent,
                        net::audio_control::Body::MediaInfo(message),
                    )
                    .await
                {
                    self.media
                        .sent
                        .insert(peer, (scope.view, *revision, allowed));
                }
            }
        }
    }
    pub(super) fn media_reset(&mut self) {
        for (_, pending) in std::mem::take(&mut self.media.gui) {
            let _ = pending.request.out.try_send((
                pending.request.request_id,
                gui::gui_frame::Msg::OpResult(gui::OpResult {
                    ok: false,
                    error_code: ipc::audio::codes::AUDIO_UNAVAILABLE.to_owned(),
                }),
            ));
        }
        let watch_dirty = self.media.watch_generation.is_some() || self.media.watch_dirty;
        self.media = State {
            watch_dirty,
            ..State::default()
        };
        self.dirty = true;
    }
    pub(super) async fn media_on_gui(&mut self, request: Request) {
        let gui::gui_frame::Msg::SendMediaCommand(command) = &request.message else {
            return;
        };
        let Ok(raw_peer) = <[u8; 32]>::try_from(command.peer_node_id.as_slice()) else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        };
        let peer = NodeId::new(raw_peer);
        let Some(scope) = self
            .media_scope(peer)
            .filter(|_| self.config.role == config::AudioRole::Listener)
        else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        };
        if command.session_id.as_slice() != scope.view.session {
            reply(&request, Err(ipc::audio::codes::AUDIO_STALE_MEDIA)).await;
            return;
        }
        let Some(kind) = gui_command(command.command) else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNSUPPORTED_COMMAND)).await;
            return;
        };
        let result = self
            .media
            .relays
            .get(&peer)
            .filter(|r| r.view() == Some(scope.view))
            .ok_or(Refusal::Inactive)
            .and_then(|r| {
                r.authorize(
                    self.media_allowed(peer),
                    command.session_token,
                    command.media_revision,
                    kind,
                )
            });
        if let Err(error) = result {
            reply(&request, result_code(refusal(error))).await;
            return;
        }
        if self.media.gui.len() >= PENDING_LIMIT {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        }
        let Ok(id) = identity::random16() else {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        };
        let message = net::MediaCommand {
            request_id: id.to_vec(),
            session_id: scope.view.session.to_vec(),
            token: command.session_token,
            revision: command.media_revision,
            command: network_command(kind) as i32,
        };
        if !self
            .media_send(
                scope.view.parent,
                net::audio_control::Body::MediaCommand(message),
            )
            .await
        {
            reply(&request, Err(ipc::audio::codes::AUDIO_UNAVAILABLE)).await;
            return;
        }
        self.media.gui.insert(
            id,
            GuiPending {
                request,
                view: scope.view,
                deadline: self.now().saturating_add(COMMAND_TIMEOUT_MS),
            },
        );
    }
    fn media_receive_info(&mut self, peer: NodeId, scope: Scope, message: &net::MediaInfo) {
        if !scope.live() || message.session_id.as_slice() != scope.view.session {
            return;
        }
        let Ok(info) = decode_info(message) else {
            return;
        };
        let relay = self.media.relays.entry(peer).or_default();
        relay.scope(Some(scope));
        if info.as_ref().zip(relay.info()).is_some_and(|(new, old)| {
            new.session_token == old.session_token && new.revision < old.revision
        }) {
            return;
        }
        let allowed = message.present && message.allow_control;
        self.dirty |= self.media.permissions.get(&peer) != Some(&(scope.view, allowed));
        self.media.permissions.insert(peer, (scope.view, allowed));
        self.media.received = self.media.received.saturating_add(1);
        self.dirty |= relay.update(
            scope.view.parent,
            scope.view.session,
            self.media.received,
            info,
        );
    }
    pub(super) async fn media_on_network(
        &mut self,
        parent: fsm::Parent,
        message: &net::AudioControl,
    ) {
        let peer = NodeId::new(parent.peer);
        // Source and Listener path-validation events have no cross-process ordering.
        // Retain one bounded snapshot, but do not expose it or authorize commands.
        if self.config.role == config::AudioRole::Listener {
            if let Some(net::audio_control::Body::MediaInfo(info)) = &message.body {
                let Some(scope) = self
                    .media_candidate_scope(peer)
                    .filter(|s| s.view.parent == parent)
                else {
                    return;
                };
                if info.session_id.as_slice() != scope.view.session || decode_info(info).is_err() {
                    return;
                }
                if !scope.live() {
                    let deadline = self
                        .media
                        .deferred
                        .get(&peer)
                        .filter(|p| p.matches(scope, self.now()))
                        .map_or_else(
                            || self.now().saturating_add(fsm::SETUP_TIMEOUT_MS),
                            |p| p.deadline,
                        );
                    self.media.deferred.insert(
                        peer,
                        DeferredInfo {
                            view: scope.view,
                            deadline,
                            message: info.clone(),
                        },
                    );
                    return;
                }
            }
        }
        let Some(scope) = self.media_scope(peer).filter(|s| s.view.parent == parent) else {
            return;
        };
        match &message.body {
            Some(net::audio_control::Body::MediaInfo(message))
                if self.config.role == config::AudioRole::Listener =>
            {
                self.media.deferred.remove(&peer);
                self.media_receive_info(peer, scope, message);
            }
            Some(net::audio_control::Body::MediaAck(ack))
                if self.config.role == config::AudioRole::Listener =>
            {
                let Ok(id) = <[u8; 16]>::try_from(ack.request_id.as_slice()) else {
                    return;
                };
                if !self.media.gui.get(&id).is_some_and(|p| {
                    p.view == scope.view && ack.session_id.as_slice() == p.view.session
                }) {
                    return;
                }
                let Some(pending) = self.media.gui.remove(&id) else {
                    return;
                };
                let result = if self.now() >= pending.deadline {
                    net::AudioControlResult::Timeout
                } else {
                    net::AudioControlResult::try_from(ack.result)
                        .unwrap_or(net::AudioControlResult::Invalid)
                };
                reply(&pending.request, result_code(result)).await;
            }
            Some(net::audio_control::Body::MediaCommand(command))
                if self.config.role == config::AudioRole::Source =>
            {
                let Ok(id) = <[u8; 16]>::try_from(command.request_id.as_slice()) else {
                    return;
                };
                if id == [0; 16] || command.session_id.as_slice() != scope.view.session {
                    return;
                }
                let allowed = self
                    .config
                    .peers
                    .iter()
                    .any(|p| p.node_id.0 == parent.peer && p.allow_media_control);
                if !allowed {
                    self.media_ack(scope.view, id, net::AudioControlResult::ConsentRequired)
                        .await;
                    return;
                }
                let Some(kind) = decode_command(command.command) else {
                    self.media_ack(scope.view, id, net::AudioControlResult::Invalid)
                        .await;
                    return;
                };
                let result = authorize_source(
                    scope,
                    self.media.relays.get(&peer),
                    allowed,
                    parent,
                    command,
                    kind,
                );
                if let Err(error) = result {
                    self.media_ack(scope.view, id, refusal(error)).await;
                    return;
                }
                match self.media.used.entry(peer).or_default().admit(
                    scope.view,
                    command.token,
                    command.revision,
                    id,
                ) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(()) => {
                        self.media_ack(scope.view, id, net::AudioControlResult::Busy)
                            .await;
                        return;
                    }
                }
                if self.media.native_pending.len() >= PENDING_LIMIT {
                    self.media_ack(scope.view, id, net::AudioControlResult::Busy)
                        .await;
                    return;
                }
                let Some(worker) = self.worker.as_ref() else {
                    return;
                };
                match worker
                    .handle
                    .media_command(command.token, command.revision, native_command(kind))
                    .await
                {
                    Ok(request_id) => {
                        self.media.native_pending.insert(
                            request_id,
                            NativePending {
                                view: scope.view,
                                request: id,
                                deadline: self.now().saturating_add(COMMAND_TIMEOUT_MS),
                            },
                        );
                    }
                    Err(_) => {
                        self.media_ack(scope.view, id, net::AudioControlResult::DeviceGone)
                            .await
                    }
                }
            }
            _ => {}
        }
    }
    pub(super) async fn media_on_worker(&mut self, event: &AudioSupervisorEvent) {
        match event {
            AudioSupervisorEvent::MediaSessions {
                generation: raw,
                sessions,
            } => {
                let worker_generation = generation(*raw, self.worker_epoch);
                if self.media.watch_generation != Some(worker_generation)
                    || !self
                        .worker
                        .as_ref()
                        .is_some_and(|w| w.alive && w.model_ready && w.generation == *raw)
                    || !self.records.keys().any(|p| self.media_scope(*p).is_some())
                {
                    return;
                }
                if sessions.revision == 0
                    || self
                        .media
                        .native
                        .as_ref()
                        .is_some_and(|(old, _)| sessions.revision <= *old)
                {
                    return;
                }
                let Ok(info) = current_native(sessions) else {
                    return;
                };
                self.media.native = Some((sessions.revision, info));
                self.media_tick().await;
            }
            AudioSupervisorEvent::MediaCommandResult {
                generation: raw,
                request_id,
                result,
            } => {
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == *raw)
                {
                    return;
                }
                let Some(pending) = self.media.native_pending.remove(request_id) else {
                    return;
                };
                if pending.view.worker_generation != generation(*raw, self.worker_epoch)
                    || !self
                        .media_scope(NodeId::new(pending.view.parent.peer))
                        .is_some_and(|s| s.view == pending.view)
                {
                    return;
                }
                let status = if self.now() >= pending.deadline {
                    net::AudioControlResult::Timeout
                } else {
                    native_result(result.result)
                };
                self.media_ack(pending.view, pending.request, status).await;
            }
            AudioSupervisorEvent::CommandRejected {
                generation: raw,
                request_id,
                error,
            } => {
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == *raw)
                {
                    return;
                }
                let Some(pending) = self.media.native_pending.remove(request_id) else {
                    return;
                };
                if pending.view.worker_generation != generation(*raw, self.worker_epoch) {
                    return;
                }
                let result = match worker::ErrorCode::try_from(error.code) {
                    Ok(worker::ErrorCode::Unsupported) => net::AudioControlResult::Unsupported,
                    Ok(worker::ErrorCode::ResourceLimit) => net::AudioControlResult::Busy,
                    Ok(worker::ErrorCode::InvalidState) => net::AudioControlResult::Cancelled,
                    _ => net::AudioControlResult::DeviceGone,
                };
                self.media_ack(pending.view, pending.request, result).await;
            }
            _ => {}
        }
    }
}

fn candidate_scope(scope: Scope) -> bool {
    scope.current_parent
        && scope.consented
        && scope.capable
        && scope.view.worker_generation == scope.worker_generation
        && matches!(
            scope.view.phase,
            fsm::Phase::Committed | fsm::Phase::Active | fsm::Phase::Rebuffering
        )
}

impl DeferredInfo {
    fn matches(&self, scope: Scope, now: u64) -> bool {
        now < self.deadline
            && candidate_scope(scope)
            && self.view.parent == scope.view.parent
            && self.view.session == scope.view.session
            && self.view.worker_generation == scope.view.worker_generation
    }
}

fn authorize_source(
    scope: Scope,
    relay: Option<&Relay>,
    allowed: bool,
    parent: fsm::Parent,
    command: &net::MediaCommand,
    kind: model::MediaCommand,
) -> Result<(), Refusal> {
    if !scope.live()
        || scope.view.parent != parent
        || command.session_id.as_slice() != scope.view.session
    {
        return Err(Refusal::Inactive);
    }
    relay
        .filter(|r| r.view() == Some(scope.view))
        .ok_or(Refusal::Inactive)?
        .authorize(allowed, command.token, command.revision, kind)
}

fn refusal(value: Refusal) -> net::AudioControlResult {
    match value {
        Refusal::Inactive => net::AudioControlResult::Cancelled,
        Refusal::ConsentRequired => net::AudioControlResult::ConsentRequired,
        Refusal::StaleRevision => net::AudioControlResult::StaleRevision,
        Refusal::Unsupported => net::AudioControlResult::Unsupported,
    }
}
fn result_code(value: net::AudioControlResult) -> Result<(), Code> {
    use ipc::audio::codes as c;
    use net::AudioControlResult as R;
    match value {
        R::Ok => Ok(()),
        R::ConsentRequired => Err(c::AUDIO_CONSENT_REQUIRED),
        R::StaleRevision => Err(c::AUDIO_STALE_MEDIA),
        R::Unsupported => Err(c::AUDIO_UNSUPPORTED_COMMAND),
        R::DeviceGone => Err(c::AUDIO_DEVICE_UNAVAILABLE),
        R::Conflict => Err(c::AUDIO_REVISION_CONFLICT),
        _ => Err(c::AUDIO_UNAVAILABLE),
    }
}
fn gui_command(raw: i32) -> Option<model::MediaCommand> {
    use gui::AudioMediaCommand as G;
    Some(match G::try_from(raw).ok()? {
        G::Play => model::MediaCommand::Play,
        G::Pause => model::MediaCommand::Pause,
        G::Toggle => model::MediaCommand::Toggle,
        G::Next => model::MediaCommand::Next,
        G::Previous => model::MediaCommand::Previous,
        _ => return None,
    })
}
fn decode_command(raw: i32) -> Option<model::MediaCommand> {
    use net::MediaCommandKind as N;
    Some(match N::try_from(raw).ok()? {
        N::Play => model::MediaCommand::Play,
        N::Pause => model::MediaCommand::Pause,
        N::PlayPause => model::MediaCommand::Toggle,
        N::Next => model::MediaCommand::Next,
        N::Previous => model::MediaCommand::Previous,
        _ => return None,
    })
}
fn network_command(command: model::MediaCommand) -> net::MediaCommandKind {
    use model::MediaCommand as M;
    match command {
        M::Play => net::MediaCommandKind::Play,
        M::Pause => net::MediaCommandKind::Pause,
        M::Toggle => net::MediaCommandKind::PlayPause,
        M::Next => net::MediaCommandKind::Next,
        M::Previous => net::MediaCommandKind::Previous,
    }
}
fn native_command(command: model::MediaCommand) -> worker::MediaCommandKind {
    use model::MediaCommand as M;
    match command {
        M::Play => worker::MediaCommandKind::Play,
        M::Pause => worker::MediaCommandKind::Pause,
        M::Toggle => worker::MediaCommandKind::Toggle,
        M::Next => worker::MediaCommandKind::Next,
        M::Previous => worker::MediaCommandKind::Previous,
    }
}
fn native_result(raw: i32) -> net::AudioControlResult {
    use worker::MediaCommandStatus as W;
    match W::try_from(raw) {
        Ok(W::Ok) => net::AudioControlResult::Ok,
        Ok(W::StaleRevision) => net::AudioControlResult::StaleRevision,
        Ok(W::Unsupported) => net::AudioControlResult::Unsupported,
        Ok(W::Timeout) => net::AudioControlResult::Timeout,
        Ok(W::Cancelled) => net::AudioControlResult::Cancelled,
        _ => net::AudioControlResult::Invalid,
    }
}
fn command_bits(commands: model::MediaCommands) -> u32 {
    u32::from(commands.play)
        | (u32::from(commands.pause) << 1)
        | (u32::from(commands.toggle) << 2)
        | (u32::from(commands.next) << 3)
        | (u32::from(commands.previous) << 4)
}
fn commands(bits: u32) -> model::MediaCommands {
    model::MediaCommands {
        play: bits & 1 != 0,
        pause: bits & 2 != 0,
        toggle: bits & 4 != 0,
        next: bits & 8 != 0,
        previous: bits & 16 != 0,
    }
}
fn current_native(sessions: &worker::MediaSessions) -> Result<Option<model::MediaInfo>, ()> {
    let Some(token) = sessions.current_token else {
        return Ok(None);
    };
    let native = sessions
        .sessions
        .iter()
        .find(|s| s.token == token)
        .ok_or(())?;
    if native.app_id.len() > 128 || native.title.len() > 256 || native.subtitle.len() > 256 {
        return Err(());
    }
    if [&native.app_id, &native.title, &native.subtitle]
        .iter()
        .any(|s| s.chars().any(char::is_control))
    {
        return Err(());
    }
    let info = model::MediaInfo {
        app_id: native.app_id.clone(),
        title: native.title.clone(),
        subtitle: native.subtitle.clone(),
        session_token: native.token,
        revision: native.revision,
        position_ms: native.position_ms,
        duration_ms: native.duration_ms,
        playback: match worker::PlaybackState::try_from(native.state) {
            Ok(worker::PlaybackState::Playing) => model::PlaybackState::Playing,
            Ok(worker::PlaybackState::Paused) => model::PlaybackState::Paused,
            Ok(worker::PlaybackState::Stopped) => model::PlaybackState::Stopped,
            Ok(worker::PlaybackState::Unknown) => model::PlaybackState::Unknown,
            _ => return Err(()),
        },
        commands: commands(native.commands),
    };
    if info.session_token == 0
        || info.revision == 0
        || native.commands & !31 != 0
        || info.validate().is_err()
    {
        return Err(());
    }
    Ok(Some(info))
}
fn encode_info(
    session: [u8; 16],
    revision: u64,
    info: Option<&model::MediaInfo>,
    allow_control: bool,
) -> net::MediaInfo {
    let Some(info) = info else {
        return net::MediaInfo {
            session_id: session.to_vec(),
            revision,
            playback: net::MediaPlayback::Stopped as i32,
            ..Default::default()
        };
    };
    net::MediaInfo {
        session_id: session.to_vec(),
        present: true,
        token: info.session_token,
        revision: info.revision,
        app: info.app_id.clone(),
        title: info.title.clone(),
        subtitle: info.subtitle.clone(),
        position_ms: info.position_ms,
        duration_ms: info.duration_ms,
        playback: match info.playback {
            model::PlaybackState::Playing => net::MediaPlayback::Playing,
            model::PlaybackState::Paused => net::MediaPlayback::Paused,
            model::PlaybackState::Stopped => net::MediaPlayback::Stopped,
            model::PlaybackState::Unknown => net::MediaPlayback::Unknown,
        } as i32,
        commands: command_bits(info.commands),
        allow_control,
    }
}
fn decode_info(info: &net::MediaInfo) -> Result<Option<model::MediaInfo>, ()> {
    if !info.present {
        return Ok(None);
    }
    if info.token == 0 || info.revision == 0 || info.commands & !31 != 0 {
        return Err(());
    }
    let value = model::MediaInfo {
        app_id: info.app.clone(),
        title: info.title.clone(),
        subtitle: info.subtitle.clone(),
        session_token: info.token,
        revision: info.revision,
        position_ms: info.position_ms,
        duration_ms: info.duration_ms,
        playback: match net::MediaPlayback::try_from(info.playback) {
            Ok(net::MediaPlayback::Playing) => model::PlaybackState::Playing,
            Ok(net::MediaPlayback::Paused) => model::PlaybackState::Paused,
            Ok(net::MediaPlayback::Stopped) => model::PlaybackState::Stopped,
            Ok(net::MediaPlayback::Unknown) => model::PlaybackState::Unknown,
            _ => return Err(()),
        },
        commands: commands(info.commands),
    };
    value.validate().map_err(|_| ())?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_history_is_bounded_per_authoritative_native_scope() {
        let mut history = RequestHistory::default();
        let view = live_scope().view;
        for id in 0..USED_LIMIT {
            let mut request = [0; 16];
            request[..8].copy_from_slice(&(id as u64 + 1).to_be_bytes());
            assert_eq!(history.admit(view, 1, 1, request), Ok(true));
            assert_eq!(history.admit(view, 1, 1, request), Ok(false));
        }
        assert_eq!(history.admit(view, 1, 1, [9; 16]), Err(()));
        for revision in 2..=300 {
            assert_eq!(history.admit(view, 1, revision, [1; 16]), Ok(true));
            assert_eq!(history.admit(view, 1, revision, [1; 16]), Ok(false));
        }
        for session in 1u64..=300 {
            let mut next = view;
            next.session[..8].copy_from_slice(&session.to_be_bytes());
            assert_eq!(history.admit(next, 1, 300, [1; 16]), Ok(true));
            assert_eq!(history.admit(next, 1, 300, [1; 16]), Ok(false));
        }
        let mut next = view;
        next.session[..8].copy_from_slice(&300u64.to_be_bytes());
        assert_eq!(history.admit(next, 2, 1, [1; 16]), Ok(true));
        assert_eq!(history.admit(next, 2, 1, [1; 16]), Ok(false));
    }

    #[test]
    fn media_wire_debug_never_discloses_metadata() {
        let secret = "private-now-playing-marker";
        let item = worker::MediaInfo {
            title: secret.into(),
            subtitle: secret.into(),
            app_id: secret.into(),
            ..native(1)
        };
        assert!(!format!("{item:?}").contains(secret));
        let sessions = worker::MediaSessions {
            sessions: vec![item],
            current_token: Some(1),
            revision: 1,
        };
        assert!(!format!("{sessions:?}").contains(secret));
        let info = current_native(&sessions).unwrap().unwrap();
        let message = encode_info([7; 16], 1, Some(&info), true);
        assert!(!format!("{message:?}").contains(secret));
    }

    fn live_scope() -> Scope {
        Scope {
            view: fsm::SessionView {
                parent: fsm::Parent {
                    peer: [1; 32],
                    connection_id: [2; 16],
                    generation: [3; 16],
                },
                session: [4; 16],
                worker_generation: [5; 16],
                phase: fsm::Phase::Active,
                reason: None,
            },
            current_parent: true,
            consented: true,
            capable: true,
            worker_generation: [5; 16],
        }
    }

    #[test]
    fn deferred_snapshot_is_quarantined_until_exact_session_becomes_active() {
        let active = live_scope();
        let mut pending_scope = active;
        pending_scope.view.phase = fsm::Phase::Committed;
        let info = current_native(&worker::MediaSessions {
            sessions: vec![native(1)],
            current_token: Some(1),
            revision: 1,
        })
        .unwrap()
        .unwrap();
        let pending = DeferredInfo {
            view: pending_scope.view,
            deadline: 100,
            message: encode_info(active.view.session, 1, Some(&info), true),
        };
        assert!(candidate_scope(pending_scope));
        assert!(!pending_scope.live());
        let mut relay = Relay::default();
        relay.scope(Some(pending_scope));
        assert!(!relay.update(
            active.view.parent,
            active.view.session,
            1,
            Some(info.clone())
        ));
        assert!(relay.info().is_none());
        assert!(pending.matches(active, 99));
        assert!(!pending.matches(active, 100));
        relay.scope(Some(active));
        assert!(relay.update(active.view.parent, active.view.session, 1, Some(info)));
        assert!(relay.info().is_some());
        for gate in 0..8 {
            let mut stale = active;
            match gate {
                0 => stale.current_parent = false,
                1 => stale.consented = false,
                2 => stale.capable = false,
                3 => stale.worker_generation = [9; 16],
                4 => stale.view.parent.connection_id = [9; 16],
                5 => stale.view.session = [9; 16],
                6 => stale.view.worker_generation = [9; 16],
                _ => stale.view.phase = fsm::Phase::Stopped,
            }
            assert!(!pending.matches(stale, 99), "gate {gate}");
        }
        relay.scope(None);
        assert!(relay.info().is_none());
    }

    #[test]
    fn source_execution_requires_local_permission_and_exact_live_binding() {
        let scope = live_scope();
        let info = current_native(&worker::MediaSessions {
            sessions: vec![native(1)],
            current_token: Some(1),
            revision: 1,
        })
        .unwrap()
        .unwrap();
        let mut relay = Relay::default();
        relay.scope(Some(scope));
        assert!(relay.update(scope.view.parent, scope.view.session, 1, Some(info.clone())));
        let command = net::MediaCommand {
            session_id: scope.view.session.to_vec(),
            token: info.session_token,
            revision: info.revision,
            ..Default::default()
        };
        let authorize = |s, allowed, parent, command: &net::MediaCommand| {
            authorize_source(
                s,
                Some(&relay),
                allowed,
                parent,
                command,
                model::MediaCommand::Play,
            )
        };
        assert_eq!(authorize(scope, true, scope.view.parent, &command), Ok(()));
        assert_eq!(
            authorize(scope, false, scope.view.parent, &command),
            Err(Refusal::ConsentRequired)
        );
        for gate in 0..6 {
            let mut stale = scope;
            match gate {
                0 => stale.current_parent = false,
                1 => stale.consented = false,
                2 => stale.capable = false,
                3 => stale.worker_generation = [9; 16],
                4 => stale.view.parent.generation = [9; 16],
                _ => stale.view.phase = fsm::Phase::Committed,
            }
            assert_eq!(
                authorize(stale, true, scope.view.parent, &command),
                Err(Refusal::Inactive)
            );
        }
        let mut stale = command.clone();
        stale.session_id = vec![9; 16];
        assert_eq!(
            authorize(scope, true, scope.view.parent, &stale),
            Err(Refusal::Inactive)
        );
        stale = command.clone();
        stale.token += 1;
        assert_eq!(
            authorize(scope, true, scope.view.parent, &stale),
            Err(Refusal::StaleRevision)
        );
        stale = command.clone();
        stale.revision += 1;
        assert_eq!(
            authorize(scope, true, scope.view.parent, &stale),
            Err(Refusal::StaleRevision)
        );
    }

    fn native(token: u64) -> worker::MediaInfo {
        worker::MediaInfo {
            token,
            revision: 2,
            app_id: "app".into(),
            title: "title".into(),
            state: worker::PlaybackState::Playing as i32,
            commands: 31,
            ..Default::default()
        }
    }

    #[test]
    fn current_os_token_is_not_inferred_from_first_session() {
        let sessions = worker::MediaSessions {
            sessions: vec![native(1), native(2)],
            current_token: Some(2),
            revision: 3,
        };
        assert_eq!(current_native(&sessions).unwrap().unwrap().session_token, 2);
        let empty = worker::MediaSessions {
            current_token: None,
            ..sessions.clone()
        };
        assert!(current_native(&empty).unwrap().is_none());
        let gone = worker::MediaSessions {
            current_token: Some(3),
            ..sessions
        };
        assert!(current_native(&gone).is_err());
    }

    #[test]
    fn native_metadata_and_command_masks_are_bounded() {
        let mut item = native(1);
        item.title = "x".repeat(257);
        let mut sessions = worker::MediaSessions {
            sessions: vec![item],
            current_token: Some(1),
            revision: 1,
        };
        assert!(current_native(&sessions).is_err());
        sessions.sessions[0] = native(1);
        sessions.sessions[0].commands = 32;
        assert!(current_native(&sessions).is_err());
    }

    #[test]
    fn metadata_roundtrip_and_clear_are_session_scoped() {
        let sessions = worker::MediaSessions {
            sessions: vec![native(1)],
            current_token: Some(1),
            revision: 1,
        };
        let info = current_native(&sessions).unwrap().unwrap();
        let encoded = encode_info([7; 16], 1, Some(&info), true);
        assert!(encoded.allow_control);
        assert_eq!(encoded.session_id, [7; 16]);
        assert_eq!(decode_info(&encoded).unwrap(), Some(info));
        let clear = encode_info([7; 16], 2, None, true);
        assert!(!clear.allow_control);
        assert!(!clear.present);
        assert_eq!(clear.token, 0);
        assert!(decode_info(&clear).unwrap().is_none());
    }

    #[test]
    fn real_native_result_and_gui_mapping_remain_typed() {
        assert_eq!(
            native_result(worker::MediaCommandStatus::Ok as i32),
            net::AudioControlResult::Ok
        );
        assert_eq!(
            native_result(worker::MediaCommandStatus::StaleRevision as i32),
            net::AudioControlResult::StaleRevision
        );
        assert_eq!(
            native_result(worker::MediaCommandStatus::Cancelled as i32),
            net::AudioControlResult::Cancelled
        );
        assert_eq!(
            result_code(net::AudioControlResult::StaleRevision),
            Err(ipc::audio::codes::AUDIO_STALE_MEDIA)
        );
        assert!(decode_command(0).is_none());
        for command in [
            model::MediaCommand::Play,
            model::MediaCommand::Pause,
            model::MediaCommand::Toggle,
            model::MediaCommand::Next,
            model::MediaCommand::Previous,
        ] {
            assert_eq!(
                decode_command(network_command(command) as i32),
                Some(command)
            );
        }
    }

    #[test]
    fn unknown_native_playback_survives_both_relay_boundaries() {
        let sessions = worker::MediaSessions {
            sessions: vec![worker::MediaInfo {
                token: 1,
                revision: 2,
                state: worker::PlaybackState::Unknown as i32,
                ..Default::default()
            }],
            current_token: Some(1),
            revision: 3,
        };
        let info = current_native(&sessions).unwrap().unwrap();
        assert_eq!(info.playback, model::PlaybackState::Unknown);
        let denied = encode_info([7; 16], 3, Some(&info), false);
        let allowed = encode_info([7; 16], 3, Some(&info), true);
        assert!(!denied.allow_control);
        assert!(allowed.allow_control);
        assert_eq!(denied.revision, allowed.revision);
        assert_eq!(allowed.playback, net::MediaPlayback::Unknown as i32);
        assert_eq!(
            decode_info(&allowed).unwrap().unwrap().playback,
            model::PlaybackState::Unknown
        );
    }
}
