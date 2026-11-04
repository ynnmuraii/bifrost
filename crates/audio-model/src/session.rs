//! Sans-I/O system-audio control. Inputs are authenticated, structurally validated values.
//! The caller supplies monotonic milliseconds, random IDs/nonces and SHA-256 results;
//! no clock, socket, worker, native backend or cryptographic implementation lives here.
//! Effects append to caller-owned storage, in security-significant order.
use crate::{jitter::Profile, keys, status::AudioRole};

pub type NodeId = [u8; 32];
pub type SessionId = [u8; 16];
pub const MAX_PEERS: usize = 4;
/// Never evict entries on a live parent: saturation refuses audio until parent replacement.
pub const USED_SESSION_LIMIT: usize = 256;
pub const USED_REQUEST_LIMIT: usize = 256;
pub const SETUP_TIMEOUT_MS: u64 = 10_000;
pub const PEER_TTL_MS: u32 = 3_000;
pub const CHILD_TTL_MS: u32 = 1_000;
pub const TRANSCRIPT_LEN: usize = b"bifrost-audio-transcript-v1".len() + keys::CONTEXT_LEN + 84;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parent {
    pub peer: NodeId,
    pub connection_id: [u8; 16],
    pub generation: [u8; 16],
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Support {
    pub control: bool,
    pub system_source: bool,
    pub listener: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Disabled,
    ConsentRequired,
    RoleConflict,
    UnsupportedFormat,
    DeviceUnavailable,
    VirtualMicMissing,
    ParentLost,
    WorkerLost,
    KeyExpired,
    Timeout,
    RateLimited,
    Revoked,
    UserStop,
    ProtocolViolation,
    GroupMismatch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Offered,
    Answered,
    Committed,
    Active,
    Rebuffering,
    DevicePaused,
    Stopping,
    Stopped,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupResolution {
    Off,
    Unconfigured,
    MintingListenerGroup,
    WaitingForListener { listener: NodeId },
    Resolved { group: [u8; 16], listener: NodeId },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub role: Option<AudioRole>,
    /// Explicit Advanced override or a previously resolved/minted group, never a guess.
    pub group: Option<[u8; 16]>,
    /// Pin an explicit Source Advanced group instead of following the selected Listener.
    pub group_override: bool,
    pub listener: Option<NodeId>,
    pub port: u16,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            role: None,
            group: None,
            group_override: false,
            listener: None,
            port: 47903,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub role: AudioRole,
    pub group: Option<[u8; 16]>,
    pub listener: NodeId,
    pub available: bool,
    pub allowed: bool,
    pub consent_revision: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offer {
    /// GUI correlation only; excluded from the canonical key context and transcript.
    pub request_id: Option<[u8; 16]>,
    pub session: SessionId,
    pub listener_nonce: [u8; 32],
    pub parent_connection_id: [u8; 16],
    pub group: [u8; 16],
    pub listener: NodeId,
    pub source: NodeId,
    pub format: keys::Format,
    /// Computed locally from format.canonical_bytes(), never copied from peer data.
    pub format_sha256: [u8; 32],
    pub listener_port: u16,
    pub consent_revision: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub session: SessionId,
    pub accepted: bool,
    pub reason: Option<Reason>,
    pub format: Option<keys::Format>,
    pub source_nonce: Option<[u8; 32]>,
    pub source_port: u16,
    pub consent_revision: u32,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartRequest {
    pub request_id: [u8; 16],
    pub group: [u8; 16],
    pub source: NodeId,
    pub consent_revision: u32,
    pub kind: keys::StreamKind,
    pub profile: Profile,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Hello(Hello),
    StartRequest(StartRequest),
    StartRefusal {
        request_id: [u8; 16],
        reason: Reason,
    },
    Offer(Offer),
    Answer(Answer),
    Commit {
        session: SessionId,
        transcript_sha256: [u8; 32],
    },
    Stop {
        session: SessionId,
        reason: Reason,
    },
    Keepalive {
        session: SessionId,
        serial: u64,
        authorized_for_ms: u32,
    },
    Consent {
        group: [u8; 16],
        revision: u32,
        allowed: bool,
    },
}

/// Public nonces and semantic agreement, not keys. Hash transcript_bytes() with SHA-256.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agreement {
    pub offer: Offer,
    pub source_nonce: [u8; 32],
    pub source_port: u16,
    pub source_consent_revision: u32,
}
impl Agreement {
    pub fn context(&self) -> keys::SessionContext {
        keys::SessionContext {
            parent_connection_id: self.offer.parent_connection_id,
            group_id: self.offer.group,
            session_id: self.offer.session,
            listener_nonce: self.offer.listener_nonce,
            source_nonce: self.source_nonce,
            nodes: [self.offer.listener, self.offer.source],
            kind: keys::StreamKind::System,
            format_sha256: self.offer.format_sha256,
        }
    }
    /// Domain, canonical key context, role-ordered node IDs, ports, consent revisions,
    /// fixed epoch and control TTL. Every integer is big endian; no protobuf hashing.
    pub fn transcript_bytes(&self) -> [u8; TRANSCRIPT_LEN] {
        let mut bytes = [0; TRANSCRIPT_LEN];
        let mut at = 0;
        let mut append = |part: &[u8]| {
            bytes[at..at + part.len()].copy_from_slice(part);
            at += part.len();
        };
        append(b"bifrost-audio-transcript-v1");
        append(&self.context().encode());
        append(&self.offer.listener);
        append(&self.offer.source);
        append(&self.offer.listener_port.to_be_bytes());
        append(&self.source_port.to_be_bytes());
        append(&self.offer.consent_revision.to_be_bytes());
        append(&self.source_consent_revision.to_be_bytes());
        append(&1u32.to_be_bytes());
        append(&PEER_TTL_MS.to_be_bytes());
        bytes
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionView {
    pub parent: Parent,
    pub session: SessionId,
    pub worker_generation: [u8; 16],
    pub phase: Phase,
    pub reason: Option<Reason>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    MintListenerGroup,
    PublishConfiguration(Settings),
    PublishStatus,
    Send {
        parent: Parent,
        message: Control,
    },
    /// Respond synchronously with fresh caller-generated IDs/nonces via Input::Start.
    /// The reserved per-parent slot counts toward the same three-session cap.
    GenerateListenerOffer {
        parent: Parent,
        request: StartRequest,
    },
    /// An accepted request has entered ordinary Offer setup, not yet activated media.
    StartRequestResult {
        parent: Parent,
        request_id: [u8; 16],
        result: Result<SessionId, Reason>,
    },
    GenerateSourceNonce {
        parent: Parent,
        session: SessionId,
    },
    DeriveKeys {
        parent: Parent,
        worker_generation: [u8; 16],
        agreement: Agreement,
    },
    PrepareChild {
        view: SessionView,
        agreement: Agreement,
        role: AudioRole,
    },
    StartChild {
        view: SessionView,
    },
    InvalidateSession {
        view: SessionView,
    },
    StopChild {
        view: SessionView,
    },
    RenewChild {
        view: SessionView,
        authorized_for_ms: u32,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Configure(Settings),
    SelectListener(NodeId),
    ListenerGroupMinted([u8; 16]),
    PeerUp {
        parent: Parent,
        support: Support,
        trusted: bool,
        local_allowed: bool,
        consent_revision: u32,
    },
    PeerDown(Parent),
    PeerSupport {
        parent: Parent,
        support: Support,
    },
    RevokePeer(NodeId),
    LocalConsent {
        peer: NodeId,
        allowed: bool,
        revision: u32,
    },
    Receive {
        parent: Parent,
        message: Control,
    },
    WorkerUp {
        generation: [u8; 16],
        capture: bool,
        render: bool,
    },
    WorkerCapabilities {
        generation: [u8; 16],
        capture: bool,
        render: bool,
    },
    WorkerHeartbeat {
        generation: [u8; 16],
    },
    WorkerLost {
        generation: [u8; 16],
    },
    RequestStart {
        parent: Parent,
        request_id: [u8; 16],
        kind: keys::StreamKind,
        profile: Profile,
    },
    Start {
        parent: Parent,
        session: SessionId,
        listener_nonce: [u8; 32],
        format: keys::Format,
        format_sha256: [u8; 32],
    },
    SourceNonce {
        parent: Parent,
        session: SessionId,
        nonce: [u8; 32],
    },
    KeysReady {
        view: SessionView,
        transcript_sha256: [u8; 32],
    },
    ChildPrepared(SessionView),
    /// The validated StartSession command was written; native startup is still unconfirmed.
    ChildStartIssued(SessionView),
    ChildStarted(SessionView),
    PathReady(SessionView),
    Buffering(SessionView),
    DeviceInvalidated(SessionView),
    ChildFailed {
        view: SessionView,
        reason: Reason,
    },
    ChildStopped(SessionView),
    Stop {
        peer: NodeId,
        reason: Reason,
    },
    Tick,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidSettings,
    TimeReversed,
    Capacity,
    StaleParent,
    StaleWorker,
    InvalidState,
    Refused(Reason),
}

struct Session {
    view: SessionView,
    offer: Offer,
    agreement: Option<Agreement>,
    digest: Option<[u8; 32]>,
    derive_issued: bool,
    prepare_issued: bool,
    start_issued: bool,
    started: bool,
    deadline: u64,
    setup_deadline: u64,
    peer_deadline: u64,
    receive_serial: Option<u64>,
    send_serial: u64,
    last_keepalive: u64,
    last_renewal: u64,
}
impl Session {
    fn live(&self) -> bool {
        !matches!(
            self.view.phase,
            Phase::Stopped | Phase::Failed | Phase::DevicePaused
        )
    }
    fn terminate(
        &mut self,
        reason: Reason,
        failed: bool,
        notify: bool,
        now: u64,
        out: &mut Vec<Effect>,
    ) {
        let native_pending = self.start_issued || self.started;
        // Logical stop expiry is not a native stop confirmation.
        if !self.live() && !(failed && self.view.phase == Phase::Stopped && native_pending)
            || self.view.phase == Phase::Stopping && !failed
        {
            return;
        }
        self.view.reason = Some(reason);
        self.view.phase = if failed {
            Phase::Failed
        } else if native_pending {
            Phase::Stopping
        } else {
            Phase::Stopped
        };
        // Revoke the local gate/keys before worker IPC or network can block.
        out.push(Effect::InvalidateSession { view: self.view });
        if native_pending {
            out.push(Effect::StopChild { view: self.view });
        }
        if notify {
            out.push(Effect::Send {
                parent: self.view.parent,
                message: Control::Stop {
                    session: self.view.session,
                    reason,
                },
            });
        }
        self.deadline = now.saturating_add(u64::from(CHILD_TTL_MS));
        out.push(Effect::PublishStatus);
    }
}
struct PendingRequest {
    request: StartRequest,
    outgoing: bool,
    deadline: u64,
}
struct Peer {
    parent: Parent,
    support: Support,
    trusted: bool,
    local_allowed: bool,
    local_revision: u32,
    remote_revision: u32,
    remote_allowed: bool,
    hello: Option<Hello>,
    used: Vec<SessionId>,
    last_offer: Option<u64>,
    used_requests: Vec<[u8; 16]>,
    last_request: Option<u64>,
    pending_request: Option<PendingRequest>,
    session: Option<Session>,
}
impl Peer {
    fn send(&self, message: Control, out: &mut Vec<Effect>) {
        if self.support.control && self.trusted {
            out.push(Effect::Send {
                parent: self.parent,
                message,
            });
        }
    }
    fn finish_request(&mut self, result: Result<SessionId, Reason>, out: &mut Vec<Effect>) {
        if let Some(pending) = self.pending_request.take() {
            if pending.outgoing {
                out.push(Effect::StartRequestResult {
                    parent: self.parent,
                    request_id: pending.request.request_id,
                    result,
                });
            } else if let Err(reason) = result {
                self.send(
                    Control::StartRefusal {
                        request_id: pending.request.request_id,
                        reason,
                    },
                    out,
                );
            }
        }
    }
    fn reserve_request(&mut self, request_id: [u8; 16], now: u64) -> bool {
        if self.used_requests.contains(&request_id)
            || self.used_requests.len() == USED_REQUEST_LIMIT
            || self
                .last_request
                .is_some_and(|last| now.saturating_sub(last) < 1000)
            || self.pending_request.is_some()
        {
            return false;
        }
        self.used_requests.push(request_id);
        self.last_request = Some(now);
        true
    }
    fn reserve(&mut self, session: SessionId, now: u64) -> Result<(), Reason> {
        if self.used.contains(&session) {
            return Err(Reason::ProtocolViolation);
        }
        if self.used.len() == USED_SESSION_LIMIT
            || self
                .last_offer
                .is_some_and(|last| now.saturating_sub(last) < 1000)
        {
            return Err(Reason::RateLimited);
        }
        self.last_offer = Some(now);
        self.used.push(session);
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct Worker {
    generation: [u8; 16],
    capture: bool,
    render: bool,
    last_health: u64,
}

pub struct ControlPlane {
    local: NodeId,
    settings: Settings,
    group: GroupResolution,
    peers: Vec<Peer>,
    worker: Option<Worker>,
    last_worker_generation: Option<[u8; 16]>,
    now: u64,
}
/// Diagnostic branch only: a pure borrowed projection for Core-owned tracing.
#[derive(Debug)]
pub struct DiagnosticHello<'a> {
    pub local_role: Option<AudioRole>,
    pub local_group: Option<&'a [u8; 16]>,
    pub enabled: bool,
    pub matching_parent: Option<&'a Parent>,
    pub trusted: Option<bool>,
    pub control: Option<bool>,
    pub accepted_hello: Option<&'a Hello>,
    pub remote_revision: Option<u32>,
    pub remote_allowed: Option<bool>,
}

impl ControlPlane {
    pub fn new(local: NodeId) -> Self {
        Self {
            local,
            settings: Settings::default(),
            group: GroupResolution::Off,
            peers: Vec::with_capacity(MAX_PEERS),
            worker: None,
            last_worker_generation: None,
            now: 0,
        }
    }
    pub fn group_resolution(&self) -> GroupResolution {
        self.group
    }
    pub fn enabled(&self) -> bool {
        self.settings.enabled && matches!(self.group, GroupResolution::Resolved { .. })
    }
    /// The publishable state never claims activation while minting or awaiting a Listener.
    pub fn configuration(&self) -> Settings {
        Settings {
            enabled: self.enabled(),
            ..self.settings
        }
    }
    pub fn session(&self, peer: NodeId) -> Option<SessionView> {
        self.peers
            .iter()
            .find(|entry| entry.parent.peer == peer)?
            .session
            .as_ref()
            .map(|session| session.view)
    }
    /// Diagnostic branch only; no I/O, clock, mutation, or owned peer copy.
    pub fn diagnostic_hello(&self, parent: Parent) -> DiagnosticHello<'_> {
        let peer = self.peer_index(parent).ok().map(|index| &self.peers[index]);
        DiagnosticHello {
            local_role: self.settings.role,
            local_group: self.settings.group.as_ref(),
            enabled: self.enabled(),
            matching_parent: peer.map(|peer| &peer.parent),
            trusted: peer.map(|peer| peer.trusted),
            control: peer.map(|peer| peer.support.control),
            accepted_hello: peer.and_then(|peer| peer.hello.as_ref()),
            remote_revision: peer.map(|peer| peer.remote_revision),
            remote_allowed: peer.map(|peer| peer.remote_allowed),
        }
    }

    pub fn handle(&mut self, now: u64, input: Input, out: &mut Vec<Effect>) -> Result<(), Error> {
        // Stale network callbacks cannot mutate a replacement link; local callers need an error.
        match input {
            Input::Receive { parent, .. } if self.peer_index(parent).is_err() => {
                return Ok(());
            }
            Input::RequestStart { parent, .. } | Input::Start { parent, .. } => {
                self.peer_index(parent)?;
            }
            _ => {}
        }
        if now < self.now {
            return Err(Error::TimeReversed);
        }
        self.now = now;
        self.expire(out);
        match input {
            Input::Configure(settings) => self.configure(settings, out)?,
            Input::SelectListener(listener) => {
                self.configure(
                    Settings {
                        enabled: false,
                        role: Some(AudioRole::Source),
                        listener: Some(listener),
                        group: None,
                        group_override: false,
                        ..self.settings
                    },
                    out,
                )?;
            }
            Input::ListenerGroupMinted(group) => {
                if self.group != GroupResolution::MintingListenerGroup {
                    return Err(Error::InvalidState);
                }
                self.settings.group = Some(group);
                self.group = GroupResolution::Resolved {
                    group,
                    listener: self.local,
                };
                self.publish_configuration(out);
                self.announce(out);
            }
            Input::PeerUp {
                parent,
                support,
                trusted,
                local_allowed,
                consent_revision,
            } => {
                if !trusted {
                    return Err(Error::Refused(Reason::Revoked));
                }
                if parent.peer == self.local {
                    return Err(Error::InvalidSettings);
                }
                if let Some(index) = self
                    .peers
                    .iter()
                    .position(|peer| peer.parent.peer == parent.peer)
                {
                    if self.peers[index].parent == parent {
                        return if self.peers[index].trusted {
                            Ok(())
                        } else {
                            Err(Error::Refused(Reason::Revoked))
                        };
                    }
                    self.peers[index].finish_request(Err(Reason::ParentLost), out);
                    if let Some(session) = &mut self.peers[index].session {
                        session.terminate(Reason::ParentLost, true, false, now, out);
                    }
                    self.peers.remove(index);
                }
                if self.peers.len() == MAX_PEERS {
                    return Err(Error::Capacity);
                }
                self.peers.push(Peer {
                    parent,
                    support,
                    trusted,
                    local_allowed,
                    local_revision: consent_revision,
                    remote_revision: 0,
                    remote_allowed: false,
                    hello: None,
                    used: Vec::with_capacity(USED_SESSION_LIMIT),
                    last_offer: None,
                    used_requests: Vec::with_capacity(USED_REQUEST_LIMIT),
                    last_request: None,
                    pending_request: None,
                    session: None,
                });
                self.announce(out);
            }
            Input::PeerDown(parent) => {
                if let Some(index) = self.peers.iter().position(|peer| peer.parent == parent) {
                    self.peers[index].finish_request(Err(Reason::ParentLost), out);
                    if let Some(session) = &mut self.peers[index].session {
                        session.terminate(Reason::ParentLost, true, false, now, out);
                    }
                    self.peers.remove(index);
                    out.push(Effect::PublishStatus);
                }
            }
            Input::PeerSupport { parent, support } => {
                let role = self.settings.role;
                let peer = self.peer_mut(parent)?;
                peer.support = support;
                if !support.control
                    || role == Some(AudioRole::Source) && !support.listener
                    || role == Some(AudioRole::Listener) && !support.system_source
                {
                    peer.finish_request(Err(Reason::Revoked), out);
                    if let Some(session) = &mut peer.session {
                        // Either side's matching UserStop can withdraw media, not control/trust.
                        let completing_user_stop = support.control
                            && session.view.reason == Some(Reason::UserStop)
                            && matches!(session.view.phase, Phase::Stopping | Phase::Stopped);
                        if !completing_user_stop {
                            session.terminate(Reason::Revoked, true, false, now, out);
                        }
                    }
                }
                let index = self.peer_index(parent)?;
                self.announce_peer(index, out);
            }
            Input::RevokePeer(id) => {
                if let Some(peer) = self.peers.iter_mut().find(|peer| peer.parent.peer == id) {
                    peer.trusted = false;
                    peer.local_allowed = false;
                    peer.remote_allowed = false;
                    peer.finish_request(Err(Reason::Revoked), out);
                    if let Some(session) = &mut peer.session {
                        session.terminate(Reason::Revoked, true, false, now, out);
                    }
                    out.push(Effect::PublishStatus);
                }
            }
            Input::LocalConsent {
                peer,
                allowed,
                revision,
            } => {
                let peer = self
                    .peers
                    .iter_mut()
                    .find(|entry| entry.parent.peer == peer)
                    .ok_or(Error::StaleParent)?;
                if allowed
                    && (revision < peer.local_revision
                        || !peer.local_allowed && revision == peer.local_revision)
                {
                    return Err(Error::InvalidState);
                }
                peer.local_revision = peer.local_revision.max(revision);
                peer.local_allowed = allowed;
                if !allowed {
                    peer.finish_request(Err(Reason::ConsentRequired), out);
                    if let Some(session) = &mut peer.session {
                        session.terminate(
                            Reason::ConsentRequired,
                            false,
                            peer.support.control,
                            now,
                            out,
                        );
                    }
                }
                if let Some(group) = self.settings.group {
                    peer.send(
                        Control::Consent {
                            group,
                            revision: peer.local_revision,
                            allowed: allowed && self.settings.enabled,
                        },
                        out,
                    );
                }
            }
            Input::WorkerUp {
                generation,
                capture,
                render,
            } => {
                if self.last_worker_generation == Some(generation) {
                    return Err(Error::StaleWorker);
                }
                self.lose_worker(out);
                self.last_worker_generation = Some(generation);
                self.worker = Some(Worker {
                    generation,
                    capture,
                    render,
                    last_health: now,
                });
                self.announce(out);
            }
            Input::WorkerCapabilities {
                generation,
                capture,
                render,
            } => {
                let worker = self
                    .worker
                    .as_mut()
                    .filter(|worker| worker.generation == generation)
                    .ok_or(Error::StaleWorker)?;
                if worker.capture == capture && worker.render == render {
                    return Ok(());
                }
                worker.capture = capture;
                worker.render = render;
                let unavailable = match self.settings.role {
                    Some(AudioRole::Source) => !capture,
                    Some(AudioRole::Listener) => !render,
                    None => false,
                };
                if unavailable {
                    for peer in &mut self.peers {
                        if let Some(session) = &mut peer.session {
                            if session.live() {
                                session.terminate(
                                    Reason::DeviceUnavailable,
                                    false,
                                    peer.support.control,
                                    now,
                                    out,
                                );
                                session.view.phase = Phase::DevicePaused;
                            }
                        }
                    }
                }
                self.announce(out);
            }
            Input::WorkerHeartbeat { generation } => {
                let worker = self
                    .worker
                    .as_mut()
                    .filter(|worker| worker.generation == generation)
                    .ok_or(Error::StaleWorker)?;
                worker.last_health = now;
            }
            Input::WorkerLost { generation } => {
                if self
                    .worker
                    .is_some_and(|worker| worker.generation == generation)
                {
                    self.lose_worker(out);
                }
            }
            Input::Receive {
                parent,
                message: Control::Hello(hello),
            } => self.hello(parent, hello, out)?,
            Input::Receive { parent, message } => {
                let index = self.peer_index(parent)?;
                self.receive(index, message, now, out)?;
            }
            Input::RequestStart {
                parent,
                request_id,
                kind,
                profile,
            } => {
                let index = self.peer_index(parent)?;
                let peer = &mut self.peers[index];
                Self::authorized(self.local, &self.settings, peer, AudioRole::Source)
                    .map_err(Error::Refused)?;
                if kind != keys::StreamKind::System {
                    return Err(Error::Refused(Reason::UnsupportedFormat));
                }
                if peer.session.as_ref().is_some_and(Session::live) {
                    return Err(Error::Refused(Reason::RateLimited));
                }
                if !peer.reserve_request(request_id, now) {
                    return Err(Error::Refused(Reason::RateLimited));
                }
                let request = StartRequest {
                    request_id,
                    group: self.settings.group.ok_or(Error::InvalidSettings)?,
                    source: self.local,
                    consent_revision: peer.local_revision,
                    kind,
                    profile,
                };
                peer.pending_request = Some(PendingRequest {
                    request,
                    outgoing: true,
                    deadline: now.saturating_add(SETUP_TIMEOUT_MS),
                });
                peer.send(Control::StartRequest(request), out);
            }
            Input::Start {
                parent,
                session,
                listener_nonce,
                format,
                format_sha256,
            } => {
                let index = self.peer_index(parent)?;
                let listener_full = self.listener_slots() >= crate::status::MAX_AUDIO_SESSIONS;
                let peer = &mut self.peers[index];
                let pending = peer
                    .pending_request
                    .as_ref()
                    .filter(|pending| !pending.outgoing);
                let request_id = pending.map(|pending| pending.request.request_id);
                let allowed =
                    Self::authorized(self.local, &self.settings, peer, AudioRole::Listener)
                        .and_then(|()| {
                            if !valid_format(format)
                                || pending.is_some_and(|pending| {
                                    pending.request.profile != format.profile
                                })
                            {
                                Err(Reason::UnsupportedFormat)
                            } else if pending.is_some_and(|pending| {
                                pending.request.consent_revision != peer.remote_revision
                            }) {
                                Err(Reason::ConsentRequired)
                            } else if !self.worker.is_some_and(|worker| worker.render) {
                                Err(Reason::DeviceUnavailable)
                            } else if peer.session.as_ref().is_some_and(Session::live)
                                || listener_full && pending.is_none()
                            {
                                Err(Reason::RateLimited)
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|()| peer.reserve(session, now));
                if let Err(reason) = allowed {
                    peer.finish_request(Err(reason), out);
                    return Err(Error::Refused(reason));
                }
                let worker = self.worker.ok_or(Error::StaleWorker)?;
                let offer = Offer {
                    request_id,
                    session,
                    listener_nonce,
                    parent_connection_id: parent.connection_id,
                    group: self.settings.group.ok_or(Error::InvalidSettings)?,
                    listener: self.local,
                    source: parent.peer,
                    format,
                    format_sha256,
                    listener_port: self.settings.port,
                    consent_revision: peer.local_revision,
                };
                peer.session = Some(Self::new_session(parent, worker.generation, offer, now));
                peer.finish_request(Ok(session), out);
                peer.send(Control::Offer(offer), out);
                out.push(Effect::PublishStatus);
            }
            Input::SourceNonce {
                parent,
                session,
                nonce,
            } => {
                let index = self.peer_index(parent)?;
                let peer = &mut self.peers[index];
                Self::authorized(self.local, &self.settings, peer, AudioRole::Source)
                    .map_err(Error::Refused)?;
                let entry = peer
                    .session
                    .as_mut()
                    .filter(|entry| {
                        entry.view.session == session
                            && entry.view.phase == Phase::Offered
                            && !entry.derive_issued
                    })
                    .ok_or(Error::InvalidState)?;
                let agreement = Agreement {
                    offer: entry.offer,
                    source_nonce: nonce,
                    source_port: self.settings.port,
                    source_consent_revision: peer.local_revision,
                };
                entry.agreement = Some(agreement);
                entry.derive_issued = true;
                out.push(Effect::DeriveKeys {
                    parent,
                    worker_generation: entry.view.worker_generation,
                    agreement,
                });
            }
            Input::KeysReady {
                view,
                transcript_sha256,
            } => {
                let role = self.settings.role.ok_or(Error::InvalidState)?;
                let entry = self.entry_mut(view)?;
                if !entry.derive_issued
                    || entry.digest.is_some()
                    || !matches!(entry.view.phase, Phase::Offered | Phase::Answered)
                {
                    return Err(Error::InvalidState);
                }
                entry.digest = Some(transcript_sha256);
                entry.prepare_issued = true;
                out.push(Effect::PrepareChild {
                    view: entry.view,
                    agreement: entry.agreement.ok_or(Error::InvalidState)?,
                    role,
                });
            }
            Input::ChildPrepared(view) => {
                let role = self.settings.role.ok_or(Error::InvalidState)?;
                let index = self.peer_index(view.parent)?;
                Self::authorized(self.local, &self.settings, &self.peers[index], role)
                    .map_err(Error::Refused)?;
                let peer = &mut self.peers[index];
                let entry = checked_entry(peer, view)?;
                if !entry.prepare_issued || entry.digest.is_none() {
                    return Err(Error::InvalidState);
                }
                if role == AudioRole::Source && entry.view.phase == Phase::Offered {
                    entry.view.phase = Phase::Answered;
                    let agreement = entry.agreement.ok_or(Error::InvalidState)?;
                    peer.send(
                        Control::Answer(Answer {
                            session: view.session,
                            accepted: true,
                            reason: None,
                            format: Some(agreement.offer.format),
                            source_nonce: Some(agreement.source_nonce),
                            source_port: agreement.source_port,
                            consent_revision: agreement.source_consent_revision,
                        }),
                        out,
                    );
                } else if role == AudioRole::Listener && entry.view.phase == Phase::Answered {
                    entry.view.phase = Phase::Committed;
                    out.push(Effect::StartChild { view: entry.view });
                } else {
                    return Err(Error::InvalidState);
                }
                out.push(Effect::PublishStatus);
            }
            Input::ChildStartIssued(view) => {
                let entry = self.entry_mut(view)?;
                if entry.view.phase != Phase::Committed {
                    return Err(Error::InvalidState);
                }
                entry.start_issued = true;
            }
            Input::ChildStarted(view) => {
                let role = self.settings.role.ok_or(Error::InvalidState)?;
                let peer = self.peer_mut(view.parent)?;
                let entry = checked_entry(peer, view)?;
                if entry.view.phase != Phase::Committed || entry.started {
                    return Err(Error::InvalidState);
                }
                entry.started = true;
                entry.peer_deadline = now.saturating_add(u64::from(PEER_TTL_MS));
                entry.deadline = entry.setup_deadline.min(entry.peer_deadline);
                entry.last_keepalive = now;
                entry.last_renewal = now;
                if role == AudioRole::Listener {
                    let digest = entry.digest.ok_or(Error::InvalidState)?;
                    peer.send(
                        Control::Commit {
                            session: view.session,
                            transcript_sha256: digest,
                        },
                        out,
                    );
                }
            }
            Input::PathReady(view) => {
                let entry = self.entry_mut(view)?;
                if !matches!(entry.view.phase, Phase::Committed | Phase::Rebuffering)
                    || !entry.started
                {
                    return Err(Error::InvalidState);
                }
                entry.view.phase = Phase::Active;
                entry.deadline = entry.peer_deadline;
                out.push(Effect::PublishStatus);
            }
            Input::Buffering(view) => {
                let entry = self.entry_mut(view)?;
                if entry.view.phase != Phase::Active {
                    return Err(Error::InvalidState);
                }
                entry.view.phase = Phase::Rebuffering;
                out.push(Effect::PublishStatus);
            }
            Input::DeviceInvalidated(view) => {
                let peer = self.peer_mut(view.parent)?;
                let entry = checked_entry(peer, view)?;
                if !entry.live() {
                    return Err(Error::InvalidState);
                }
                entry.view.phase = Phase::DevicePaused;
                entry.view.reason = Some(Reason::DeviceUnavailable);
                out.push(Effect::InvalidateSession { view: entry.view });
                out.push(Effect::StopChild { view: entry.view });
                peer.send(
                    Control::Stop {
                        session: view.session,
                        reason: Reason::DeviceUnavailable,
                    },
                    out,
                );
                out.push(Effect::PublishStatus);
            }
            Input::ChildFailed { view, reason } => {
                let peer = self.peer_mut(view.parent)?;
                let notify = peer.support.control && peer.trusted;
                checked_entry(peer, view)?.terminate(reason, true, notify, now, out);
            }
            Input::ChildStopped(view) => {
                let entry = self.entry_mut(view)?;
                if !matches!(entry.view.phase, Phase::Stopping | Phase::Stopped) {
                    return Err(Error::InvalidState);
                }
                entry.start_issued = false;
                entry.started = false;
                if entry.view.phase == Phase::Stopped {
                    return Ok(());
                }
                entry.view.phase = Phase::Stopped;
                out.push(Effect::PublishStatus);
            }
            Input::Stop { peer, reason } => {
                if let Some(peer) = self
                    .peers
                    .iter_mut()
                    .find(|entry| entry.parent.peer == peer)
                {
                    peer.finish_request(Err(reason), out);
                    if let Some(session) = &mut peer.session {
                        session.terminate(reason, false, peer.support.control, now, out);
                    }
                }
            }
            Input::Tick => self.renew(out),
        }
        Ok(())
    }
    fn peer_index(&self, parent: Parent) -> Result<usize, Error> {
        self.peers
            .iter()
            .position(|entry| entry.parent == parent)
            .ok_or(Error::StaleParent)
    }
    fn peer_mut(&mut self, parent: Parent) -> Result<&mut Peer, Error> {
        let index = self.peer_index(parent)?;
        Ok(&mut self.peers[index])
    }
    fn entry_mut(&mut self, view: SessionView) -> Result<&mut Session, Error> {
        checked_entry(self.peer_mut(view.parent)?, view)
    }
    fn new_session(parent: Parent, worker_generation: [u8; 16], offer: Offer, now: u64) -> Session {
        Session {
            view: SessionView {
                parent,
                session: offer.session,
                worker_generation,
                phase: Phase::Offered,
                reason: None,
            },
            offer,
            agreement: None,
            digest: None,
            derive_issued: false,
            prepare_issued: false,
            start_issued: false,
            started: false,
            deadline: now.saturating_add(SETUP_TIMEOUT_MS),
            setup_deadline: now.saturating_add(SETUP_TIMEOUT_MS),
            peer_deadline: now.saturating_add(SETUP_TIMEOUT_MS),
            receive_serial: None,
            send_serial: 0,
            last_keepalive: now,
            last_renewal: now,
        }
    }
    fn configure(&mut self, settings: Settings, out: &mut Vec<Effect>) -> Result<(), Error> {
        let now = self.now;
        if settings.port == 0
            || settings.port == 47902
            || (settings.enabled && settings.role.is_none())
            || (settings.group_override
                && (settings.role != Some(AudioRole::Source) || settings.group.is_none()))
            || (settings.role == Some(AudioRole::Source) && settings.listener == Some(self.local))
            || (settings.role == Some(AudioRole::Listener)
                && settings
                    .listener
                    .is_some_and(|listener| listener != self.local))
        {
            return Err(Error::InvalidSettings);
        }
        // Discovery pinning does not change the current authenticated transport.
        if (Settings {
            group_override: self.settings.group_override,
            ..settings
        }) != self.settings
        {
            let changed_scope = settings.role != self.settings.role
                || settings.group != self.settings.group
                || settings.listener != self.settings.listener;
            for peer in &mut self.peers {
                peer.finish_request(Err(Reason::Disabled), out);
                if let Some(session) = &mut peer.session {
                    session.terminate(Reason::Disabled, false, peer.support.control, now, out);
                }
                if changed_scope {
                    peer.local_allowed = false;
                }
            }
        }
        let was_minting =
            self.group == GroupResolution::MintingListenerGroup && settings == self.settings;
        self.settings = settings;
        self.group = match (settings.role, settings.group, settings.listener) {
            (None, _, _) => GroupResolution::Off,
            (Some(AudioRole::Listener), Some(group), _) => GroupResolution::Resolved {
                group,
                listener: self.local,
            },
            (Some(AudioRole::Listener), None, _) if settings.enabled => {
                if !was_minting {
                    out.push(Effect::MintListenerGroup);
                }
                GroupResolution::MintingListenerGroup
            }
            (Some(AudioRole::Source), Some(group), Some(listener)) => {
                GroupResolution::Resolved { group, listener }
            }
            (Some(AudioRole::Source), None, Some(listener)) => {
                GroupResolution::WaitingForListener { listener }
            }
            _ => GroupResolution::Unconfigured,
        };
        self.resolve_listener_group();
        self.publish_configuration(out);
        self.announce(out);
        Ok(())
    }
    fn publish_configuration(&self, out: &mut Vec<Effect>) {
        out.push(Effect::PublishConfiguration(self.configuration()));
        out.push(Effect::PublishStatus);
    }
    fn announce(&self, out: &mut Vec<Effect>) {
        for index in 0..self.peers.len() {
            self.announce_peer(index, out);
        }
    }
    fn announce_peer(&self, index: usize, out: &mut Vec<Effect>) {
        let Some(role) = self.settings.role else {
            return;
        };
        let listener = if role == AudioRole::Listener {
            self.local
        } else if let Some(listener) = self.settings.listener {
            listener
        } else {
            return;
        };
        if role == AudioRole::Listener && self.settings.group.is_none() {
            return;
        }
        let peer = &self.peers[index];
        if role == AudioRole::Source && peer.parent.peer != listener {
            return;
        }
        peer.send(
            Control::Hello(Hello {
                role,
                group: self.settings.group,
                listener,
                available: self.enabled()
                    && role == AudioRole::Listener
                    && self.worker.is_some_and(|worker| worker.render),
                allowed: self.enabled() && peer.local_allowed,
                consent_revision: peer.local_revision,
            }),
            out,
        );
    }
    fn resolve_listener_group(&mut self) -> bool {
        if self.settings.role != Some(AudioRole::Source) || self.settings.group_override {
            return false;
        }
        let Some(listener) = self.settings.listener else {
            return false;
        };
        let group = self
            .peers
            .iter()
            .find(|peer| peer.parent.peer == listener && peer.trusted && peer.support.control)
            .and_then(|peer| peer.hello)
            .filter(|hello| hello.role == AudioRole::Listener && hello.listener == listener)
            .and_then(|hello| hello.group);
        let Some(group) = group else {
            return false;
        };
        if self.settings.group == Some(group) {
            return false;
        }
        self.settings.group = Some(group);
        self.group = GroupResolution::Resolved { group, listener };
        true
    }
    fn hello(&mut self, parent: Parent, hello: Hello, out: &mut Vec<Effect>) -> Result<(), Error> {
        let now = self.now;
        let index = self.peer_index(parent)?;
        if !self.peers[index].trusted {
            return Err(Error::Refused(Reason::Revoked));
        }
        if !self.peers[index].support.control {
            return Err(Error::Refused(Reason::Disabled));
        }
        if hello.role == AudioRole::Listener
            && (hello.listener != parent.peer || hello.group.is_none())
        {
            return Err(Error::Refused(Reason::RoleConflict));
        }
        let peer = &mut self.peers[index];
        let changed = peer.hello.is_some_and(|previous| {
            previous.role != hello.role
                || previous.group != hello.group
                || previous.listener != hello.listener
        });
        if hello.consent_revision < peer.remote_revision {
            return Ok(());
        }
        if !hello.allowed
            || peer.hello.is_none()
            || changed
            || hello.consent_revision > peer.remote_revision
        {
            peer.remote_allowed = hello.allowed;
        }
        peer.remote_revision = if changed {
            hello.consent_revision
        } else {
            peer.remote_revision.max(hello.consent_revision)
        };
        peer.hello = Some(hello);
        if changed || !peer.remote_allowed || !hello.available && hello.role == AudioRole::Listener
        {
            peer.finish_request(
                Err(if changed {
                    Reason::GroupMismatch
                } else {
                    Reason::ConsentRequired
                }),
                out,
            );
            if let Some(session) = &mut peer.session {
                session.terminate(
                    if changed {
                        Reason::GroupMismatch
                    } else {
                        Reason::ConsentRequired
                    },
                    false,
                    peer.support.control,
                    now,
                    out,
                );
            }
        }
        if self.resolve_listener_group() {
            self.publish_configuration(out);
            self.announce(out);
        }
        if self.settings.role == Some(AudioRole::Listener) && hello.role == AudioRole::Source {
            self.announce_peer(index, out);
        }
        Ok(())
    }
    fn authorized(
        local: NodeId,
        settings: &Settings,
        peer: &Peer,
        role: AudioRole,
    ) -> Result<(), Reason> {
        if !settings.enabled || settings.role != Some(role) || !peer.support.control {
            return Err(Reason::Disabled);
        }
        if !peer.trusted {
            return Err(Reason::Revoked);
        }
        let hello = peer.hello.ok_or(Reason::RoleConflict)?;
        let group = settings.group.ok_or(Reason::RoleConflict)?;
        if hello.group.is_some() && hello.group != Some(group) {
            return Err(Reason::GroupMismatch);
        }
        if !peer.local_allowed || !peer.remote_allowed {
            return Err(Reason::ConsentRequired);
        }
        match role {
            AudioRole::Listener
                if peer.support.system_source
                    && hello.role == AudioRole::Source
                    && hello.group == Some(group)
                    && hello.listener == local =>
            {
                Ok(())
            }
            AudioRole::Source
                if peer.support.listener
                    && hello.role == AudioRole::Listener
                    && hello.available
                    && settings.listener == Some(peer.parent.peer)
                    && hello.listener == peer.parent.peer
                    && hello.group == Some(group) =>
            {
                Ok(())
            }
            _ => Err(Reason::RoleConflict),
        }
    }
    fn listener_slots(&self) -> usize {
        self.peers
            .iter()
            .filter(|peer| {
                peer.session.as_ref().is_some_and(Session::live)
                    || peer
                        .pending_request
                        .as_ref()
                        .is_some_and(|pending| !pending.outgoing)
            })
            .count()
    }
    fn receive(
        &mut self,
        index: usize,
        message: Control,
        now: u64,
        out: &mut Vec<Effect>,
    ) -> Result<(), Error> {
        let listener_full = self.listener_slots() >= crate::status::MAX_AUDIO_SESSIONS;
        let local = self.local;
        let settings = &self.settings;
        let worker = self.worker;
        let peer = &mut self.peers[index];
        if !peer.trusted {
            return Err(Error::Refused(Reason::Revoked));
        }
        if !peer.support.control {
            return Err(Error::Refused(Reason::Disabled));
        }
        match message {
            Control::StartRequest(request) => {
                if settings.role != Some(AudioRole::Listener) {
                    return Ok(());
                }
                if !peer.reserve_request(request.request_id, now) {
                    return Ok(());
                }
                let allowed = Self::authorized(local, settings, peer, AudioRole::Listener)
                    .and_then(|()| {
                        if request.kind != keys::StreamKind::System {
                            Err(Reason::UnsupportedFormat)
                        } else if Some(request.group) != settings.group {
                            Err(Reason::GroupMismatch)
                        } else if request.source != peer.parent.peer {
                            Err(Reason::RoleConflict)
                        } else if request.consent_revision != peer.remote_revision {
                            Err(Reason::ConsentRequired)
                        } else if listener_full || peer.session.as_ref().is_some_and(Session::live)
                        {
                            Err(Reason::RateLimited)
                        } else if !worker.is_some_and(|worker| worker.render) {
                            Err(Reason::DeviceUnavailable)
                        } else {
                            Ok(())
                        }
                    });
                if let Err(reason) = allowed {
                    peer.send(
                        Control::StartRefusal {
                            request_id: request.request_id,
                            reason,
                        },
                        out,
                    );
                    return Ok(());
                }
                peer.pending_request = Some(PendingRequest {
                    request,
                    outgoing: false,
                    deadline: now.saturating_add(SETUP_TIMEOUT_MS),
                });
                out.push(Effect::GenerateListenerOffer {
                    parent: peer.parent,
                    request,
                });
            }
            Control::StartRefusal { request_id, reason } => {
                if peer.pending_request.as_ref().is_some_and(|pending| {
                    pending.outgoing && pending.request.request_id == request_id
                }) {
                    peer.finish_request(Err(reason), out);
                }
            }
            Control::Offer(offer) => {
                if let Some(request_id) = offer.request_id {
                    if !peer.pending_request.as_ref().is_some_and(|pending| {
                        pending.outgoing
                            && pending.request.request_id == request_id
                            && pending.request.profile == offer.format.profile
                            && pending.request.group == offer.group
                    }) {
                        return Ok(());
                    }
                } else if peer
                    .pending_request
                    .as_ref()
                    .is_some_and(|pending| pending.outgoing)
                {
                    return Ok(());
                }
                if let Err(reason) = peer.reserve(offer.session, now) {
                    if reason == Reason::ProtocolViolation {
                        if let Some(session) = &mut peer.session {
                            if session.view.session == offer.session {
                                session.terminate(reason, true, false, now, out);
                            }
                        }
                        peer.send(
                            Control::Stop {
                                session: offer.session,
                                reason,
                            },
                            out,
                        );
                    } else {
                        reject_offer(peer, offer.session, reason, out);
                    }
                    if offer.request_id.is_some() {
                        peer.finish_request(Err(reason), out);
                    }
                    return Err(Error::Refused(reason));
                }
                let allowed = Self::authorized(local, settings, peer, AudioRole::Source)
                    .and_then(|()| {
                        if offer.parent_connection_id != peer.parent.connection_id
                            || offer.listener != peer.parent.peer
                            || offer.source != local
                        {
                            Err(Reason::ProtocolViolation)
                        } else if Some(offer.group) != settings.group {
                            Err(Reason::GroupMismatch)
                        } else if offer.consent_revision != peer.remote_revision {
                            Err(Reason::ConsentRequired)
                        } else {
                            Ok(())
                        }
                    })
                    .and_then(|()| {
                        if !valid_format(offer.format)
                            || offer.listener_port == 0
                            || offer.listener_port == 47902
                        {
                            Err(Reason::UnsupportedFormat)
                        } else {
                            Ok(())
                        }
                    })
                    .and_then(|()| {
                        if peer.session.as_ref().is_some_and(Session::live) {
                            Err(Reason::RateLimited)
                        } else {
                            Ok(())
                        }
                    });
                let worker = worker.filter(|worker| worker.capture);
                if let Err(reason) =
                    allowed.and_then(|()| worker.map(|_| ()).ok_or(Reason::DeviceUnavailable))
                {
                    reject_offer(peer, offer.session, reason, out);
                    if offer.request_id.is_some() {
                        peer.finish_request(Err(reason), out);
                    }
                    return Err(Error::Refused(reason));
                }
                peer.session = Some(Self::new_session(
                    peer.parent,
                    worker.ok_or(Error::StaleWorker)?.generation,
                    offer,
                    now,
                ));
                if offer.request_id.is_some() {
                    peer.finish_request(Ok(offer.session), out);
                }
                out.push(Effect::GenerateSourceNonce {
                    parent: peer.parent,
                    session: offer.session,
                });
                out.push(Effect::PublishStatus);
            }
            Control::Answer(answer) => {
                Self::authorized(local, settings, peer, AudioRole::Listener)
                    .map_err(Error::Refused)?;
                let entry = peer
                    .session
                    .as_mut()
                    .filter(|entry| {
                        entry.view.session == answer.session
                            && entry.view.phase == Phase::Offered
                            && !entry.derive_issued
                    })
                    .ok_or(Error::InvalidState)?;
                if !answer.accepted {
                    if answer.reason.is_none()
                        || answer.format.is_some()
                        || answer.source_nonce.is_some()
                        || answer.source_port != 0
                    {
                        return Err(Error::Refused(Reason::ProtocolViolation));
                    }
                    entry.terminate(
                        answer.reason.ok_or(Error::InvalidState)?,
                        true,
                        false,
                        now,
                        out,
                    );
                } else {
                    if answer.reason.is_some()
                        || answer.format != Some(entry.offer.format)
                        || answer.source_port == 0
                        || answer.source_port == 47902
                    {
                        return Err(Error::Refused(Reason::UnsupportedFormat));
                    }
                    if answer.consent_revision != peer.remote_revision {
                        return Err(Error::Refused(Reason::ConsentRequired));
                    }
                    let agreement = Agreement {
                        offer: entry.offer,
                        source_nonce: answer
                            .source_nonce
                            .ok_or(Error::Refused(Reason::ProtocolViolation))?,
                        source_port: answer.source_port,
                        source_consent_revision: answer.consent_revision,
                    };
                    entry.agreement = Some(agreement);
                    entry.derive_issued = true;
                    entry.view.phase = Phase::Answered;
                    out.push(Effect::DeriveKeys {
                        parent: peer.parent,
                        worker_generation: entry.view.worker_generation,
                        agreement,
                    });
                }
            }
            Control::Commit {
                session,
                transcript_sha256,
            } => {
                Self::authorized(local, settings, peer, AudioRole::Source)
                    .map_err(Error::Refused)?;
                let entry = peer
                    .session
                    .as_mut()
                    .filter(|entry| {
                        entry.view.session == session && entry.view.phase == Phase::Answered
                    })
                    .ok_or(Error::InvalidState)?;
                if entry.digest != Some(transcript_sha256) {
                    entry.terminate(Reason::ProtocolViolation, true, true, now, out);
                    return Err(Error::Refused(Reason::ProtocolViolation));
                }
                entry.view.phase = Phase::Committed;
                out.push(Effect::StartChild { view: entry.view });
                out.push(Effect::PublishStatus);
            }
            Control::Stop { session, reason } => {
                if let Some(entry) = peer
                    .session
                    .as_mut()
                    .filter(|entry| entry.view.session == session)
                {
                    entry.terminate(reason, false, false, now, out);
                }
            }
            Control::Keepalive {
                session,
                serial,
                authorized_for_ms,
            } => {
                if authorized_for_ms == 0 || authorized_for_ms > PEER_TTL_MS {
                    return Err(Error::Refused(Reason::ProtocolViolation));
                }
                let entry = peer
                    .session
                    .as_mut()
                    .filter(|entry| {
                        entry.view.session == session
                            && entry.started
                            && matches!(
                                entry.view.phase,
                                Phase::Committed | Phase::Active | Phase::Rebuffering
                            )
                    })
                    .ok_or(Error::InvalidState)?;
                if entry.receive_serial.is_none_or(|last| serial > last) {
                    entry.receive_serial = Some(serial);
                    entry.peer_deadline = now.saturating_add(u64::from(authorized_for_ms));
                    entry.deadline = if entry.view.phase == Phase::Committed {
                        entry.setup_deadline.min(entry.peer_deadline)
                    } else {
                        entry.peer_deadline
                    };
                }
            }
            Control::Consent {
                group,
                revision,
                allowed,
            } => {
                if Some(group) != settings.group {
                    return Err(Error::Refused(Reason::GroupMismatch));
                }
                if allowed && revision <= peer.remote_revision {
                    return Ok(());
                }
                peer.remote_revision = peer.remote_revision.max(revision);
                peer.remote_allowed = allowed;
                if !allowed {
                    peer.finish_request(Err(Reason::ConsentRequired), out);
                    if let Some(entry) = &mut peer.session {
                        entry.terminate(Reason::ConsentRequired, false, true, now, out);
                    }
                }
            }
            Control::Hello(_) => {
                unreachable!("Hello changes group resolution before peer processing")
            }
        }
        Ok(())
    }
    fn lose_worker(&mut self, out: &mut Vec<Effect>) {
        let now = self.now;
        if self.worker.take().is_some() {
            for peer in &mut self.peers {
                peer.finish_request(Err(Reason::WorkerLost), out);
                if let Some(session) = &mut peer.session {
                    session.terminate(Reason::WorkerLost, true, peer.support.control, now, out);
                }
            }
            out.push(Effect::PublishStatus);
            self.announce(out);
        }
    }
    fn expire(&mut self, out: &mut Vec<Effect>) {
        let now = self.now;
        if self.worker.is_some_and(|worker| {
            self.now.saturating_sub(worker.last_health) >= u64::from(CHILD_TTL_MS)
        }) {
            self.lose_worker(out);
        }
        for peer in &mut self.peers {
            if peer
                .pending_request
                .as_ref()
                .is_some_and(|pending| now >= pending.deadline)
            {
                peer.finish_request(Err(Reason::Timeout), out);
            }
            if let Some(session) = &mut peer.session {
                if session.live() && self.now >= session.deadline {
                    if session.view.phase == Phase::Stopping {
                        session.view.phase = Phase::Stopped;
                        out.push(Effect::PublishStatus);
                    } else {
                        session.terminate(Reason::Timeout, true, peer.support.control, now, out);
                    }
                }
            }
        }
    }
    fn renew(&mut self, out: &mut Vec<Effect>) {
        let now = self.now;
        for peer in &mut self.peers {
            let Some(session) = &mut peer.session else {
                continue;
            };
            if !session.started
                || !matches!(
                    session.view.phase,
                    Phase::Committed | Phase::Active | Phase::Rebuffering
                )
            {
                continue;
            }
            if self.now.saturating_sub(session.last_renewal) >= 250 {
                session.last_renewal = self.now;
                out.push(Effect::RenewChild {
                    view: session.view,
                    authorized_for_ms: CHILD_TTL_MS,
                });
            }
            if self.now.saturating_sub(session.last_keepalive) >= 1000 {
                session.last_keepalive = self.now;
                session.send_serial = match session.send_serial.checked_add(1) {
                    Some(serial) => serial,
                    None => {
                        session.terminate(
                            Reason::ProtocolViolation,
                            true,
                            peer.support.control,
                            now,
                            out,
                        );
                        continue;
                    }
                };
                out.push(Effect::Send {
                    parent: peer.parent,
                    message: Control::Keepalive {
                        session: session.view.session,
                        serial: session.send_serial,
                        authorized_for_ms: PEER_TTL_MS,
                    },
                });
            }
        }
    }
}
fn checked_entry(peer: &mut Peer, view: SessionView) -> Result<&mut Session, Error> {
    peer.session
        .as_mut()
        .filter(|entry| {
            entry.view.session == view.session
                && entry.view.worker_generation == view.worker_generation
        })
        .ok_or(Error::StaleWorker)
}
fn reject_offer(peer: &Peer, session: SessionId, reason: Reason, out: &mut Vec<Effect>) {
    peer.send(
        Control::Answer(Answer {
            session,
            accepted: false,
            reason: Some(reason),
            format: None,
            source_nonce: None,
            source_port: 0,
            consent_revision: peer.local_revision,
        }),
        out,
    );
}
fn valid_format(format: keys::Format) -> bool {
    format.sample_rate == 48_000
        && format.channels == 2
        && format.max_datagram == 1200
        && match format.profile {
            Profile::Ethernet => {
                format.frame_samples == 480 && format.bitrate == 96_000 && !format.fec
            }
            Profile::Wifi => format.frame_samples == 960 && format.bitrate == 64_000 && format.fec,
        }
}
