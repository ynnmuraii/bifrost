//! Clipboard coordination in core: the policy, the offers and the fan-out (ADR-008,
//! KVM-041..044).
//!
//! `docs/04-decisions.md` ADR-008 and `docs/05-protocol.md` («Clipboard канал») split the work
//! three ways, and this module is the middle one: `crates/clipboard` owns the pure decisions
//! (what may leave, what may be applied, when the echo stops, how fast a channel may talk), the
//! platform adapters own the OS selection behind `lws-agent`, and **core owns the wire**.
//!
//! # The shape of a copy
//!
//! 1. The agent reports a local change with `ClipboardChanged` (chunked, ≤32 KiB per frame).
//!    This module assembles it, asks [`cb::Source`] whether it may be offered, and mints the new
//!    `origin_version`.
//! 2. Follow mode carries the offer until the session addresses it to a peer; broadcast mode
//!    offers it to every member. A peer without a channel first receives `ClipboardGrant`
//!    over the input link, then dials back with `ClipboardBind`.
//! 3. A received text/PNG payload is verified (count, size, SHA-256, UTF-8/PNG signature)
//!    and only then handed to the agent as `SetClipboardText`. Peer payloads are never re-offered
//!    ([`cb::Source::apply_from_peer`]), which is what keeps the fan-out from looping.
//!
//! # Why the transfer is its own task
//!
//! `KVM-042`: «объём clipboard не блокирует key-up». A 1 MiB payload at 256 KiB/s takes four
//! seconds if it is paced honestly, and the session loop drives the input link — so the paced
//! sender runs on a task of its own and the session only ever hands it a payload. Everything
//! this module does *inside* the session loop is bookkeeping.
//!
//! Text stays capped at 1 MiB. PNG is a separate opt-in, capped at 32 MiB, gated by
//! backend support and peer capability 15. Images are transient carry/transfer bytes,
//! never history rows. Incoming reservations cover assembly, stale verification jobs and
//! the agent handoff until its final owner drops. Text and image byte budgets are independent.
//! The network reader waits for Offer admission, then backpressures a four-frame/64 KiB
//! byte-accounted assembler queue directly; payload frames never queue on the input loop.

use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clipboard as cb;
use geometry::NodeId;
use protocol::ipc;
use tokio::sync::{mpsc, Mutex};

use transport::clipboard::{
    dial_clipboard, ClipboardConnection, ClipboardListener, ClipboardMessage, ClipboardReader,
    ClipboardWriter, Grants, Outgoing,
};

/// The chunk size the agent's frames use: `MAX_FRAME_IPC` is 64 KiB, so 32 KiB leaves room for
/// the frame's own fields. A 1 MiB payload is 32 chunks.
const IPC_CHUNK_BYTES: usize = 32 * 1024;

const INGRESS_FRAMES: usize = 4;
const INGRESS_BYTES: usize = INGRESS_FRAMES * cb::CHUNK_BYTES;

/// Reader-only bounded admission handle.
#[derive(Clone, Debug)]
pub struct IncomingRoute {
    state: Arc<std::sync::Mutex<IngressState>>,
    bytes: Arc<tokio::sync::Semaphore>,
    cancelled: tokio::sync::watch::Sender<bool>,
    offer_id: [u8; 16],
    size: u64,
}

#[derive(Debug)]
struct IngressState {
    sender: Option<mpsc::Sender<IncomingChunk>>,
    received: u64,
    complete: bool,
}

#[derive(Debug)]
struct IncomingChunk {
    message: ClipboardMessage,
    _bytes: tokio::sync::OwnedSemaphorePermit,
}

impl IncomingRoute {
    fn new(offer_id: [u8; 16], size: u64) -> (Self, mpsc::Receiver<IncomingChunk>) {
        let (sender, receiver) = mpsc::channel(INGRESS_FRAMES);
        (
            Self {
                state: Arc::new(std::sync::Mutex::new(IngressState {
                    sender: Some(sender),
                    received: 0,
                    complete: false,
                })),
                bytes: Arc::new(tokio::sync::Semaphore::new(INGRESS_BYTES)),
                cancelled: tokio::sync::watch::channel(false).0,
                offer_id,
                size,
            },
            receiver,
        )
    }

    fn cancel(&self) {
        self.bytes.close();
        self.cancelled.send_replace(true);
        self.state.lock().expect("clipboard ingress").sender = None;
    }

    async fn send(&self, message: ClipboardMessage) -> Result<(), ()> {
        let (id, len, complete) = match &message {
            ClipboardMessage::Data { offer_id, data, .. } => (*offer_id, data.len(), false),
            ClipboardMessage::Complete { offer_id, .. } => (*offer_id, 0, true),
            _ => return Err(()),
        };
        if id != self.offer_id || len > cb::CHUNK_BYTES {
            return Err(());
        }
        let sender = {
            let mut state = self.state.lock().expect("clipboard ingress");
            if state.complete || len as u64 > self.size.saturating_sub(state.received) {
                return Err(());
            }
            let sender = state.sender.clone().ok_or(())?;
            state.received += len as u64;
            state.complete = complete;
            sender
        };
        let permit = Arc::clone(&self.bytes)
            .acquire_many_owned(len as u32)
            .await
            .map_err(|_| ())?;
        let mut cancelled = self.cancelled.subscribe();
        if *cancelled.borrow() {
            return Err(());
        }
        let chunk = IncomingChunk {
            message,
            _bytes: permit,
        };
        tokio::select! {
            biased;
            _ = cancelled.changed() => Err(()),
            sent = sender.send(chunk) => sent.map_err(|_| ()),
        }
    }
}

/// Closing or aborting a network reader must revoke its outstanding routes.
struct IncomingReadRoutes(BTreeMap<[u8; 16], IncomingRoute>);

impl Drop for IncomingReadRoutes {
    fn drop(&mut self) {
        for route in self.0.values() {
            route.cancel();
        }
    }
}

trait ClipboardFrameReader {
    fn read_frame(&mut self) -> impl Future<Output = Option<ClipboardMessage>> + Send;
}

impl ClipboardFrameReader for ClipboardReader {
    async fn read_frame(&mut self) -> Option<ClipboardMessage> {
        self.recv().await.ok()
    }
}

/// Everything `[clipboard]` decides.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// `clipboard.enabled`: the capability bit and the whole channel hang off it.
    pub enabled: bool,
    /// Independent text/image opt-ins and size limits, plus secret filtering.
    pub policy: cb::Policy,
    /// Whether copies follow focus or are offered to every peer.
    pub routing_policy: config::ClipboardRoutingPolicy,
    /// Session-owned focus settling delay.
    pub settle_ms: u32,
    /// `clipboard.rate_bytes_per_s`, handed to the paced sender.
    pub rate_bytes_per_s: u64,
    /// PNG transfer pacing, independent of the text rate.
    pub image_rate_bytes_per_s: u64,
    /// `clipboard.transfer`: ask for an offer as soon as it arrives, or only when focus enters.
    pub eager: bool,
}

impl Settings {
    /// Reads the settings out of `config.toml`.
    pub fn from_config(clipboard: &config::ClipboardConfig) -> Self {
        Settings {
            enabled: clipboard.enabled,
            policy: cb::Policy {
                enabled: clipboard.enabled,
                skip_secret_marked: clipboard.skip_secret_marked,
                max_bytes: clipboard.max_bytes,
                images: clipboard.images,
                max_image_bytes: clipboard.max_image_bytes,
            },
            routing_policy: clipboard.policy,
            settle_ms: clipboard.settle_ms,
            rate_bytes_per_s: clipboard.rate_bytes_per_s,
            image_rate_bytes_per_s: clipboard.image_rate_bytes_per_s,
            eager: matches!(clipboard.transfer, config::ClipboardTransfer::OnOffer),
        }
    }
}

/// What the coordinator wants the session to do, in order.
#[derive(Debug)]
pub enum Out {
    /// A validated local copy is available for addressed delivery.
    Carried {
        /// Local origin.
        origin: NodeId,
        /// Stable source version.
        version: u64,
        /// Payload digest.
        sha256: [u8; 32],
    },
    /// A new local copy invalidated the previous carry, even if the new copy was refused.
    CarryCleared,
    /// An addressed offer was queued for a peer.
    Delivery {
        /// Destination.
        peer: NodeId,
        /// Source version used by the offer.
        version: u64,
        /// Payload digest used by the completion.
        sha256: [u8; 32],
    },
    /// Associate the following agent apply with its sender before forwarding agent chunks.
    ApplyPending {
        /// Sending peer.
        peer: NodeId,
        /// Incoming source version.
        version: u64,
        /// Verified payload digest.
        sha256: [u8; 32],
        /// Kind for selective image revocation.
        kind: cb::ContentKind,
    },
    /// Send this clipboard message on the **input** link to the peer (a grant).
    Grant {
        /// The peer.
        peer: NodeId,
        /// The grant.
        message: ClipboardMessage,
    },
    /// Open the clipboard channel a grant authorised.
    Dial {
        /// The peer the grant came from.
        peer: NodeId,
        /// Where its listener is: the address the live input link comes from, at the port the
        /// grant advertised.
        address: SocketAddr,
        /// The parent input link's connection id.
        parent: [u8; 16],
        /// The channel id the grant named.
        channel_id: [u8; 16],
        /// The one-use binding token.
        token: [u8; 32],
    },
    /// A verified payload, streamed to the agent without building every IPC frame.
    AgentPayload {
        version: u64,
        payload: Arc<Vec<u8>>,
        sha256: [u8; 32],
        kind: cb::ContentKind,
        reservation: Option<Arc<IncomingReservation>>,
    },
    /// Withdraw agent-bound PNG work without revoking text.
    RevokeImages,
    /// A completed transfer, for the session-owned history ring.
    History {
        /// The transfer's remote peer.
        peer: NodeId,
        /// Which side sent the payload.
        direction: ipc::ClipboardHistoryDirection,
        /// Full verified UTF-8 text.
        text: String,
        /// Shared verified payload, reused by addressed history sends.
        payload: Arc<Vec<u8>>,
        /// Digest prepared outside the session.
        sha256: [u8; 32],
        /// Secret text must be discarded by history.
        secret: bool,
    },
    /// Tell the peer why a transfer is off (`ClipboardCancel` with the documented code).
    Cancel {
        /// The peer.
        peer: NodeId,
        /// The offer the refusal answers.
        offer_id: [u8; 16],
        /// The wire code.
        reason: cb::ClipboardError,
    },
}

/// What the runtime tells the session loop.
#[derive(Debug)]
pub enum Inbound {
    /// Off-loop assembly and verification finished.
    Verified(Verified),
    /// A local copy finished preparation away from the session loop.
    LocalPrepared {
        generation: u64,
        prepared: Result<PreparedCopy, cb::ClipboardError>,
    },
    /// A full outbound transfer successfully written to its channel.
    Sent {
        /// The receiving peer.
        peer: NodeId,
        /// Source version of the addressed offer.
        version: u64,
        /// Digest of the sent payload.
        sha256: [u8; 32],
        /// Text-only history data; image success still reports transfer identity.
        history: Option<(String, Arc<Vec<u8>>)>,
        /// Secret classification captured with the request.
        secret: bool,
    },
    /// An addressed transfer could not finish.
    Failed {
        /// Destination peer.
        peer: NodeId,
        /// Source version of the failed offer.
        version: u64,
        /// Digest of the payload.
        sha256: [u8; 32],
        /// Failure description without clipboard content.
        detail: String,
    },
    /// A peer's clipboard channel is accepted, bound and validated; its messages follow.
    Bound {
        /// Which peer.
        peer: NodeId,
        /// The channel the bind named.
        channel_id: [u8; 16],
        /// Authenticated parent input link, retained through queued events.
        parent: [u8; 16],
    },
    /// Offer admission completes before this reader accepts following data.
    Offer {
        peer: NodeId,
        parent: [u8; 16],
        generation: u64,
        message: ClipboardMessage,
        ready: tokio::sync::oneshot::Sender<Option<IncomingRoute>>,
    },
    /// One message on a bound channel.
    Message {
        /// Which peer.
        peer: NodeId,
        /// Authenticated parent input link, retained through queued events.
        parent: [u8; 16],
        /// Bound clipboard connection generation.
        generation: u64,
        /// What it sent.
        message: ClipboardMessage,
    },
    /// That peer's channel is gone (reset, closed, or refused).
    Down {
        /// Which peer.
        peer: NodeId,
        /// Authenticated parent input link, retained through queued events.
        parent: [u8; 16],
    },
}

/// The core-side clipboard endpoint: the coordinator and the handle to its runtime.
pub struct Clipboard {
    settings: Settings,
    /// The version counter, the echo set and the "never re-offer a received payload" flag.
    source: cb::Source,
    runtime: Handle,
    /// Per member: the input link's connection id (the parent of its channel) and, once it is
    /// bound, the channel.
    peers: BTreeMap<NodeId, Peer>,
    /// Offers waiting for their authorized channel to bind.
    waiting: BTreeMap<NodeId, PendingOffer>,
    /// Offered payloads not started yet: broadcasts wait for Request; addressed deliveries
    /// start after the session has bound their application acknowledgement.
    offered: BTreeMap<NodeId, PendingOffer>,
    /// The offer a peer made and we held back (`transfer = "on_focus_enter"`).
    pending_request: Option<(NodeId, [u8; 16])>,
    /// One validated offer per incoming transfer, keyed by `offer_id`.
    incoming: BTreeMap<[u8; 16], Incoming>,
    reservations: Arc<std::sync::Mutex<BTreeMap<u64, (cb::ContentKind, u64)>>>,
    local_image_support: bool,
    next_verification: u64,
    /// Completed versions scoped to authenticated sender and its Hello epoch.
    received: BTreeMap<NodeId, ReceivedSession>,
    /// Peers whose connection is closed (link down, revoke): frames still queued from them are
    /// dropped. Cleared when the peer's link comes up again.
    down: BTreeSet<NodeId>,
    /// The agent's chunks for the copy it is currently reporting.
    local: Option<LocalAssembly>,
    local_generation: u64,
    local_preparation: LocalPreparation,
    /// Validated local carry; history applies and explicit sends do not replace it.
    payload: Arc<Vec<u8>>,
    carried: Option<(cb::Offer, [u8; 32])>,
    /// Latest outbound wire transfer per peer, retained through its Request/Cancel.
    deliveries: BTreeMap<NodeId, ([u8; 16], u64, [u8; 32])>,
    /// The outbound queue capacity a channel runtime is given.
    queue_capacity: usize,
    /// `network.port_clipboard`: the port this node's own listener is bound to, which every
    /// grant it issues advertises.
    clipboard_port: u16,
    #[cfg(test)]
    test_events: Option<mpsc::Receiver<Inbound>>,
    #[cfg(test)]
    test_assembly_gate: Option<(
        tokio::sync::oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
}

/// One member's clipboard state.
#[derive(Default)]
struct Peer {
    /// The parent input link's connection id.
    parent: Option<[u8; 16]>,
    /// The channel this peer bound.
    channel: Option<[u8; 16]>,
    /// Whether the peer claimed `CAPABILITY_TEXT_CLIPBOARD` in its `Hello`.
    text_clipboard: bool,
    image_clipboard: bool,
    /// The IP the peer's input link comes from — where its clipboard listener is reachable.
    remote_ip: Option<std::net::IpAddr>,
    epoch: Option<[u8; 16]>,
}

#[derive(Default)]
struct ReceivedSession {
    epoch: Option<[u8; 16]>,
    echo: cb::Echo,
}

struct PendingOffer {
    message: ClipboardMessage,
    payload: Arc<Vec<u8>>,
    immediate: bool,
}

/// A validated payload a peer offered.
struct Incoming {
    /// Who is sending it: only that peer may complete it, and it ends with that peer's link.
    peer: NodeId,
    offer: cb::Offer,
    worker: Option<IncomingRoute>,
    job: u64,
    parent: Option<[u8; 16]>,
    epoch: Option<[u8; 16]>,
    deadline: Instant,
}

impl Drop for Incoming {
    fn drop(&mut self) {
        if let Some(route) = &self.worker {
            route.cancel();
        }
    }
}

type VerifiedContent = Result<(Option<String>, Arc<Vec<u8>>, [u8; 32]), cb::ClipboardError>;

/// Internal completion; its reservation survives cancelled/stale jobs.
pub struct Verified {
    job: u64,
    result: VerifiedContent,
    _reservation: IncomingReservation,
}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verified")
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct IncomingReservation {
    job: u64,
    reservations: Arc<std::sync::Mutex<BTreeMap<u64, (cb::ContentKind, u64)>>>,
}

#[cfg(test)]
impl IncomingReservation {
    pub(crate) fn for_test(kind: cb::ContentKind, size: u64) -> Self {
        Self {
            job: 1,
            reservations: Arc::new(std::sync::Mutex::new(BTreeMap::from([(1, (kind, size))]))),
        }
    }
}

impl Drop for IncomingReservation {
    fn drop(&mut self) {
        self.reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.job);
    }
}

/// The agent's `ClipboardChanged` chunks, assembled before anything is decided.
struct LocalAssembly {
    version: u64,
    count: u32,
    next: u32,
    size: u64,
    kind: cb::ContentKind,
    mime: String,
    markers: Vec<String>,
    chunks: Vec<Vec<u8>>,
    received: u64,
}

/// A prepared copy shares immutable transfer bytes; only text has a history representation.
pub struct PreparedCopy {
    payload: Arc<Vec<u8>>,
    text: Option<String>,
    kind: cb::ContentKind,
    sha256: [u8; 32],
    markers: Vec<cb::Marker>,
}

impl std::fmt::Debug for PreparedCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedCopy")
            .field("kind", &self.kind)
            .field("bytes", &self.payload.len())
            .finish()
    }
}

struct LocalPreparation {
    task: Option<tokio::task::JoinHandle<()>>,
    gate: Arc<tokio::sync::Semaphore>,
    kind: Option<cb::ContentKind>,
}

impl LocalPreparation {
    fn new() -> Self {
        Self {
            task: None,
            gate: Arc::new(tokio::sync::Semaphore::new(1)),
            kind: None,
        }
    }

    fn cancel(&mut self) {
        self.kind = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for LocalPreparation {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn prepare_copy(
    chunks: Vec<Vec<u8>>,
    expected_hash: Option<Vec<u8>>,
    markers: Vec<cb::Marker>,
    kind: cb::ContentKind,
) -> Result<PreparedCopy, cb::ClipboardError> {
    let size = chunks.iter().map(Vec::len).sum();
    let mut payload = Vec::with_capacity(size);
    for chunk in chunks {
        payload.extend_from_slice(&chunk);
    }
    let sha256: [u8; 32] = cb::sha256(&payload).try_into().expect("SHA-256 length");
    if expected_hash.is_some_and(|expected| expected.as_slice() != sha256) {
        return Err(cb::ClipboardError::HashMismatch);
    }
    let content = cb::ClipboardContent::from_bytes(kind, payload)?;
    let (text, payload) = retain_content(content);
    Ok(PreparedCopy {
        payload,
        text,
        kind,
        sha256,
        markers,
    })
}

fn retain_content(content: cb::ClipboardContent) -> (Option<String>, Arc<Vec<u8>>) {
    match content {
        cb::ClipboardContent::Text(text) => {
            let payload = Arc::new(text.as_bytes().to_vec());
            (Some(text), payload)
        }
        cb::ClipboardContent::Image { png } => (None, Arc::new(png)),
    }
}

type AssembledCopy = Result<Option<LocalAssembly>, cb::ClipboardError>;

impl Clipboard {
    /// Builds the endpoint and its runtime: the listener on `47902` and the dial list.
    pub async fn start(
        node: NodeId,
        settings: Settings,
        identity: Arc<identity::NodeIdentity>,
        trusted_peers: Arc<std::sync::RwLock<std::collections::BTreeSet<NodeId>>>,
        listen: SocketAddr,
        queue_capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<Inbound>), String> {
        let (runtime, events) = Handle::start(identity, trusted_peers, listen, queue_capacity)
            .await
            .map_err(|error| format!("clipboard channel: {error}"))?;
        let endpoint = Clipboard {
            settings,
            source: cb::Source::new(*node.as_bytes()),
            runtime,
            peers: BTreeMap::new(),
            waiting: BTreeMap::new(),
            offered: BTreeMap::new(),
            pending_request: None,
            incoming: BTreeMap::new(),
            reservations: Arc::default(),
            next_verification: 0,
            local_image_support: false,
            received: BTreeMap::new(),
            down: BTreeSet::new(),
            local: None,
            local_generation: 0,
            local_preparation: LocalPreparation::new(),
            payload: Arc::new(Vec::new()),
            carried: None,
            deliveries: BTreeMap::new(),
            queue_capacity,
            clipboard_port: listen.port(),
            #[cfg(test)]
            test_events: None,
            #[cfg(test)]
            test_assembly_gate: None,
        };
        Ok((endpoint, events))
    }

    /// Opens the channel a grant authorised: the dial is the receiving side's job
    /// (`docs/05-protocol.md`: «Пир устанавливает второй mTLS-канал к тому же узлу»).
    pub async fn dial(
        &self,
        peer: NodeId,
        address: SocketAddr,
        parent: [u8; 16],
        channel_id: [u8; 16],
        token: [u8; 32],
    ) {
        self.runtime
            .dial(
                peer,
                address,
                parent,
                channel_id,
                token,
                self.queue_capacity,
            )
            .await;
    }

    /// Tells the peer why a transfer is off, with the documented `ClipboardCancel` code.
    pub async fn cancel(&self, peer: NodeId, offer_id: [u8; 16], reason: cb::ClipboardError) {
        let _ = self
            .runtime
            .try_send(
                peer,
                ClipboardMessage::Cancel {
                    offer_id,
                    reason: wire_error(reason),
                },
            )
            .await;
    }

    /// Cancels only the wire delivery named by the session.
    pub async fn cancel_delivery(&mut self, peer: NodeId, wire_version: u64, sha256: [u8; 32]) {
        let Some(&(offer_id, version, hash)) = self.deliveries.get(&peer) else {
            return;
        };
        if version != wire_version || hash != sha256 {
            return;
        }
        self.deliveries.remove(&peer);
        for offers in [&mut self.waiting, &mut self.offered] {
            if offers
                .get(&peer)
                .is_some_and(|offer| offer_id_of(&offer.message) == Some(offer_id))
            {
                offers.remove(&peer);
            }
        }
        self.runtime.cancel_transfer(peer, offer_id).await;
        self.cancel(peer, offer_id, cb::ClipboardError::Superseded)
            .await;
    }

    /// Addressing content ends its retained carry without discarding the explicit delivery.
    pub fn address_carried(&mut self, sha256: [u8; 32]) -> bool {
        if self
            .carried
            .as_ref()
            .is_none_or(|(_, hash)| *hash != sha256)
        {
            return false;
        }
        self.carried = None;
        self.payload = Arc::new(Vec::new());
        true
    }

    /// Closes the listener and joins every runtime task without changing the OS clipboard.
    ///
    /// Offered and partially received payloads are discarded. The source is returned so the
    /// session can preserve origin versions and echo suppression across opt-in restarts.
    pub async fn stop(self) -> cb::Source {
        self.runtime.stop().await;
        self.source
    }

    /// Restores the session's source state after restarting the clipboard runtime.
    pub fn restore_source(&mut self, source: cb::Source) {
        self.source = source;
    }

    /// Moves the listener without losing source versions, authenticated replay state or carry.
    /// Bind first so an unavailable replacement address leaves the existing runtime intact.
    pub async fn rebind_listener(
        &mut self,
        listen: SocketAddr,
    ) -> Result<mpsc::Receiver<Inbound>, String> {
        let (replacement, events) = Handle::start(
            Arc::clone(&self.runtime.identity),
            Arc::clone(&self.runtime.trusted_peers),
            listen,
            self.queue_capacity,
        )
        .await?;
        self.runtime.stop().await;
        self.runtime = replacement;
        self.clipboard_port = listen.port();
        self.incoming.clear();
        self.pending_request = None;
        self.waiting.clear();
        self.offered.clear();
        self.deliveries.clear();
        for state in self.peers.values_mut() {
            state.channel = None;
        }
        Ok(events)
    }

    /// Whether clipboard transfers are on, for `Status.clipboard_enabled`.
    pub const fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    fn max_bytes(&self, kind: cb::ContentKind) -> u64 {
        match kind {
            cb::ContentKind::Text => self.settings.policy.max_bytes,
            cb::ContentKind::Png => self.settings.policy.max_image_bytes,
        }
        .min(kind.hard_max_bytes())
    }

    /// Backend support, not configuration; withdrawal leaves text transfers intact.
    pub async fn set_local_image_support(&mut self, supported: bool) -> Vec<Out> {
        self.local_image_support = supported;
        if supported {
            return Vec::new();
        }
        self.revoke_local_images().await
    }

    /// Changes image consent without changing the adapter's advertised capability.
    pub async fn set_images(&mut self, enabled: bool) -> Vec<Out> {
        if self.settings.policy.images == enabled {
            return Vec::new();
        }
        self.settings.policy.images = enabled;
        if enabled {
            Vec::new()
        } else {
            self.revoke_local_images().await
        }
    }

    async fn revoke_local_images(&mut self) -> Vec<Out> {
        if self
            .local
            .as_ref()
            .is_some_and(|a| a.kind == cb::ContentKind::Png)
            || self.local_preparation.kind == Some(cb::ContentKind::Png)
        {
            self.local_generation = self.local_generation.wrapping_add(1);
            self.local = None;
            self.local_preparation.cancel();
        }
        let mut out = self.revoke_images(None).await;
        out.push(Out::RevokeImages);
        if self
            .carried
            .as_ref()
            .is_some_and(|(offer, _)| offer.mime == cb::IMAGE_MIME)
        {
            self.carried = None;
            self.payload = Arc::new(Vec::new());
            out.push(Out::CarryCleared);
        }
        out
    }

    /// Updates capability 15 without replacing the text channel or its replay namespace.
    pub async fn set_peer_image_support(&mut self, peer: NodeId, supported: bool) -> Vec<Out> {
        let state = self.peers.entry(peer).or_default();
        let changed = state.image_clipboard != supported;
        state.image_clipboard = supported;
        if !supported {
            return self.revoke_images(Some(peer)).await;
        }
        if changed && self.settings.routing_policy == config::ClipboardRoutingPolicy::Broadcast {
            if let Some((offer, hash)) = self.carried.as_ref() {
                if offer.mime == cb::IMAGE_MIME {
                    return self
                        .offer_to(peer, offer.clone(), Arc::clone(&self.payload), *hash, false)
                        .await
                        .unwrap_or_default();
                }
            }
        }
        Vec::new()
    }

    async fn revoke_images(&mut self, peer: Option<NodeId>) -> Vec<Out> {
        let mut out = Vec::new();
        self.incoming.retain(|offer_id, incoming| {
            if incoming.offer.mime != cb::IMAGE_MIME || peer.is_some_and(|p| p != incoming.peer) {
                return true;
            }
            if self.pending_request == Some((incoming.peer, *offer_id)) {
                self.pending_request = None;
            }
            out.push(Out::Cancel {
                peer: incoming.peer,
                offer_id: *offer_id,
                reason: cb::ClipboardError::Disabled,
            });
            false
        });
        let destinations: Vec<_> = self
            .deliveries
            .keys()
            .copied()
            .filter(|p| peer.is_none_or(|wanted| wanted == *p))
            .collect();
        for destination in destinations {
            let retained_image = [&self.waiting, &self.offered].iter().any(|offers| {
                offers.get(&destination).is_some_and(|offer| {
                    matches!(&offer.message, ClipboardMessage::Offer { mime, .. } if mime == cb::IMAGE_MIME)
                })
            });
            let active_image = self
                .runtime
                .inner
                .lock()
                .await
                .conns
                .get(&destination)
                .is_some_and(|conn| {
                    conn.transfers
                        .values()
                        .any(|t| t.kind == cb::ContentKind::Png)
                });
            if retained_image || active_image {
                if let Some(&(_, version, sha256)) = self.deliveries.get(&destination) {
                    self.cancel_delivery(destination, version, sha256).await;
                    let _ = self.runtime.events.try_send(Inbound::Failed {
                        peer: destination,
                        version,
                        sha256,
                        detail: "image clipboard capability withdrawn".to_owned(),
                    });
                }
            }
        }
        out
    }

    /// Updates session-owned routing settings without restarting the clipboard runtime.
    pub fn set_routing_policy(&mut self, policy: config::ClipboardRoutingPolicy, settle_ms: u32) {
        if self.settings.routing_policy != policy {
            self.carried = None;
            self.payload = Arc::new(Vec::new());
        }
        self.settings.routing_policy = policy;
        self.settings.settle_ms = settle_ms;
    }

    /// One `ClipboardChanged` chunk from the agent.
    ///
    /// Nothing is decided — and nothing is offered — until the last chunk arrived with a
    /// matching hash and valid UTF-8.
    pub async fn on_agent_frame(&mut self, message: &ipc::ClipboardChanged) -> Vec<Out> {
        let fresh = self
            .local
            .as_ref()
            .is_none_or(|assembly| assembly.version != message.origin_version);
        let mut out = if fresh {
            self.clear_carry().await
        } else {
            Vec::new()
        };
        if !markers_of(&message.markers).is_empty() {
            if message.chunk_index == 0 {
                tracing::info!(
                    code = cb::ClipboardError::Secret.code(),
                    bytes = message.size_bytes,
                    "clipboard copy not offered"
                );
            }
            self.local = None;
            if !fresh {
                out.extend(self.clear_carry().await);
            }
            return out;
        }
        let assembly = match self.take_chunk(message) {
            Ok(Some(assembly)) => assembly,
            Ok(None) => return out,
            Err(error) => {
                tracing::warn!(?error, "clipboard chunks from the agent refused");
                self.local = None;
                out.extend(self.clear_carry().await);
                return out;
            }
        };
        let generation = self.local_generation;
        let expected_hash = message.sha256.clone();
        let gate = Arc::clone(&self.local_preparation.gate);
        let events = self.runtime.events.clone();
        self.local_preparation.kind = Some(assembly.kind);
        self.local_preparation.task = Some(tokio::spawn(async move {
            let Ok(permit) = gate.acquire_owned().await else {
                return;
            };
            let prepared = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                prepare_copy(
                    assembly.chunks,
                    Some(expected_hash),
                    markers_of(&assembly.markers),
                    assembly.kind,
                )
            })
            .await
            .unwrap_or(Err(cb::ClipboardError::Invalid));
            let _ = events
                .send(Inbound::LocalPrepared {
                    generation,
                    prepared,
                })
                .await;
        }));
        out
    }

    /// A stale preparation must never replace a newer local copy or cleared carry.
    pub async fn on_local_prepared(
        &mut self,
        generation: u64,
        prepared: Result<PreparedCopy, cb::ClipboardError>,
    ) -> Vec<Out> {
        if generation != self.local_generation {
            return Vec::new();
        }
        self.local_preparation.kind = None;
        match prepared {
            Ok(prepared) => self.accept_local_copy(prepared).await,
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    ?error,
                    "clipboard local preparation refused"
                );
                Vec::new()
            }
        }
    }

    /// Assembles one chunk.
    ///
    /// `Ok(None)` means more chunks are expected; `Ok(Some(..))` is the last one, with the
    /// payload and the markers the platform reported for the whole copy.
    fn take_chunk(&mut self, message: &ipc::ClipboardChanged) -> AssembledCopy {
        let kind = if message.mime.is_empty() {
            cb::ContentKind::Text
        } else {
            cb::ContentKind::from_mime(&message.mime)?
        };
        if kind == cb::ContentKind::Png
            && (!self.local_image_support || !self.settings.policy.images)
        {
            return Err(cb::ClipboardError::Disabled);
        }
        let count = message.chunk_count;
        let max_count = match kind {
            cb::ContentKind::Text => 64,
            cb::ContentKind::Png => 1024,
        };
        if count == 0
            || count > max_count
            || message.chunk_index >= count
            || message.size_bytes > max_count as u64 * IPC_CHUNK_BYTES as u64
        {
            self.local = None;
            return Err(cb::ClipboardError::Invalid);
        }
        let fresh = match &self.local {
            Some(assembly) => assembly.version != message.origin_version,
            None => true,
        };
        if fresh {
            self.local = Some(LocalAssembly {
                version: message.origin_version,
                count,
                next: 0,
                size: message.size_bytes,
                kind,
                mime: message.mime.clone(),
                markers: message.markers.clone(),
                chunks: Vec::new(),
                received: 0,
            });
        }
        let assembly = self.local.as_mut().expect("inserted above");
        // The assembly is bounded by what the agent *declared*, not by the policy's limit: a
        // copy over the limit has to arrive whole so that the policy can name it
        // (`CLIPBOARD_TOO_LARGE`); IPC ceilings are 2 MiB text / 32 MiB PNG.
        if message.chunk_index != assembly.next
            || kind != assembly.kind
            || message.mime != assembly.mime
            || count != assembly.count
            || (message.chunk_index + 1 < count && !message.sha256.is_empty())
            || (message.chunk_index + 1 == count && message.sha256.len() != 32)
            || message.size_bytes != assembly.size
            || message.markers != assembly.markers
            || message.data.len() > IPC_CHUNK_BYTES
            || message.data.len() as u64 > assembly.size.saturating_sub(assembly.received)
        {
            self.local = None;
            return Err(cb::ClipboardError::Invalid);
        }
        assembly.received += message.data.len() as u64;
        assembly.chunks.push(message.data.clone());
        assembly.next += 1;
        if assembly.next < count {
            return Ok(None);
        }
        let assembly = self.local.take().expect("checked above");
        if assembly.received != assembly.size {
            return Err(cb::ClipboardError::Invalid);
        }
        Ok(Some(assembly))
    }

    #[cfg(test)]
    async fn on_local_copy(&mut self, payload: Vec<u8>, markers: Vec<cb::Marker>) -> Vec<Out> {
        let mut out = self.clear_carry().await;
        let prepared = tokio::task::spawn_blocking(move || {
            prepare_copy(vec![payload], None, markers, cb::ContentKind::Text)
        })
        .await
        .expect("copy preparation");
        if let Ok(prepared) = prepared {
            out.extend(self.accept_local_copy(prepared).await);
        }
        out
    }

    async fn accept_local_copy(&mut self, prepared: PreparedCopy) -> Vec<Out> {
        let PreparedCopy {
            payload,
            text,
            sha256,
            markers,
            kind,
        } = prepared;
        let mut out = Vec::new();
        let refusal = if !self.settings.enabled {
            Some(cb::ClipboardError::Disabled)
        } else if !markers.is_empty() {
            Some(cb::ClipboardError::Secret)
        } else if kind == cb::ContentKind::Png && !self.local_image_support {
            Some(cb::ClipboardError::Disabled)
        } else {
            None
        };
        if let Some(reason) = refusal {
            tracing::info!(
                code = reason.code(),
                bytes = payload.len(),
                "clipboard copy not offered"
            );
            return out;
        }
        let content = cb::Content {
            bytes: payload.len() as u64,
            markers: markers.clone(),
            kind,
        };
        let Ok(offer_id) = identity::random16() else {
            return out;
        };
        let offer =
            match self
                .source
                .local_copy(&content, &markers, &self.settings.policy, offer_id)
            {
                Ok(offer) => offer,
                Err(cb::source::Refusal::Policy(reason)) => {
                    // `AC-15`: a password-manager marker (or the size limit) stops the copy here.
                    tracing::info!(
                        code = reason.code(),
                        bytes = payload.len(),
                        "clipboard copy not offered"
                    );
                    return out;
                }
            };
        tracing::info!(
            bytes = offer.size,
            version = offer.version,
            "clipboard offer"
        );
        if matches!(
            self.settings.routing_policy,
            config::ClipboardRoutingPolicy::Follow
        ) {
            out.push(Out::Carried {
                origin: offer.origin.into(),
                version: offer.version,
                sha256,
            });
        }
        if let Some(text) = text {
            out.push(Out::History {
                peer: offer.origin.into(),
                direction: ipc::ClipboardHistoryDirection::Copied,
                text,
                payload: Arc::clone(&payload),
                sha256,
                secret: false,
            });
        }
        self.payload = payload;
        self.carried = Some((offer.clone(), sha256));
        if matches!(
            self.settings.routing_policy,
            config::ClipboardRoutingPolicy::Broadcast
        ) {
            out.extend(self.fan_out(offer).await);
        }
        out
    }

    async fn clear_carry(&mut self) -> Vec<Out> {
        self.local_generation = self.local_generation.wrapping_add(1);
        self.local_preparation.cancel();
        self.payload = Arc::new(Vec::new());
        self.carried = None;
        for (peer, (offer_id, _, _)) in std::mem::take(&mut self.deliveries) {
            self.runtime.cancel_transfer(peer, offer_id).await;
            self.cancel(peer, offer_id, cb::ClipboardError::Superseded)
                .await;
        }
        self.waiting.clear();
        self.offered.clear();
        // A new local copy wins even while its chunks are still being assembled.
        // «Локальное копирование побеждает»: *any* new local copy — even one the policy then
        // refuses (a password) or a disabled node ignores — supersedes the transfer the wire was
        // about to deliver, otherwise a late `ClipboardComplete` would apply the peer's payload
        // over what the user just copied.
        let mut out: Vec<_> = std::mem::take(&mut self.incoming)
            .into_iter()
            .map(|(offer_id, incoming)| Out::Cancel {
                peer: incoming.peer,
                offer_id,
                reason: cb::ClipboardError::Superseded,
            })
            .collect();
        if let Some((peer, offer_id)) = self.pending_request.take() {
            if !out.iter().any(|event| {
                matches!(event,
                    Out::Cancel { peer: p, offer_id: id, .. } if *p == peer && *id == offer_id
                )
            }) {
                out.push(Out::Cancel {
                    peer,
                    offer_id,
                    reason: cb::ClipboardError::Superseded,
                });
            }
        }
        out.push(Out::CarryCleared);
        out
    }

    /// Delivers the logical carry with a fresh wire version, preserving its routing identity.
    pub async fn deliver_carried(
        &mut self,
        peer: NodeId,
        sha256: [u8; 32],
    ) -> Result<Vec<Out>, String> {
        // The carried MIME selects the destination capability.
        let (carried, hash) = self.carried.as_ref().ok_or("no carried clipboard")?;
        if *hash != sha256 {
            return Err("carried clipboard changed".to_owned());
        }
        let kind = cb::ContentKind::from_mime(&carried.mime).map_err(|e| e.code().to_owned())?;
        self.require_peer(peer, kind)?;
        let offer = self.mint_delivery_offer(self.payload.len(), kind)?;
        self.offer_to(peer, offer, self.payload.clone(), sha256, true)
            .await
    }

    /// Sends an already prepared history item without copying or hashing it.
    pub async fn send_prepared_to(
        &mut self,
        peer: NodeId,
        payload: Arc<Vec<u8>>,
        hash: [u8; 32],
    ) -> Result<Vec<Out>, String> {
        self.require_peer(peer, cb::ContentKind::Text)?;
        let offer = self.mint_delivery_offer(payload.len(), cb::ContentKind::Text)?;
        self.offer_to(peer, offer, payload, hash, true).await
    }

    #[cfg(test)]
    async fn send_text_to(&mut self, peer: NodeId, text: &str) -> Result<Vec<Out>, String> {
        let text = text.to_owned();
        let (payload, hash) = tokio::task::spawn_blocking(move || {
            let hash = cb::sha256(text.as_bytes())
                .try_into()
                .expect("SHA-256 length");
            (Arc::new(text.into_bytes()), hash)
        })
        .await
        .expect("test preparation");
        self.send_prepared_to(peer, payload, hash).await
    }

    fn mint_delivery_offer(
        &mut self,
        bytes: usize,
        kind: cb::ContentKind,
    ) -> Result<cb::Offer, String> {
        let offer_id = identity::random16().map_err(|error| error.to_string())?;
        let from_peer = self.source.applied_from_peer();
        let offer = self
            .source
            .local_copy(
                &cb::Content {
                    bytes: bytes as u64,
                    markers: Vec::new(),
                    kind,
                },
                &[],
                &self.settings.policy,
                offer_id,
            )
            .map_err(|error| format!("clipboard send refused: {error:?}"))?;
        if from_peer {
            self.source.apply_from_peer(&offer);
        }
        Ok(offer)
    }

    fn require_peer(&self, peer: NodeId, kind: cb::ContentKind) -> Result<(), String> {
        if !self.settings.enabled {
            return Err("clipboard is disabled".to_owned());
        }
        if self.peers.get(&peer).is_none_or(|state| {
            state.parent.is_none()
                || match kind {
                    cb::ContentKind::Text => !state.text_clipboard,
                    cb::ContentKind::Png => !state.image_clipboard,
                }
        }) {
            return Err("peer has no live clipboard link for this content kind".to_owned());
        }
        Ok(())
    }

    async fn offer_to(
        &mut self,
        peer: NodeId,
        offer: cb::Offer,
        payload: Arc<Vec<u8>>,
        sha256: [u8; 32],
        immediate: bool,
    ) -> Result<Vec<Out>, String> {
        let kind = cb::ContentKind::from_mime(&offer.mime).map_err(|e| e.code().to_owned())?;
        self.require_peer(peer, kind)?;
        if kind == cb::ContentKind::Png
            && (!self.peers[&peer].image_clipboard
                || !self.local_image_support
                || !self.settings.policy.images)
        {
            return Err("image clipboard is unavailable".to_owned());
        }
        let relayed = offer.origin != self.source.origin;
        if relayed && self.peers[&peer].epoch.is_none() {
            return Err("peer does not support authenticated clipboard relays".to_owned());
        }
        if let Some(&(_, version, hash)) = self.deliveries.get(&peer) {
            self.cancel_delivery(peer, version, hash).await;
        }
        let state = &self.peers[&peer];
        let message = ClipboardMessage::Offer {
            offer_id: offer.offer_id,
            origin_node_id: offer.origin,
            origin_version: offer.version,
            mime: offer.mime,
            size_bytes: offer.size,
            secret_marked: false,
            sender_epoch: state
                .epoch
                .map(|_| transport::clipboard::process_epoch().to_vec())
                .unwrap_or_default(),
            relayed,
        };
        let mut out =
            Vec::with_capacity(usize::from(immediate) + usize::from(state.channel.is_none()));
        if immediate {
            out.push(Out::Delivery {
                peer,
                version: offer.version,
                sha256,
            });
        }
        self.deliveries
            .insert(peer, (offer.offer_id, offer.version, sha256));
        self.waiting.remove(&peer);
        self.offered.remove(&peer);
        if state.channel.is_some() {
            self.offered.insert(
                peer,
                PendingOffer {
                    message: message.clone(),
                    payload,
                    immediate,
                },
            );
            if self.runtime.send(peer, message).await.is_err() {
                self.offered.remove(&peer);
                self.deliveries.remove(&peer);
                return Err("clipboard channel is unavailable".to_owned());
            }
        } else {
            let parent = state.parent.expect("live peer checked above");
            let (channel_id, token) = self.runtime.issue(parent).await;
            tracing::info!(peer = %peer, "clipboard grant");
            out.push(Out::Grant {
                peer,
                message: ClipboardMessage::Grant {
                    channel_id,
                    binding_token: token,
                    ttl_ms: cb::CHANNEL_TTL_MS as u32,
                    clipboard_port: u32::from(self.clipboard_port),
                },
            });
            self.waiting.insert(
                peer,
                PendingOffer {
                    message,
                    payload,
                    immediate,
                },
            );
        }
        Ok(out)
    }

    async fn send_offered(&mut self, peer: NodeId, offer_id: [u8; 16]) {
        let Entry::Occupied(entry) = self.offered.entry(peer) else {
            return;
        };
        if offer_id_of(&entry.get().message) != Some(offer_id) {
            return;
        }
        let PendingOffer {
            message: offer,
            payload,
            ..
        } = entry.remove();
        let ClipboardMessage::Offer {
            origin_version,
            secret_marked: secret,
            mime,
            ..
        } = offer
        else {
            unreachable!("only offers are retained");
        };
        let Some(sha256) = self
            .deliveries
            .get(&peer)
            .filter(|(id, _, _)| *id == offer_id)
            .map(|(_, _, hash)| *hash)
        else {
            return;
        };
        let Ok(kind) = cb::ContentKind::from_mime(&mime) else {
            return;
        };
        if kind == cb::ContentKind::Png && !self.peers.get(&peer).is_some_and(|p| p.image_clipboard)
        {
            return;
        }
        let rate = match kind {
            cb::ContentKind::Text => self.settings.rate_bytes_per_s,
            cb::ContentKind::Png => self.settings.image_rate_bytes_per_s,
        };
        let result = self
            .runtime
            .send_transfer(
                peer,
                offer_id,
                (origin_version, sha256, kind),
                payload,
                rate,
                secret,
            )
            .await;
        if result.is_err() {
            let _ = self
                .runtime
                .events
                .send(Inbound::Failed {
                    peer,
                    version: origin_version,
                    sha256,
                    detail: "clipboard channel is unavailable".to_owned(),
                })
                .await;
        }
    }

    /// Offers a local copy to every live clipboard member in broadcast mode.
    async fn fan_out(&mut self, offer: cb::Offer) -> Vec<Out> {
        let peers: Vec<_> = self.peers.keys().copied().collect();
        let hash = self
            .carried
            .as_ref()
            .expect("broadcast carries a local copy")
            .1;
        let mut out = Vec::new();
        for peer in peers {
            if self
                .require_peer(
                    peer,
                    cb::ContentKind::from_mime(&offer.mime).expect("local MIME"),
                )
                .is_ok()
            {
                if let Ok(events) = self
                    .offer_to(peer, offer.clone(), self.payload.clone(), hash, false)
                    .await
                {
                    out.extend(events);
                }
            }
        }
        out
    }

    /// A peer's channel is bound: deliver what was waiting for it, and start the transfer.
    pub async fn on_bound(&mut self, peer: NodeId, channel_id: [u8; 16]) -> Vec<Out> {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.channel = Some(channel_id);
        }
        if let Some(offer) = self.waiting.remove(&peer) {
            if matches!(&offer.message, ClipboardMessage::Offer { mime, .. } if mime == cb::ContentKind::Png.mime())
                && !self.peers.get(&peer).is_some_and(|p| p.image_clipboard)
            {
                return Vec::new();
            }
            if self
                .runtime
                .send(peer, offer.message.clone())
                .await
                .is_err()
            {
                if let Some((_, version, sha256)) = self.deliveries.remove(&peer) {
                    let _ = self
                        .runtime
                        .events
                        .send(Inbound::Failed {
                            peer,
                            version,
                            sha256,
                            detail: "clipboard channel is unavailable".to_owned(),
                        })
                        .await;
                }
                return Vec::new();
            }
            let offer_id = offer_id_of(&offer.message).expect("only offers are retained");
            let immediate = offer.immediate;
            self.offered.insert(peer, offer);
            if immediate {
                self.send_offered(peer, offer_id).await;
            }
        }
        Vec::new()
    }

    /// A grant arrived on the input link: open the channel it authorises.
    pub fn on_grant(
        &mut self,
        peer: NodeId,
        parent: [u8; 16],
        message: &ClipboardMessage,
    ) -> Vec<Out> {
        let ClipboardMessage::Grant {
            channel_id,
            binding_token,
            clipboard_port: advertised,
            ..
        } = message
        else {
            return Vec::new();
        };
        let (Ok(channel_id), Ok(token)) = (
            <[u8; 16]>::try_from(channel_id.as_slice()),
            <[u8; 32]>::try_from(binding_token.as_slice()),
        ) else {
            return Vec::new();
        };
        if !self.settings.enabled {
            // A disabled node does not open the channel either: nothing would be applied.
            return Vec::new();
        }
        // The port is the granting node's, so a workspace that moved it off the default still
        // works; the IP is the one its input link comes from, which is the only address this
        // node knows is live (`docs/05-protocol.md`, «Clipboard канал»).
        let port = u16::try_from(*advertised).unwrap_or(0);
        let port = if port == 0 {
            protocol::limits::PORT_CLIPBOARD
        } else {
            port
        };
        let Some(ip) = self.peers.get(&peer).and_then(|peer| peer.remote_ip) else {
            tracing::warn!(peer = %peer, "no address for the clipboard dial: the input link is gone");
            return Vec::new();
        };
        vec![Out::Dial {
            peer,
            address: SocketAddr::new(ip, port),
            parent,
            channel_id,
            token,
        }]
    }

    /// Reclaims expired reservations before returning cancellations to the session.
    /// Never waits for the runtime queue: a saturated channel cannot prevent reclamation.
    pub fn expire_incoming(&mut self, now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        self.incoming.retain(|offer_id, incoming| {
            if now < incoming.deadline {
                return true;
            }
            if self.pending_request == Some((incoming.peer, *offer_id)) {
                self.pending_request = None;
            }
            out.push(Out::Cancel {
                peer: incoming.peer,
                offer_id: *offer_id,
                reason: cb::ClipboardError::Superseded,
            });
            false
        });
        out
    }

    /// Revalidates an off-loop result against the live authenticated session.
    pub fn on_verified(&mut self, verified: Verified) -> Vec<Out> {
        let Some(offer_id) = self
            .incoming
            .iter()
            .find_map(|(id, incoming)| (incoming.job == verified.job).then_some(*id))
        else {
            return Vec::new();
        };
        let incoming = self.incoming.remove(&offer_id).expect("located above");
        let peer = incoming.peer;
        if self.pending_request == Some((peer, offer_id)) {
            self.pending_request = None;
        }
        let current = self.peers.get(&peer);
        let kind = cb::ContentKind::from_mime(&incoming.offer.mime).expect("accepted MIME");
        if self.down.contains(&peer)
            || !self.settings.enabled
            || incoming
                .worker
                .as_ref()
                .is_some_and(|route| route.bytes.is_closed())
            || current.and_then(|state| state.parent) != incoming.parent
            || current.and_then(|state| state.epoch) != incoming.epoch
            || current.is_some_and(|state| match kind {
                cb::ContentKind::Text => !state.text_clipboard,
                cb::ContentKind::Png => !state.image_clipboard,
            })
            || Instant::now() >= incoming.deadline
            || incoming.offer.size > self.max_bytes(kind)
            || (kind == cb::ContentKind::Png
                && (!self.local_image_support
                    || !self.settings.policy.images
                    || !current.is_some_and(|p| p.image_clipboard)))
            || self.received.get(&peer).is_some_and(|session| {
                session
                    .echo
                    .judge(&incoming.offer.origin, incoming.offer.version)
                    != cb::EchoDecision::Accept
            })
        {
            return Vec::new();
        }
        let (text, payload, sha256) = match verified.result {
            Ok(prepared) => prepared,
            Err(reason) => {
                return vec![Out::Cancel {
                    peer,
                    offer_id,
                    reason,
                }]
            }
        };
        self.received
            .entry(peer)
            .or_default()
            .echo
            .accept(&incoming.offer.origin, incoming.offer.version);
        self.source.apply_from_peer(&incoming.offer);
        tracing::info!(
            peer = %peer,
            bytes = payload.len(),
            version = incoming.offer.version,
            "clipboard payload applied"
        );
        if self.settings.routing_policy == config::ClipboardRoutingPolicy::Broadcast {
            self.carried = None;
            self.payload = Arc::new(Vec::new());
        }
        let mut out = vec![
            Out::ApplyPending {
                peer,
                version: incoming.offer.version,
                sha256,
                kind,
            },
            Out::AgentPayload {
                version: incoming.offer.version,
                payload: Arc::clone(&payload),
                sha256,
                kind,
                reservation: Some(Arc::new(verified._reservation)),
            },
        ];
        if let Some(text) = text {
            out.push(Out::History {
                peer,
                direction: ipc::ClipboardHistoryDirection::Received,
                text,
                payload,
                sha256,
                secret: false,
            });
        }
        out
    }

    /// One message from a peer on the clipboard channel.
    async fn on_peer_message(&mut self, peer: NodeId, message: ClipboardMessage) -> Vec<Out> {
        if self.down.contains(&peer) {
            // Frames of a connection that is already closed (a revoked peer's last, queued ones).
            return Vec::new();
        }
        let expired = self.expire_incoming(Instant::now());
        let mut out = self.on_live_peer_message(peer, message).await;
        out.extend(expired);
        out
    }

    /// Completes policy admission before the reader accepts fast-following data.
    pub async fn on_network_offer(
        &mut self,
        peer: NodeId,
        parent: [u8; 16],
        generation: u64,
        message: ClipboardMessage,
        ready: tokio::sync::oneshot::Sender<Option<IncomingRoute>>,
    ) -> Vec<Out> {
        if ready.is_closed() {
            return Vec::new();
        }
        if !self
            .network_generation_is_current(peer, parent, generation)
            .await
        {
            return Vec::new();
        }
        let id = offer_id_of(&message);
        let previous_job = self.next_verification;
        let out = self.on_peer_message_from(peer, parent, message).await;
        let route = id
            .and_then(|id| self.incoming.get(&id))
            .filter(|incoming| {
                incoming.job > previous_job
                    && incoming.peer == peer
                    && incoming.parent == Some(parent)
            })
            .and_then(|incoming| incoming.worker.clone());
        if ready.send(route).is_err() {
            if let Some(id) =
                id.filter(|id| self.incoming.get(id).is_some_and(|i| i.job > previous_job))
            {
                self.incoming.remove(&id);
                if self.pending_request == Some((peer, id)) {
                    self.pending_request = None;
                }
            }
        }
        out
    }

    async fn network_generation_is_current(
        &self,
        peer: NodeId,
        parent: [u8; 16],
        generation: u64,
    ) -> bool {
        self.runtime
            .inner
            .lock()
            .await
            .conns
            .get(&peer)
            .is_some_and(|conn| conn.parent == parent && conn.generation == generation)
    }

    /// Drops control frames queued by a replaced clipboard connection.
    pub async fn on_network_message(
        &mut self,
        peer: NodeId,
        parent: [u8; 16],
        generation: u64,
        message: ClipboardMessage,
    ) -> Vec<Out> {
        if !self
            .network_generation_is_current(peer, parent, generation)
            .await
        {
            return Vec::new();
        }
        self.on_peer_message_from(peer, parent, message).await
    }

    /// Rejects queued frames from a replaced authenticated parent, including Complete.
    pub async fn on_peer_message_from(
        &mut self,
        peer: NodeId,
        parent: [u8; 16],
        message: ClipboardMessage,
    ) -> Vec<Out> {
        if self.peers.get(&peer).and_then(|state| state.parent) != Some(parent) {
            return Vec::new();
        }
        self.on_peer_message(peer, message).await
    }

    async fn on_live_peer_message(&mut self, peer: NodeId, message: ClipboardMessage) -> Vec<Out> {
        match message {
            offer @ ClipboardMessage::Offer { .. } => self.on_offer(peer, offer).await,
            ClipboardMessage::Request { offer_id } => {
                self.send_offered(peer, offer_id).await;
                Vec::new()
            }
            message @ (ClipboardMessage::Data { .. } | ClipboardMessage::Complete { .. }) => {
                // Production payload frames never enter the session event queue.
                #[cfg(test)]
                {
                    let id = match &message {
                        ClipboardMessage::Data { offer_id, .. }
                        | ClipboardMessage::Complete { offer_id, .. } => *offer_id,
                        _ => unreachable!(),
                    };
                    let route = self
                        .incoming
                        .get(&id)
                        .filter(|i| i.peer == peer)
                        .and_then(|i| i.worker.clone());
                    if let Some(route) = route {
                        if route.send(message).await.is_err() {
                            self.incoming.remove(&id);
                            return vec![Out::Cancel {
                                peer,
                                offer_id: id,
                                reason: cb::ClipboardError::Invalid,
                            }];
                        }
                    }
                }
                #[cfg(not(test))]
                let _ = message;
                Vec::new()
            }
            ClipboardMessage::Cancel { offer_id, reason } => {
                tracing::info!(peer = %peer, reason = ?reason, "clipboard transfer cancelled by the peer");
                if self
                    .incoming
                    .get(&offer_id)
                    .is_some_and(|incoming| incoming.peer == peer)
                {
                    self.incoming.remove(&offer_id);
                }
                if self.pending_request == Some((peer, offer_id)) {
                    self.pending_request = None;
                }
                for offers in [&mut self.offered, &mut self.waiting] {
                    if offers
                        .get(&peer)
                        .is_some_and(|offer| offer_id_of(&offer.message) == Some(offer_id))
                    {
                        offers.remove(&peer);
                    }
                }
                self.runtime.cancel_transfer(peer, offer_id).await;
                if self
                    .deliveries
                    .get(&peer)
                    .is_some_and(|(id, _, _)| *id == offer_id)
                {
                    let (_, version, sha256) = self.deliveries.remove(&peer).expect("matched");
                    let _ = self
                        .runtime
                        .events
                        .send(Inbound::Failed {
                            peer,
                            version,
                            sha256,
                            detail: format!("clipboard refused: {reason:?}"),
                        })
                        .await;
                }
                Vec::new()
            }
            ClipboardMessage::Grant { .. } | ClipboardMessage::Bind { .. } => Vec::new(),
        }
    }

    /// An offer arrived: judge it against the policy and the echo set, then ask for it.
    async fn on_offer(&mut self, peer: NodeId, message: ClipboardMessage) -> Vec<Out> {
        let ClipboardMessage::Offer {
            offer_id,
            origin_node_id,
            origin_version,
            mime,
            size_bytes,
            secret_marked: secret,
            sender_epoch,
            relayed,
        } = message
        else {
            unreachable!("only Offer is dispatched to on_offer");
        };
        if !self.settings.enabled {
            // `CLIPBOARD_DISABLED`: the receiver is off, so nothing is asked for or applied.
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Disabled,
            }];
        }
        if secret {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Secret,
            }];
        }
        let kind = match cb::ContentKind::from_mime(&mime) {
            Ok(kind) => kind,
            Err(reason) => {
                return vec![Out::Cancel {
                    peer,
                    offer_id,
                    reason,
                }]
            }
        };
        if kind == cb::ContentKind::Text
            && self
                .peers
                .get(&peer)
                .is_some_and(|state| !state.text_clipboard)
        {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Disabled,
            }];
        }
        if kind == cb::ContentKind::Png
            && (!self.local_image_support
                || !self.settings.policy.images
                || !self.peers.get(&peer).is_some_and(|p| p.image_clipboard))
        {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Disabled,
            }];
        }
        let Ok(origin) = <[u8; 32]>::try_from(origin_node_id.as_slice()) else {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        };
        let expected_epoch = self.peers.get(&peer).and_then(|state| state.epoch);
        if sender_epoch.as_slice() != expected_epoch.as_ref().map_or(&[][..], |epoch| &epoch[..])
            || (origin != *peer.as_bytes() && (!relayed || expected_epoch.is_none()))
            || (relayed && expected_epoch.is_none())
        {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        }
        // Relays have their own authenticated sender namespace; never advance origin's
        // direct-channel mark. Their claimed origin must remain pairwise trusted locally.
        if relayed
            && origin != self.source.origin
            && !self
                .runtime
                .trusted_peers
                .read()
                .is_ok_and(|pins| pins.contains(&NodeId::from(origin)))
        {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        }
        let offer = cb::Offer {
            offer_id,
            origin,
            version: origin_version,
            mime,
            size: size_bytes,
        };
        if self.received.get(&peer).is_some_and(|session| {
            session.echo.judge(&origin, origin_version) != cb::EchoDecision::Accept
        }) {
            return Vec::new();
        }
        if let Err(error) = offer.is_acceptable().and({
            if offer.size > self.max_bytes(kind) {
                Err(cb::ClipboardError::TooLarge)
            } else {
                Ok(())
            }
        }) {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: error,
            }];
        }
        let job = {
            let mut reservations = self
                .reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let reserved: u64 = reservations
                .values()
                .filter(|(k, _)| *k == kind)
                .map(|(_, n)| n)
                .sum();
            let image_count = reservations
                .values()
                .filter(|(k, _)| *k == cb::ContentKind::Png)
                .count();
            let budget = match kind {
                cb::ContentKind::Text => cb::MAX_INCOMING_TEXT_BYTES,
                cb::ContentKind::Png => cb::MAX_INCOMING_IMAGE_BYTES,
            };
            if self.incoming.contains_key(&offer_id)
                || reservations.len() >= cb::MAX_INCOMING_OFFERS
                || (kind == cb::ContentKind::Png && image_count >= cb::MAX_INCOMING_IMAGE_OFFERS)
                || size_bytes > budget.saturating_sub(reserved)
            {
                return vec![Out::Cancel {
                    peer,
                    offer_id,
                    reason: cb::ClipboardError::TooLarge,
                }];
            }
            self.next_verification += 1;
            let job = self.next_verification;
            reservations.insert(job, (kind, size_bytes));
            job
        };
        let reservation = IncomingReservation {
            job,
            reservations: Arc::clone(&self.reservations),
        };
        let (worker, mut chunks) = IncomingRoute::new(offer_id, size_bytes);
        let worker_bytes = Arc::clone(&worker.bytes);
        let events = self.runtime.events.clone();
        #[cfg(test)]
        let assembly_gate = self.test_assembly_gate.take();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some((started, release)) = assembly_gate {
                let _ = started.send(());
                let _ = release.recv();
            }
            let result = (|| {
                if worker_bytes.is_closed() {
                    return Err(cb::ClipboardError::Superseded);
                }
                let mut assembler =
                    cb::Assembler::new(offer_id, size_bytes, kind).map_err(|e| e.error())?;
                while let Some(chunk) = chunks.blocking_recv() {
                    if worker_bytes.is_closed() {
                        return Err(cb::ClipboardError::Superseded);
                    }
                    match chunk.message {
                        ClipboardMessage::Data {
                            offer_id,
                            chunk_index,
                            chunk_count,
                            data,
                        } => {
                            assembler
                                .push(&offer_id, chunk_index, chunk_count, &data)
                                .map_err(|e| e.error())?;
                        }
                        ClipboardMessage::Complete { sha256, .. } => {
                            let digest = sha256
                                .as_slice()
                                .try_into()
                                .map_err(|_| cb::ClipboardError::Invalid)?;
                            let content = assembler.finish(&sha256).map_err(|e| e.error())?;
                            let (text, payload) = retain_content(content);
                            return Ok((text, payload, digest));
                        }
                        _ => return Err(cb::ClipboardError::Invalid),
                    }
                }
                Err(cb::ClipboardError::Superseded)
            })();
            let _ = events.blocking_send(Inbound::Verified(Verified {
                job,
                result,
                _reservation: reservation,
            }));
        });
        self.incoming.insert(
            offer_id,
            Incoming {
                peer,
                offer,
                worker: Some(worker),
                job,
                parent: self.peers.get(&peer).and_then(|state| state.parent),
                epoch: expected_epoch,
                deadline: Instant::now() + cb::INCOMING_TTL,
            },
        );
        if self.settings.eager {
            if self
                .runtime
                .try_send(peer, ClipboardMessage::Request { offer_id })
                .await
                .is_err()
            {
                self.incoming.remove(&offer_id);
                return vec![Out::Cancel {
                    peer,
                    offer_id,
                    reason: cb::ClipboardError::Superseded,
                }];
            }
        } else {
            // `transfer = "on_focus_enter"`: hold it until this node is the one being typed
            // into, which is what `on_focus` is for.
            self.pending_request = Some((peer, offer_id));
        }
        Vec::new()
    }

    /// Starts an addressed transfer after the session binds its ACK and notifies any owner.
    /// Data follows Offer directly: legacy on-focus receivers already accept that sequence.
    pub async fn start_delivery(&mut self, peer: NodeId, version: u64, hash: [u8; 32]) {
        if !self.offered.get(&peer).is_some_and(|offer| offer.immediate) {
            return;
        }
        if let Some(&(offer_id, current_version, current_hash)) = self.deliveries.get(&peer) {
            if version == current_version && hash == current_hash {
                self.send_offered(peer, offer_id).await;
            }
        }
    }

    /// Ask the peer for the offer that was held back, now that focus is here.
    ///
    /// The policy is `[clipboard].transfer`; the caller decides when focus arrived (the
    /// session asks while this node is the receiver of an active lease).
    pub async fn on_focus(&mut self) {
        let Some((peer, offer_id)) = self.pending_request.take() else {
            return;
        };
        if !self.incoming.contains_key(&offer_id) {
            return;
        }
        tracing::info!(peer = %peer, "clipboard request on focus");
        if self
            .runtime
            .try_send(peer, ClipboardMessage::Request { offer_id })
            .await
            .is_err()
        {
            self.incoming.remove(&offer_id);
        }
    }

    /// Applies history locally without minting a network offer, even when sharing is disabled.
    pub(crate) fn history_to_agent(&mut self, payload: Arc<Vec<u8>>, sha256: [u8; 32]) -> Vec<Out> {
        if self.settings.routing_policy == config::ClipboardRoutingPolicy::Broadcast {
            self.carried = None;
            self.payload = Arc::new(Vec::new());
        }
        self.pending_request = None;
        let mut out: Vec<_> = std::mem::take(&mut self.incoming)
            .into_iter()
            .map(|(offer_id, incoming)| Out::Cancel {
                peer: incoming.peer,
                offer_id,
                reason: cb::ClipboardError::Superseded,
            })
            .collect();
        out.extend(history_to_agent(&mut self.source, payload, sha256));
        out
    }

    /// Learns the epoch only from authenticated input Hello, never from an offer.
    pub fn set_peer_session(&mut self, peer: NodeId, parent: [u8; 16], epoch: Option<[u8; 16]>) {
        let state = self.peers.entry(peer).or_default();
        if state.parent != Some(parent) || state.epoch != epoch {
            self.incoming.retain(|_, incoming| incoming.peer != peer);
            if self
                .pending_request
                .is_some_and(|(pending, _)| pending == peer)
            {
                self.pending_request = None;
            }
            state.channel = None;
            state.image_clipboard = false;
        }
        state.parent = Some(parent);
        state.epoch = epoch;
        let previous = self.received.entry(peer).or_default();
        if epoch.is_some() && previous.epoch != epoch {
            *previous = ReceivedSession {
                epoch,
                echo: cb::Echo::new(),
            };
        }
    }

    /// Remembers a live member; only broadcast mode offers the carry to late joiners.
    pub async fn on_link_up(
        &mut self,
        peer: NodeId,
        connection_id: [u8; 16],
        text_clipboard: bool,
        remote_ip: std::net::IpAddr,
    ) -> Vec<Out> {
        self.down.remove(&peer);
        let state = self.peers.entry(peer).or_default();
        if state.parent != Some(connection_id) {
            state.image_clipboard = false;
        }
        state.parent = Some(connection_id);
        state.text_clipboard = text_clipboard;
        state.remote_ip = Some(remote_ip);
        if !matches!(
            self.settings.routing_policy,
            config::ClipboardRoutingPolicy::Broadcast
        ) {
            return Vec::new();
        }
        let Some((offer, hash)) = self.carried.as_ref() else {
            return Vec::new();
        };
        self.offer_to(peer, offer.clone(), self.payload.clone(), *hash, false)
            .await
            .unwrap_or_default()
    }

    /// A member's input link went down: its clipboard channel goes with it.
    pub async fn on_link_down(&mut self, peer: NodeId) {
        self.peers.remove(&peer);
        // A TCP reconnect is not a process restart. Without an authenticated epoch,
        // legacy peers retain replay protection but cannot signal a counter restart.
        self.waiting.remove(&peer);
        self.offered.remove(&peer);
        self.deliveries.remove(&peer);
        // What the peer was sending is not delivered after it left (a revoke), even when its
        // last frames are already queued behind this call.
        self.incoming.retain(|_, incoming| incoming.peer != peer);
        self.down.insert(peer);
        if matches!(self.pending_request, Some((pending, _)) if pending == peer) {
            self.pending_request = None;
        }
        self.runtime.close(peer).await;
    }

    /// A member's clipboard channel died: keep the parent link, drop the bind.
    pub async fn on_channel_down(&mut self, peer: NodeId) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.channel = None;
        }
        self.incoming.retain(|_, incoming| incoming.peer != peer);
        if self
            .pending_request
            .is_some_and(|(pending, _)| pending == peer)
        {
            self.pending_request = None;
        }
        if let Some(&(_, version, sha256)) = self.deliveries.get(&peer) {
            self.cancel_delivery(peer, version, sha256).await;
        }
        self.waiting.remove(&peer);
        self.offered.remove(&peer);
    }
}

/// Queues prepared history for the local agent without offering it to any peer.
pub(crate) fn history_to_agent(
    source: &mut cb::Source,
    payload: Arc<Vec<u8>>,
    sha256: [u8; 32],
) -> Vec<Out> {
    let offer = source
        .local_copy(
            &cb::Content::text(payload.len() as u64),
            &[],
            &cb::Policy {
                enabled: true,
                skip_secret_marked: true,
                max_bytes: cb::MAX_BYTES,
                images: false,
                max_image_bytes: 0,
            },
            [0; 16],
        )
        .expect("history text is bounded by the clipboard payload limit");
    source.apply_from_peer(&offer);
    vec![Out::AgentPayload {
        version: offer.version,
        payload,
        sha256,
        kind: cb::ContentKind::Text,
        reservation: None,
    }]
}

/// The proto's marker names as [`cb::Marker`].
fn markers_of(names: &[String]) -> Vec<cb::Marker> {
    names
        .iter()
        .filter_map(|name| match name.as_str() {
            "exclude_from_monitor" => Some(cb::Marker::ExcludeFromMonitor),
            "no_history" => Some(cb::Marker::NoHistory),
            "viewer_ignore" => Some(cb::Marker::ViewerIgnore),
            "kde_secret" => Some(cb::Marker::KdeSecret),
            "portal_secret" => Some(cb::Marker::PortalSecret),
            _ => None,
        })
        .collect()
}

/// The wire code a refusal carries (`docs/05-protocol.md`, «Ошибки и завершение»).
///
/// The contract gives a code to the size limit and to the opt-in; a malformed or unverifiable
/// transfer is `INVALID_FRAME`, which is what the receiver's `ClipboardCancel` means.
fn wire_error(reason: cb::ClipboardError) -> protocol::wire::ErrorCode {
    use protocol::wire::ErrorCode;
    match reason {
        cb::ClipboardError::Disabled => ErrorCode::ClipboardDisabled,
        cb::ClipboardError::TooLarge => ErrorCode::ClipboardTooLarge,
        cb::ClipboardError::Superseded => ErrorCode::ClipboardSuperseded,
        cb::ClipboardError::HashMismatch
        | cb::ClipboardError::Invalid
        | cb::ClipboardError::Secret => ErrorCode::InvalidFrame,
    }
}

/// The `offer_id` of an `Offer`, for matching a `Request` to what was offered.
fn offer_id_of(message: &ClipboardMessage) -> Option<[u8; 16]> {
    match message {
        ClipboardMessage::Offer { offer_id, .. } => Some(*offer_id),
        _ => None,
    }
}

/// Every marker name a platform adapter can report, for the tests and the docs.
#[cfg(test)]
const MARKER_NAMES: [&str; 5] = [
    "exclude_from_monitor",
    "no_history",
    "viewer_ignore",
    "kde_secret",
    "portal_secret",
];

/// The clipboard runtime: the listener on `47902`, the dialer, and one task per channel.
///
/// `docs/05-protocol.md` puts the dial on the *receiving* side («Пир устанавливает второй
/// mTLS-канал к тому же узлу»), so a node that holds a payload only issues the grant and waits;
/// the peer calls back. One connection serves the pair.
// Only the endpoint owns the task registry. Worker clones hold a weak reference, so dropping
// the endpoint aborts its tasks instead of leaving an Arc cycle around the listener.
struct Handle {
    inner: Arc<Mutex<Inner>>,
    identity: Arc<identity::NodeIdentity>,
    trusted_peers: Arc<std::sync::RwLock<std::collections::BTreeSet<NodeId>>>,
    events: mpsc::Sender<Inbound>,
    epoch: Instant,
    /// The listener's address, for the tests that dial in from a real peer.
    #[cfg(test)]
    listen_address: SocketAddr,
    tasks: std::sync::Weak<TaskRegistry>,
    _owner: Option<Arc<TaskRegistry>>,
}

#[derive(Default)]
struct TaskRegistry {
    state: std::sync::Mutex<TaskState>,
}

#[derive(Default)]
struct TaskState {
    closed: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    acceptor: Option<tokio::task::JoinHandle<()>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl TaskState {
    fn prune(&mut self) {
        if self.tasks.len() < 64 {
            return;
        }
        // Finished tasks own no running work; retain every live handle for shutdown to join.
        self.tasks.retain(|task| !task.is_finished());
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        let state = self.state.get_mut().expect("task registry");
        if let Some(acceptor) = &state.acceptor {
            acceptor.abort();
        }
        for task in &state.tasks {
            task.abort();
        }
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            identity: Arc::clone(&self.identity),
            trusted_peers: Arc::clone(&self.trusted_peers),
            events: self.events.clone(),
            epoch: self.epoch,
            #[cfg(test)]
            listen_address: self.listen_address,
            tasks: self.tasks.clone(),
            _owner: None,
        }
    }
}

struct Inner {
    /// The one-use binding tokens.
    grants: Grants,
    /// The bound connection of each peer: its sender, and its reader task so a revocation can
    /// close the connection (dropping the sender only ends the writer; the reader still owns
    /// half of the TLS stream).
    conns: BTreeMap<NodeId, Conn>,
    /// Numbers the connections, so a replaced one cannot unregister its successor.
    generation: u64,
}

struct Conn {
    generation: u64,
    tx: mpsc::Sender<QueuedMessage>,
    parent: [u8; 16],
    progress: tokio::sync::watch::Receiver<tokio::time::Instant>,
    reader: tokio::task::AbortHandle,
    writer: tokio::task::AbortHandle,
    transfers: BTreeMap<[u8; 16], Transfer>,
}

struct Transfer {
    abort: tokio::task::AbortHandle,
    version: u64,
    sha256: [u8; 32],
    kind: cb::ContentKind,
}

impl Transfer {
    fn abort(&self) {
        self.abort.abort();
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        for transfer in self.transfers.values() {
            transfer.abort();
        }
    }
}

impl Handle {
    async fn start(
        identity: Arc<identity::NodeIdentity>,
        trusted_peers: Arc<std::sync::RwLock<std::collections::BTreeSet<NodeId>>>,
        listen: SocketAddr,
        queue_capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<Inbound>), String> {
        let (events, receiver) = mpsc::channel(256);
        let listener =
            ClipboardListener::bind(Arc::clone(&identity), Arc::clone(&trusted_peers), listen)
                .await
                .map_err(|error| error.to_string())?;
        let local_addr = listener.local_addr();
        tracing::info!(address = %local_addr, "clipboard channel listening");
        let tasks = Arc::new(TaskRegistry::default());
        let handle = Handle {
            inner: Arc::new(Mutex::new(Inner {
                grants: Grants::default(),
                conns: BTreeMap::new(),
                generation: 0,
            })),
            identity,
            trusted_peers,
            events,
            epoch: Instant::now(),
            #[cfg(test)]
            listen_address: local_addr,
            tasks: Arc::downgrade(&tasks),
            _owner: Some(tasks),
        };
        let acceptor = handle.clone();
        let (shutdown, stopping) = tokio::sync::oneshot::channel();
        {
            let tasks = handle.tasks.upgrade().expect("runtime owner");
            let mut state = tasks.state.lock().expect("task registry");
            state.shutdown = Some(shutdown);
            state.acceptor = Some(tokio::spawn(async move {
                acceptor
                    .accept_loop(listener, queue_capacity, stopping)
                    .await;
            }));
        }
        Ok((handle, receiver))
    }

    /// Registration and shutdown share one lock: no task can escape the shutdown snapshot.
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let Some(tasks) = self.tasks.upgrade() else {
            return;
        };
        let mut state = tasks.state.lock().expect("task registry");
        if state.closed {
            return;
        }
        state.prune();
        state.tasks.push(tokio::spawn(future));
    }

    async fn stop(&self) {
        let Some(tasks) = self.tasks.upgrade() else {
            return;
        };
        let (handles, acceptor) = {
            let mut state = tasks.state.lock().expect("task registry");
            state.closed = true;
            if let Some(shutdown) = state.shutdown.take() {
                let _ = shutdown.send(());
            }
            let handles = std::mem::take(&mut state.tasks);
            for task in &handles {
                task.abort();
            }
            (handles, state.acceptor.take())
        };
        if let Some(acceptor) = acceptor {
            let _ = acceptor.await;
        }
        for task in handles {
            let _ = task.await;
        }
        self.inner.lock().await.conns.clear();
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Mints a channel id and a one-use token for `parent`.
    ///
    /// The expiry is stamped from **this** clock, the one [`Grants::accept`] checks the bind
    /// against (audit F5): the endpoint's clock used to be passed in, and its `Instant` origin
    /// is a different one, so the 60 s window was really `60 s - <startup delta>` and the two
    /// numbers were not comparable at all.
    async fn issue(&self, parent: [u8; 16]) -> ([u8; 16], [u8; 32]) {
        let now_ms = self.now_ms();
        self.inner.lock().await.grants.issue(parent, now_ms)
    }

    /// The expiry the store holds for `channel_id`, for the one-clock test (audit F5).
    #[cfg(test)]
    async fn grant_expiry_ms(&self, channel_id: &[u8; 16]) -> Option<u64> {
        self.inner.lock().await.grants.expires_ms(channel_id)
    }

    /// Whether a bind would be accepted at this instant, for the one-clock test (audit F5).
    #[cfg(test)]
    async fn accepts(
        &self,
        parent: &[u8; 16],
        channel: &[u8; 16],
        token: &[u8; 32],
        now_ms: u64,
    ) -> bool {
        self.inner
            .lock()
            .await
            .grants
            .accept(parent, channel, token, now_ms)
            .is_ok()
    }

    /// Queues one message on the peer's channel.
    async fn send(&self, peer: NodeId, message: ClipboardMessage) -> Result<(), ()> {
        self.try_send(peer, message).await
    }

    /// Control refusals must not wait behind a paced payload or a stalled socket.
    async fn try_send(&self, peer: NodeId, message: ClipboardMessage) -> Result<(), ()> {
        let inner = self.inner.lock().await;
        let conn = inner.conns.get(&peer).ok_or(())?;
        conn.tx
            .try_send(QueuedMessage {
                message,
                written: None,
            })
            .map_err(|_| ())
    }

    /// Paces a payload onto the peer's channel from a task of its own (`KVM-042`).
    async fn send_transfer(
        &self,
        peer: NodeId,
        offer_id: [u8; 16],
        identity: (u64, [u8; 32], cb::ContentKind),
        payload: Arc<Vec<u8>>,
        rate_bytes_per_s: u64,
        secret: bool,
    ) -> Result<(), ()> {
        let (version, sha256, kind) = identity;
        let mut inner = self.inner.lock().await;
        let Some(conn) = inner.conns.get_mut(&peer) else {
            return Err(());
        };
        let tx = conn.tx.clone();
        let generation = conn.generation;
        let mut progress = conn.progress.clone();
        let handle = self.clone();
        let Some(tasks) = self.tasks.upgrade() else {
            return Err(());
        };
        let mut state = tasks.state.lock().expect("task registry");
        if state.closed {
            return Err(());
        }
        state.prune();
        for transfer in std::mem::take(&mut conn.transfers).into_values() {
            transfer.abort();
        }
        let mut outgoing = Outgoing::prepared(
            offer_id,
            payload,
            sha256,
            kind,
            rate_bytes_per_s,
            kind.hard_max_bytes(),
            0,
        )
        .map_err(|_| ())?;
        let size = outgoing.size_bytes();
        let started = Instant::now();
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            let transfer = async {
                while let Some((message, delay_ms)) =
                    outgoing.next(started.elapsed().as_millis() as u64)
                {
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                    let (written, completion) = tokio::sync::oneshot::channel();
                    let admitted = wait_writer_progress(
                        tx.send(QueuedMessage {
                            message,
                            written: Some(written),
                        }),
                        &mut progress,
                        transport::clipboard::CLIPBOARD_WRITE_IDLE_TIMEOUT,
                    )
                    .await;
                    let completed = if matches!(admitted, Ok(Ok(()))) {
                        wait_writer_progress(
                            completion,
                            &mut progress,
                            transport::clipboard::CLIPBOARD_WRITE_IDLE_TIMEOUT,
                        )
                        .await
                    } else {
                        Err(())
                    };
                    if !matches!(completed, Ok(Ok(true))) {
                        handle.request_teardown(peer, generation);
                        return;
                    }
                }
                tracing::info!(peer = %peer, bytes = size, "clipboard transfer sent");
                {
                    let mut inner = handle.inner.lock().await;
                    if let Some(conn) = inner
                        .conns
                        .get_mut(&peer)
                        .filter(|conn| conn.generation == generation)
                    {
                        conn.transfers.remove(&offer_id);
                    }
                }
                let payload = outgoing.into_payload();
                let history = if kind == cb::ContentKind::Text {
                    let history_payload = Arc::clone(&payload);
                    tokio::task::spawn_blocking(move || {
                        String::from_utf8(history_payload.as_ref().to_vec()).ok()
                    })
                    .await
                    .ok()
                    .flatten()
                    .map(|text| (text, payload))
                } else {
                    None
                };
                let _ = events
                    .send(Inbound::Sent {
                        peer,
                        version,
                        sha256,
                        history,
                        secret,
                    })
                    .await;
            };
            if tokio::time::timeout(cb::INCOMING_TTL, transfer)
                .await
                .is_err()
            {
                handle.request_teardown(peer, generation);
            }
        });
        if let Some(old) = conn.transfers.insert(
            offer_id,
            Transfer {
                abort: task.abort_handle(),
                version,
                sha256,
                kind,
            },
        ) {
            old.abort();
        }
        state.tasks.push(task);
        Ok(())
    }

    async fn cancel_transfer(&self, peer: NodeId, offer_id: [u8; 16]) {
        let mut inner = self.inner.lock().await;
        if let Some(task) = inner
            .conns
            .get_mut(&peer)
            .and_then(|conn| conn.transfers.remove(&offer_id))
        {
            task.abort();
        }
    }

    fn request_teardown(&self, peer: NodeId, generation: u64) {
        let handle = self.clone();
        self.spawn(async move {
            handle.teardown(peer, generation).await;
        });
    }

    async fn teardown(&self, peer: NodeId, generation: u64) {
        let conn = {
            let mut inner = self.inner.lock().await;
            if !inner
                .conns
                .get(&peer)
                .is_some_and(|conn| conn.generation == generation)
            {
                return;
            }
            inner.conns.remove(&peer).expect("current generation")
        };
        let parent = conn.parent;
        let failures: Vec<_> = conn
            .transfers
            .values()
            .map(|transfer| (transfer.version, transfer.sha256))
            .collect();
        drop(conn);
        for (version, sha256) in failures {
            let _ = self
                .events
                .send(Inbound::Failed {
                    peer,
                    version,
                    sha256,
                    detail: "clipboard channel write failed or stalled".to_owned(),
                })
                .await;
        }
        let _ = self.events.send(Inbound::Down { peer, parent }).await;
    }

    /// Drops the channel to `peer` and closes its connection.
    async fn close(&self, peer: NodeId) {
        let conn = self.inner.lock().await.conns.remove(&peer);
        if let Some(conn) = conn {
            // Aborting drops the reader half; with the sender gone too the whole TLS stream is
            // closed, so a revoked peer's bound channel does not survive.
            conn.reader.abort();
        }
    }

    /// The accept loop: bind, validate, then read.
    async fn accept_loop(
        &self,
        mut listener: ClipboardListener,
        queue_capacity: usize,
        mut stopping: tokio::sync::oneshot::Receiver<()>,
    ) {
        tokio::select! {
            biased;
            _ = &mut stopping => {}
            _ = self.accept_connections(&mut listener, queue_capacity) => {}
        }
        listener.shutdown().await;
    }

    async fn accept_connections(&self, listener: &mut ClipboardListener, queue_capacity: usize) {
        loop {
            let (connection, message) = match listener.accept().await {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::warn!(%error, "clipboard accept failed");
                    continue;
                }
            };
            let ClipboardMessage::Bind {
                parent_connection_id,
                channel_id,
                binding_token,
            } = message
            else {
                connection.close().await;
                continue;
            };
            let (Ok(parent), Ok(channel), Ok(token)) = (
                <[u8; 16]>::try_from(parent_connection_id.as_slice()),
                <[u8; 16]>::try_from(channel_id.as_slice()),
                <[u8; 32]>::try_from(binding_token.as_slice()),
            ) else {
                connection.close().await;
                continue;
            };
            let peer = connection.peer;
            let accepted = self
                .inner
                .lock()
                .await
                .grants
                .accept(&parent, &channel, &token, self.now_ms())
                .is_ok();
            if !accepted {
                // One-use tokens and the 60 s ttl are what refuse a replay.
                tracing::warn!(peer = %peer, "clipboard bind refused");
                connection.close().await;
                continue;
            }
            self.serve(peer, channel, connection, queue_capacity).await;
        }
    }

    /// Registers one accepted or dialed connection and gives it its tasks.
    async fn serve(
        &self,
        peer: NodeId,
        channel_id: [u8; 16],
        connection: ClipboardConnection,
        queue_capacity: usize,
    ) {
        // A revoke that landed while this connection was being dialled or accepted: the pin set
        // no longer names the peer, so the connection is dropped (closed) instead of bound.
        let still_trusted = self
            .trusted_peers
            .read()
            .map(|pins| pins.contains(&peer))
            .unwrap_or(false);
        if !still_trusted {
            tracing::info!(peer = %peer, "clipboard connection dropped: the peer is no longer trusted");
            return;
        }
        let Some(parent) = connection.parent_connection_id() else {
            return;
        };
        let (reader, writer) = connection.into_split();
        let progress = writer.subscribe_progress();
        let writer_handle = self.clone();
        let (tx, rx) = mpsc::channel(queue_capacity.max(1));
        let events = self.events.clone();
        let handle = self.clone();
        let reader_events = events.clone();
        // Registered under the lock the reader's cleanup also takes, so a connection that
        // closes at once cannot clean up before it is registered and leave a dead entry.
        let replaced = {
            let mut inner = self.inner.lock().await;
            let Some(tasks) = self.tasks.upgrade() else {
                return;
            };
            let mut task_state = tasks.state.lock().expect("task registry");
            if task_state.closed {
                return;
            }
            task_state.prune();
            inner.generation += 1;
            let generation = inner.generation;
            let reader_task = tokio::spawn(async move {
                handle
                    .read_task(peer, parent, generation, reader, reader_events.clone())
                    .await;
                handle.request_teardown(peer, generation);
            });
            let reader_abort = reader_task.abort_handle();
            task_state.tasks.push(reader_task);
            let writer_task = tokio::spawn(async move {
                write_task(writer, rx).await;
                writer_handle.request_teardown(peer, generation);
            });
            let writer_abort = writer_task.abort_handle();
            task_state.tasks.push(writer_task);
            drop(task_state);
            inner.conns.insert(
                peer,
                Conn {
                    generation,
                    tx,
                    parent,
                    progress,
                    reader: reader_abort,
                    writer: writer_abort,
                    transfers: BTreeMap::new(),
                },
            )
        };
        if let Some(old) = replaced {
            // Both sides dialled at once: the older connection (and its sender) goes, instead of
            // lingering unregistered where a revoke could never reach it.
            old.reader.abort();
        }
        let _ = events
            .send(Inbound::Bound {
                peer,
                channel_id,
                parent,
            })
            .await;
        // Both stream halves are already registered before publishing Bound.
    }

    /// The reader half of one channel.
    async fn read_task(
        &self,
        peer: NodeId,
        parent: [u8; 16],
        generation: u64,
        mut reader: impl ClipboardFrameReader,
        events: mpsc::Sender<Inbound>,
    ) {
        let mut routes = IncomingReadRoutes(BTreeMap::new());
        loop {
            let Some(message) = reader.read_frame().await else {
                return;
            };
            match message {
                message @ ClipboardMessage::Offer { offer_id, .. } => {
                    routes.0.retain(|_, route| !route.bytes.is_closed());
                    let (ready, admitted) = tokio::sync::oneshot::channel();
                    if events
                        .send(Inbound::Offer {
                            peer,
                            parent,
                            generation,
                            message,
                            ready,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    match tokio::time::timeout(cb::INCOMING_TTL, admitted).await {
                        Ok(Ok(Some(route))) => {
                            routes.0.insert(offer_id, route);
                        }
                        Ok(Ok(None)) => {}
                        _ => return,
                    }
                }
                message @ (ClipboardMessage::Data { offer_id, .. }
                | ClipboardMessage::Complete { offer_id, .. }) => {
                    if let Some(route) = routes.0.get(&offer_id) {
                        if !matches!(
                            tokio::time::timeout(cb::INCOMING_TTL, route.send(message)).await,
                            Ok(Ok(()))
                        ) {
                            route.cancel();
                        }
                    }
                }
                message => {
                    if let ClipboardMessage::Cancel { offer_id, .. } = &message {
                        if let Some(route) = routes.0.remove(offer_id) {
                            route.cancel();
                        }
                    }
                    if events
                        .send(Inbound::Message {
                            peer,
                            parent,
                            generation,
                            message,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }

    /// The dialer: opens the channel a grant authorised.
    ///
    /// The address is the peer's *as its live input link sees it*, at the port the grant
    /// advertised. Nothing is guessed: the pin in the certificate would accept any trusted
    /// peer, so the address has to be the one this node is already talking to, and the port
    /// has to be the one the peer named (`network.port_clipboard` is configurable, so the
    /// default 47902 is only the fallback for a grant that names none).
    ///
    /// The dial runs in a task of its own and under [`DIAL_TIMEOUT`]: the caller is the session
    /// loop, and a peer that drops the SYN or stalls the TLS handshake must not freeze input
    /// links, lease renewals and the GUI for the OS connect timeout (or forever).
    async fn dial(
        &self,
        peer: NodeId,
        address: SocketAddr,
        parent: [u8; 16],
        channel_id: [u8; 16],
        token: [u8; 32],
        queue_capacity: usize,
    ) {
        let handle = self.clone();
        self.spawn(async move {
            let result = tokio::time::timeout(
                DIAL_TIMEOUT,
                dial_clipboard(
                    &handle.identity,
                    Arc::clone(&handle.trusted_peers),
                    peer,
                    address,
                    parent,
                    channel_id,
                    token,
                ),
            )
            .await;
            match result {
                Ok(Ok(connection)) => {
                    handle
                        .serve(peer, channel_id, connection, queue_capacity)
                        .await
                }
                // A refused bind shows up on the peer as a closed connection; the next grant
                // from that peer opens a fresh one.
                Ok(Err(error)) => {
                    tracing::warn!(peer = %peer, %address, %error, "the clipboard bind failed")
                }
                Err(_) => {
                    tracing::warn!(peer = %peer, %address, "the clipboard dial timed out")
                }
            }
        });
    }
}

/// How long one clipboard dial (TCP connect, TLS handshake, bind) may take.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits only while the actual channel writer continues to make progress.
async fn wait_writer_progress<F: Future>(
    future: F,
    progress: &mut tokio::sync::watch::Receiver<tokio::time::Instant>,
    idle: Duration,
) -> Result<F::Output, ()> {
    tokio::pin!(future);
    let mut deadline = tokio::time::Instant::now() + idle;
    loop {
        tokio::select! {
            biased;
            result = &mut future => return Ok(result),
            changed = progress.changed() => {
                if changed.is_err() {
                    return Err(());
                }
                let advanced = *progress.borrow_and_update();
                deadline = deadline.max(advanced + idle);
            }
            _ = tokio::time::sleep_until(deadline) => return Err(()),
        }
    }
}

/// Writes one channel's queue; the reader is a separate task so a blocked read never stalls a
/// send (`KVM-042`).
struct QueuedMessage {
    message: ClipboardMessage,
    written: Option<tokio::sync::oneshot::Sender<bool>>,
}

trait ClipboardFrameWriter {
    fn write_frame(&mut self, message: ClipboardMessage) -> impl Future<Output = bool> + Send;
}

impl ClipboardFrameWriter for ClipboardWriter {
    async fn write_frame(&mut self, message: ClipboardMessage) -> bool {
        self.send(message).await.is_ok()
    }
}

async fn write_task(
    mut writer: impl ClipboardFrameWriter,
    mut outbound: mpsc::Receiver<QueuedMessage>,
) {
    while let Some(queued) = outbound.recv().await {
        let success = writer.write_frame(queued.message).await;
        if let Some(written) = queued.written {
            let _ = written.send(success);
        }
        if !success {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_an_image_retains_the_vec_allocation() {
        let mut png = vec![0; cb::MAX_IMAGE_BYTES as usize];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let pointer = png.as_ptr();
        let content = cb::ClipboardContent::from_bytes(cb::ContentKind::Png, png).unwrap();
        let (text, retained) = retain_content(content);
        assert!(text.is_none());
        assert_eq!(pointer, retained.as_ptr());
        assert_eq!(retained.len(), cb::MAX_IMAGE_BYTES as usize);
    }

    #[tokio::test]
    async fn network_ingress_waits_for_offer_and_bounds_stalled_assembler_bytes() {
        struct Frames {
            frames: mpsc::Receiver<ClipboardMessage>,
            read: tokio::sync::watch::Sender<usize>,
        }
        impl ClipboardFrameReader for Frames {
            async fn read_frame(&mut self) -> Option<ClipboardMessage> {
                let message = self.frames.recv().await?;
                self.read.send_modify(|read| *read += 1);
                Some(message)
            }
        }
        for cancel in [false, true] {
            let peer = [0x21; 32].into();
            let (mut endpoint, root) = image_endpoint("bounded-ingress", peer).await;
            let (tx, _outbound) = mpsc::channel(1);
            let connection_reader = tokio::spawn(std::future::pending::<()>());
            let connection_writer = tokio::spawn(std::future::pending::<()>());
            endpoint.runtime.inner.lock().await.conns.insert(
                peer,
                Conn {
                    generation: 1,
                    parent: [1; 16],
                    tx,
                    progress: test_progress(),
                    reader: connection_reader.abort_handle(),
                    writer: connection_writer.abort_handle(),
                    transfers: BTreeMap::new(),
                },
            );
            let (started, assembling) = tokio::sync::oneshot::channel();
            let (release, stalled) = std::sync::mpsc::channel();
            endpoint.test_assembly_gate = Some((started, stalled));
            let (frames, wire) = mpsc::channel(16);
            let (read, mut observed) = tokio::sync::watch::channel(0);
            let (events, mut received) = mpsc::channel(4);
            let handle = endpoint.runtime.clone();
            let task = tokio::spawn(async move {
                handle
                    .read_task(peer, [1; 16], 1, Frames { frames: wire, read }, events)
                    .await;
            });
            let mut png = vec![0; 8 * cb::CHUNK_BYTES];
            png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
            frames
                .send(image_offer(peer, 1, png.len() as u64))
                .await
                .unwrap();
            for (index, chunk) in png.chunks(cb::CHUNK_BYTES).enumerate() {
                frames
                    .send(ClipboardMessage::Data {
                        offer_id: [1; 16],
                        chunk_index: index as u32,
                        chunk_count: 8,
                        data: chunk.to_vec(),
                    })
                    .await
                    .unwrap();
            }
            frames
                .send(ClipboardMessage::Complete {
                    offer_id: [1; 16],
                    sha256: cb::sha256(&png),
                })
                .await
                .unwrap();
            let Inbound::Offer {
                peer,
                parent,
                generation,
                message,
                ready,
            } = received.recv().await.unwrap()
            else {
                panic!("offer must precede data admission");
            };
            assert_eq!(
                *observed.borrow(),
                1,
                "reader cannot race Offer policy admission"
            );
            let (stale_ready, stale_admission) = tokio::sync::oneshot::channel();
            assert!(endpoint
                .on_network_offer(
                    peer,
                    parent,
                    generation + 1,
                    image_offer(peer, 2, 8),
                    stale_ready
                )
                .await
                .is_empty());
            assert!(stale_admission.await.is_err());
            assert!(
                endpoint.incoming.is_empty(),
                "replaced generation cannot reserve an offer"
            );
            assert!(endpoint
                .on_network_offer(peer, parent, generation, message, ready)
                .await
                .is_empty());
            assembling.await.unwrap();
            while *observed.borrow_and_update() < INGRESS_FRAMES + 2 {
                observed.changed().await.unwrap();
            }
            let route = endpoint.incoming[&[1; 16]].worker.as_ref().unwrap().clone();
            let sender = route.state.lock().unwrap().sender.clone().unwrap();
            assert_eq!(sender.capacity(), 0);
            assert_eq!(route.bytes.available_permits(), 0);
            assert_eq!(*observed.borrow(), INGRESS_FRAMES + 2);
            assert_eq!(sender.max_capacity(), INGRESS_FRAMES);
            assert_eq!(
                route.state.lock().unwrap().received,
                (INGRESS_BYTES + cb::CHUNK_BYTES) as u64
            );
            assert!(
                received.try_recv().is_err(),
                "payload never queues on core events"
            );
            assert!(endpoint
                .on_network_message(
                    peer,
                    parent,
                    generation + 1,
                    ClipboardMessage::Cancel {
                        offer_id: [1; 16],
                        reason: protocol::wire::ErrorCode::InvalidFrame
                    }
                )
                .await
                .is_empty());
            assert!(
                !route.bytes.is_closed(),
                "stale cancellation cannot revoke the live transfer"
            );
            if cancel {
                endpoint.on_link_down(peer).await;
                assert!(route.bytes.is_closed());
                assert!(route
                    .send(ClipboardMessage::Complete {
                        offer_id: [1; 16],
                        sha256: vec![0; 32],
                    })
                    .await
                    .is_err());
                release.send(()).unwrap();
            } else {
                let job = endpoint.incoming[&[1; 16]].job;
                release.send(()).unwrap();
                let out = verified_out(&mut endpoint, job).await;
                assert!(
                    out.iter().any(|out| matches!(out,
                Out::AgentPayload { payload, .. } if payload.as_slice() == png.as_slice())),
                    "backpressure must resume a healthy transfer, not cancel it"
                );
            }
            drop(frames);
            task.await.unwrap();
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn incoming_route_enforces_cumulative_offer_bytes_before_queueing() {
        let (route, mut queued) = IncomingRoute::new([1; 16], cb::CHUNK_BYTES as u64);
        route
            .send(ClipboardMessage::Data {
                offer_id: [1; 16],
                chunk_index: 0,
                chunk_count: 2,
                data: vec![0; cb::CHUNK_BYTES],
            })
            .await
            .unwrap();
        assert!(route
            .send(ClipboardMessage::Data {
                offer_id: [1; 16],
                chunk_index: 1,
                chunk_count: 2,
                data: vec![0],
            })
            .await
            .is_err());
        assert_eq!(route.state.lock().unwrap().received, cb::CHUNK_BYTES as u64);
        assert!(queued.try_recv().is_ok());
        assert!(queued.try_recv().is_err());
        route.cancel();
    }

    #[tokio::test]
    async fn verified_handoff_keeps_two_full_image_slots_until_final_owner_drops() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("handoff-budget", peer).await;
        let mut png = vec![0; cb::MAX_IMAGE_BYTES as usize];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        endpoint
            .on_peer_message(peer, image_offer(peer, 1, cb::MAX_IMAGE_BYTES))
            .await;
        let verified = finish_image(&mut endpoint, peer, 1, &png).await;
        let pointer = verified.result.as_ref().unwrap().1.as_ptr();
        let out = endpoint.on_verified(verified);
        let (payload, producer, supervisor) = out
            .into_iter()
            .find_map(|out| match out {
                Out::AgentPayload {
                    payload,
                    reservation: Some(guard),
                    ..
                } => Some((payload, Arc::clone(&guard), guard)),
                _ => None,
            })
            .unwrap();
        assert_eq!(pointer, payload.as_ptr());
        endpoint
            .on_peer_message(peer, image_offer(peer, 2, cb::MAX_IMAGE_BYTES))
            .await;
        let held = finish_image(&mut endpoint, peer, 2, &png).await;
        assert_eq!(
            endpoint
                .reservations
                .lock()
                .unwrap()
                .values()
                .map(|(_, bytes)| bytes)
                .sum::<u64>(),
            cb::MAX_INCOMING_IMAGE_BYTES
        );
        drop(supervisor);
        assert!(matches!(
            endpoint
                .on_peer_message(peer, image_offer(peer, 3, 8))
                .await
                .as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::TooLarge,
                ..
            }]
        ));
        drop(payload);
        drop(producer);
        assert!(endpoint
            .on_peer_message(peer, image_offer(peer, 3, cb::MAX_IMAGE_BYTES))
            .await
            .is_empty());
        let third = finish_image(&mut endpoint, peer, 3, &png).await;
        endpoint.set_images(false).await;
        assert_eq!(
            endpoint
                .reservations
                .lock()
                .unwrap()
                .values()
                .filter(|(kind, _)| *kind == cb::ContentKind::Png)
                .map(|(_, bytes)| bytes)
                .sum::<u64>(),
            cb::MAX_INCOMING_IMAGE_BYTES
        );
        assert!(endpoint.on_verified(held).is_empty());
        assert!(endpoint.on_verified(third).is_empty());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_frame_drops_queue_without_writing_another_frame() {
        struct FailedWriter(Arc<std::sync::atomic::AtomicUsize>);
        impl ClipboardFrameWriter for FailedWriter {
            async fn write_frame(&mut self, _message: ClipboardMessage) -> bool {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel(2);
        let mut completions = Vec::new();
        for _ in 0..2 {
            let (written, completion) = tokio::sync::oneshot::channel();
            tx.send(QueuedMessage {
                message: ClipboardMessage::Request { offer_id: [1; 16] },
                written: Some(written),
            })
            .await
            .unwrap();
            completions.push(completion);
        }
        write_task(FailedWriter(Arc::clone(&writes)), rx).await;
        assert_eq!(writes.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!completions.remove(0).await.unwrap());
        assert!(completions.remove(0).await.is_err());
        assert!(tx.is_closed());
    }

    fn test_progress() -> tokio::sync::watch::Receiver<tokio::time::Instant> {
        static PROGRESS: std::sync::LazyLock<tokio::sync::watch::Sender<tokio::time::Instant>> =
            std::sync::LazyLock::new(|| tokio::sync::watch::channel(tokio::time::Instant::now()).0);
        PROGRESS.subscribe()
    }

    async fn verified_out(endpoint: &mut Clipboard, job: u64) -> Vec<Out> {
        loop {
            let event = tokio::time::timeout(
                Duration::from_secs(2),
                endpoint.test_events.as_mut().expect("test receiver").recv(),
            )
            .await
            .expect("verification finishes")
            .expect("runtime open");
            if let Inbound::Verified(verified) = event {
                let matching = verified.job == job;
                let out = endpoint.on_verified(verified);
                if matching {
                    return out;
                }
            }
        }
    }

    #[tokio::test]
    async fn stalled_queue_and_confirmation_waits_are_bounded_by_writer_progress() {
        let (progress, mut observed) = tokio::sync::watch::channel(tokio::time::Instant::now());
        let (tx, _rx) = mpsc::channel(1);
        tx.send(()).await.unwrap();
        assert!(
            wait_writer_progress(tx.send(()), &mut observed, Duration::from_millis(10))
                .await
                .is_err()
        );
        let (_completion, confirmation) = tokio::sync::oneshot::channel::<bool>();
        assert!(
            wait_writer_progress(confirmation, &mut observed, Duration::from_millis(10))
                .await
                .is_err()
        );
        let advancing = tokio::spawn(async move {
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(5)).await;
                progress.send_replace(tokio::time::Instant::now());
            }
        });
        assert!(wait_writer_progress(
            tokio::time::sleep(Duration::from_millis(30)),
            &mut observed,
            Duration::from_millis(20),
        )
        .await
        .is_ok());
        advancing.await.unwrap();
    }

    #[tokio::test]
    async fn generation_teardown_closes_both_halves_fails_transfer_and_preserves_replacement() {
        let (mut endpoint, root) = started_clipboard("writer-teardown").await;
        let peer: NodeId = [0x21; 32].into();
        let (tx, _rx) = mpsc::channel(1);
        let reader = tokio::spawn(std::future::pending::<()>());
        let writer = tokio::spawn(std::future::pending::<()>());
        let transfer = tokio::spawn(std::future::pending::<()>());
        let mut transfers = BTreeMap::new();
        transfers.insert(
            [3; 16],
            Transfer {
                abort: transfer.abort_handle(),
                version: 7,
                sha256: [9; 32],
                kind: cb::ContentKind::Text,
            },
        );
        endpoint.runtime.inner.lock().await.conns.insert(
            peer,
            Conn {
                generation: 2,
                tx,
                parent: [2; 16],
                progress: test_progress(),
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
                transfers,
            },
        );
        endpoint.runtime.teardown(peer, 1).await;
        assert!(endpoint
            .runtime
            .inner
            .lock()
            .await
            .conns
            .contains_key(&peer));
        assert!(!reader.is_finished());
        endpoint.runtime.teardown(peer, 2).await;
        assert!(!endpoint
            .runtime
            .inner
            .lock()
            .await
            .conns
            .contains_key(&peer));
        assert!(reader.await.unwrap_err().is_cancelled());
        assert!(writer.await.unwrap_err().is_cancelled());
        assert!(transfer.await.unwrap_err().is_cancelled());
        let events = endpoint.test_events.as_mut().unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Inbound::Failed {
                version: 7,
                sha256: [9, ..],
                ..
            })
        ));
        assert!(matches!(
            events.recv().await,
            Some(Inbound::Down {
                parent: [2, ..],
                ..
            })
        ));
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn delayed_verification_revalidates_local_copy_parent_epoch_expiry_and_policy() {
        for change in 0..5 {
            let (mut endpoint, root) = started_clipboard("stale-verification").await;
            let peer: NodeId = [0x21; 32].into();
            offer_and_data(&mut endpoint, peer, [3; 16], b"text").await;
            let job = endpoint.incoming[&[3; 16]].job;
            assert!(
                endpoint
                    .on_peer_message(
                        peer,
                        ClipboardMessage::Complete {
                            offer_id: [3; 16],
                            sha256: cb::sha256(b"text"),
                        }
                    )
                    .await
                    .is_empty(),
                "session does not await assembly/hash completion"
            );
            let finished = loop {
                let event = tokio::time::timeout(
                    Duration::from_secs(2),
                    endpoint.test_events.as_mut().unwrap().recv(),
                )
                .await
                .unwrap()
                .unwrap();
                if let Inbound::Verified(verified) = event {
                    break verified;
                }
            };
            assert!(endpoint.reservations.lock().unwrap().contains_key(&job));
            match change {
                0 => {
                    endpoint.clear_carry().await;
                }
                1 => {
                    endpoint.peers.entry(peer).or_default().parent = Some([8; 16]);
                }
                2 => {
                    endpoint.peers.entry(peer).or_default().epoch = Some([8; 16]);
                }
                3 => {
                    endpoint.incoming.get_mut(&[3; 16]).unwrap().deadline = Instant::now();
                }
                _ => {
                    endpoint.settings.enabled = false;
                }
            }
            let (control, input) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                control.send(()).unwrap();
            });
            input
                .await
                .expect("control remains live while completion is delayed");
            assert!(endpoint.on_verified(finished).is_empty());
            assert!(!endpoint.reservations.lock().unwrap().contains_key(&job));
            assert!(!endpoint.source.applied_from_peer());
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn actual_carried_fanout_marks_relay_and_receiver_accepts_authenticated_provenance() {
        let (mut sender, sender_root) = started_clipboard("relay-producer").await;
        let (mut receiver, receiver_root) = started_clipboard("relay-receiver").await;
        let sender_id: NodeId = sender.source.origin.into();
        let receiver_id: NodeId = receiver.source.origin.into();
        let origin: NodeId = [0x21; 32].into();
        receiver
            .runtime
            .trusted_peers
            .write()
            .unwrap()
            .extend([sender_id, origin]);
        let carried = cb::Offer {
            offer_id: [71; 16],
            origin: *origin.as_bytes(),
            version: 3,
            mime: cb::MIME.to_owned(),
            size: 0,
        };
        sender.carried = Some((carried, cb::sha256(b"").try_into().unwrap()));
        sender.settings.routing_policy = config::ClipboardRoutingPolicy::Broadcast;
        sender.set_peer_session(
            receiver_id,
            [1; 16],
            Some(transport::clipboard::process_epoch()),
        );
        sender
            .on_link_up(receiver_id, [1; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        let message = sender
            .waiting
            .remove(&receiver_id)
            .expect("carried fanout queued")
            .message;
        assert!(
            matches!(&message, ClipboardMessage::Offer { relayed: true, sender_epoch, .. }
            if sender_epoch.as_slice() == transport::clipboard::process_epoch())
        );
        receiver.set_peer_session(
            sender_id,
            [1; 16],
            Some(transport::clipboard::process_epoch()),
        );
        receiver.peers.get_mut(&sender_id).unwrap().text_clipboard = true;
        assert!(receiver
            .on_peer_message_from(sender_id, [1; 16], message)
            .await
            .is_empty());
        assert!(complete_empty(&mut receiver, sender_id, [1; 16], 71)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        // Old peers may still receive direct local carries, but never ambiguous relays.
        sender.peers.get_mut(&receiver_id).unwrap().epoch = None;
        let (offer, hash) = sender.carried.clone().unwrap();
        assert!(sender
            .offer_to(receiver_id, offer, Arc::new(Vec::new()), hash, false)
            .await
            .is_err());
        sender.runtime.stop().await;
        receiver.runtime.stop().await;
        let _ = std::fs::remove_dir_all(sender_root);
        let _ = std::fs::remove_dir_all(receiver_root);
    }

    #[tokio::test]
    async fn rebinding_listener_preserves_logical_source_carry_and_authenticated_epoch() {
        let (mut endpoint, root) = started_clipboard("rebind-state").await;
        endpoint
            .on_local_copy(b"retained carry".to_vec(), Vec::new())
            .await;
        let peer: NodeId = [0x21; 32].into();
        endpoint.set_peer_session(peer, [1; 16], Some([11; 16]));
        endpoint.peers.get_mut(&peer).unwrap().text_clipboard = true;
        endpoint
            .on_peer_message_from(peer, [1; 16], session_offer(peer, [11; 16], 7, 1, false))
            .await;
        assert!(complete_empty(&mut endpoint, peer, [1; 16], 1)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        let old_address = endpoint.runtime.listen_address;
        let version = endpoint.source.version();
        let carry = endpoint.carried.clone();
        let payload = endpoint.payload.clone();
        let _events = endpoint
            .rebind_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let address = endpoint.runtime.listen_address;
        assert_ne!(address, old_address);
        assert_eq!(endpoint.source.version(), version);
        assert_eq!(endpoint.carried, carry);
        assert_eq!(endpoint.payload, payload);
        assert_eq!(endpoint.peers[&peer].epoch, Some([11; 16]));
        assert_eq!(
            endpoint.received[&peer].echo.highest(peer.as_bytes()),
            Some(7)
        );
        let old_listener = tokio::net::TcpListener::bind(old_address)
            .await
            .expect("old listener was closed");
        let new_connection = tokio::net::TcpStream::connect(address)
            .await
            .expect("replacement listener accepts");
        drop(new_connection);
        drop(old_listener);
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    fn session_offer(
        origin: NodeId,
        epoch: [u8; 16],
        version: u64,
        id: u8,
        relayed: bool,
    ) -> ClipboardMessage {
        ClipboardMessage::Offer {
            offer_id: [id; 16],
            origin_node_id: *origin.as_bytes(),
            origin_version: version,
            mime: cb::MIME.to_owned(),
            size_bytes: 0,
            secret_marked: false,
            sender_epoch: epoch.to_vec(),
            relayed,
        }
    }

    async fn complete_empty(
        endpoint: &mut Clipboard,
        peer: NodeId,
        parent: [u8; 16],
        id: u8,
    ) -> Vec<Out> {
        endpoint
            .on_peer_message_from(
                peer,
                parent,
                ClipboardMessage::Data {
                    offer_id: [id; 16],
                    chunk_index: 0,
                    chunk_count: 1,
                    data: Vec::new(),
                },
            )
            .await;
        let out = endpoint
            .on_peer_message_from(
                peer,
                parent,
                ClipboardMessage::Complete {
                    offer_id: [id; 16],
                    sha256: cb::sha256(b""),
                },
            )
            .await;
        match endpoint.incoming.get(&[id; 16]).filter(|incoming| {
            incoming
                .worker
                .as_ref()
                .is_some_and(|route| route.state.lock().unwrap().complete)
        }) {
            Some(incoming) => {
                let job = incoming.job;
                verified_out(endpoint, job).await
            }
            None => out,
        }
    }

    #[tokio::test]
    async fn relay_provenance_cannot_poison_direct_origin_or_accept_untrusted_origins() {
        let (mut endpoint, root) = started_clipboard("relay-provenance").await;
        let a: NodeId = [0x21; 32].into();
        let b: NodeId = [0x31; 32].into();
        endpoint
            .runtime
            .trusted_peers
            .write()
            .unwrap()
            .extend([a, b]);
        endpoint.set_peer_session(a, [1; 16], Some([11; 16]));
        endpoint.set_peer_session(b, [2; 16], Some([22; 16]));
        endpoint.peers.get_mut(&a).unwrap().text_clipboard = true;
        endpoint.peers.get_mut(&b).unwrap().text_clipboard = true;
        let spoofed = endpoint
            .on_peer_message_from(b, [2; 16], session_offer(a, [22; 16], u64::MAX, 1, false))
            .await;
        assert!(matches!(
            spoofed.as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::Invalid,
                ..
            }]
        ));
        assert!(endpoint
            .on_peer_message_from(b, [2; 16], session_offer(a, [22; 16], u64::MAX, 2, true))
            .await
            .is_empty());
        assert!(complete_empty(&mut endpoint, b, [2; 16], 2)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        assert!(endpoint
            .on_peer_message_from(a, [1; 16], session_offer(a, [11; 16], 1, 3, false))
            .await
            .is_empty());
        assert!(complete_empty(&mut endpoint, a, [1; 16], 3)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        let untrusted = endpoint
            .on_peer_message_from(
                b,
                [2; 16],
                session_offer([0x41; 32].into(), [22; 16], 1, 4, true),
            )
            .await;
        assert!(matches!(
            untrusted.as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::Invalid,
                ..
            }]
        ));
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn authenticated_restart_accepts_low_versions_and_rejects_old_sessions_and_completions() {
        let (mut endpoint, root) = started_clipboard("restart-replay").await;
        let a: NodeId = [0x21; 32].into();
        endpoint.set_peer_session(a, [1; 16], Some([11; 16]));
        endpoint.peers.get_mut(&a).unwrap().text_clipboard = true;
        endpoint
            .on_peer_message_from(a, [1; 16], session_offer(a, [11; 16], 90, 1, false))
            .await;
        assert!(complete_empty(&mut endpoint, a, [1; 16], 1)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        endpoint
            .on_peer_message_from(a, [1; 16], session_offer(a, [11; 16], 91, 2, false))
            .await;
        endpoint.on_link_down(a).await;
        endpoint.set_peer_session(a, [2; 16], Some([22; 16]));
        endpoint
            .on_link_up(a, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [22; 16], 1, 3, false))
            .await;
        assert!(complete_empty(&mut endpoint, a, [2; 16], 3)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        assert!(endpoint
            .on_peer_message_from(a, [1; 16], session_offer(a, [11; 16], 92, 4, false))
            .await
            .is_empty());
        let replay = endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [11; 16], 92, 5, false))
            .await;
        assert!(matches!(
            replay.as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::Invalid,
                ..
            }]
        ));
        assert!(complete_empty(&mut endpoint, a, [1; 16], 2)
            .await
            .is_empty());
        // A reservation alone is not a committed high-water mark.
        endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [22; 16], u64::MAX, 6, false))
            .await;
        endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [22; 16], 2, 7, false))
            .await;
        assert!(complete_empty(&mut endpoint, a, [2; 16], 7)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [22; 16], 3, 8, false))
            .await;
        endpoint
            .on_peer_message_from(a, [2; 16], session_offer(a, [22; 16], 4, 9, false))
            .await;
        assert!(complete_empty(&mut endpoint, a, [2; 16], 9)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        assert!(complete_empty(&mut endpoint, a, [2; 16], 8)
            .await
            .is_empty());
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn reconnecting_the_same_process_does_not_replay_committed_clipboard() {
        let (mut endpoint, root) = started_clipboard("same-epoch-replay").await;
        let peer: NodeId = [0x21; 32].into();
        let epoch = [11; 16];
        endpoint.set_peer_session(peer, [1; 16], Some(epoch));
        endpoint.peers.get_mut(&peer).unwrap().text_clipboard = true;
        endpoint
            .on_peer_message_from(peer, [1; 16], session_offer(peer, epoch, 90, 1, false))
            .await;
        assert!(complete_empty(&mut endpoint, peer, [1; 16], 1)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));

        endpoint.on_link_down(peer).await;
        endpoint.set_peer_session(peer, [2; 16], Some(epoch));
        endpoint
            .on_link_up(peer, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        endpoint
            .on_peer_message_from(peer, [2; 16], session_offer(peer, epoch, 90, 2, false))
            .await;
        assert!(
            !endpoint.incoming.contains_key(&[2; 16]),
            "a new connection must retain the authenticated process high-water"
        );
        assert!(complete_empty(&mut endpoint, peer, [2; 16], 2)
            .await
            .is_empty());
        endpoint
            .on_peer_message_from(peer, [2; 16], session_offer(peer, epoch, 91, 3, false))
            .await;
        assert!(complete_empty(&mut endpoint, peer, [2; 16], 3)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_reconnect_preserves_committed_clipboard_high_water() {
        for initial_epoch in [None, Some([11; 16])] {
            let (mut endpoint, root) = started_clipboard("legacy-reconnect-replay").await;
            let peer: NodeId = [0x21; 32].into();
            let offer = |epoch: Option<[u8; 16]>, version, id| {
                let mut message = session_offer(peer, [0; 16], version, id, false);
                if let ClipboardMessage::Offer { sender_epoch, .. } = &mut message {
                    *sender_epoch = epoch.map(|value| value.to_vec()).unwrap_or_default();
                }
                message
            };
            endpoint.set_peer_session(peer, [1; 16], initial_epoch);
            endpoint.peers.get_mut(&peer).unwrap().text_clipboard = true;
            endpoint
                .on_peer_message_from(peer, [1; 16], offer(initial_epoch, 90, 1))
                .await;
            assert!(complete_empty(&mut endpoint, peer, [1; 16], 1)
                .await
                .iter()
                .any(|out| matches!(out, Out::AgentPayload { .. })));

            endpoint.on_link_down(peer).await;
            endpoint.set_peer_session(peer, [2; 16], None);
            endpoint
                .on_link_up(peer, [2; 16], true, "127.0.0.1".parse().unwrap())
                .await;
            endpoint
                .on_peer_message_from(peer, [2; 16], offer(None, 90, 2))
                .await;
            assert!(
                !endpoint.incoming.contains_key(&[2; 16]),
                "absence of an authenticated epoch must not reset committed versions"
            );
            assert!(complete_empty(&mut endpoint, peer, [2; 16], 2)
                .await
                .is_empty());
            endpoint
                .on_peer_message_from(peer, [2; 16], offer(None, 91, 3))
                .await;
            assert!(complete_empty(&mut endpoint, peer, [2; 16], 3)
                .await
                .iter()
                .any(|out| matches!(out, Out::AgentPayload { .. })));
            endpoint.runtime.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn direct_offer_cannot_claim_another_authenticated_origin() {
        let (mut endpoint, root) = started_clipboard("direct-origin").await;
        let peer: NodeId = [0x21; 32].into();
        let out = endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Offer {
                    offer_id: [0x61; 16],
                    origin_node_id: [0x31; 32],
                    origin_version: u64::MAX,
                    mime: cb::MIME.to_owned(),
                    size_bytes: 0,
                    secret_marked: false,
                    sender_epoch: Vec::new(),
                    relayed: false,
                },
            )
            .await;
        assert!(matches!(
            out.as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::Invalid,
                ..
            }]
        ));
        assert!(endpoint.incoming.is_empty());
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn incoming_reservations_expire_and_terminal_paths_reclaim_them() {
        let (mut endpoint, root) = started_clipboard("incoming-cleanup").await;
        let peer: NodeId = [0x21; 32].into();
        offer_and_data(&mut endpoint, peer, [1; 16], b"text").await;
        let deadline = endpoint.incoming[&[1; 16]].deadline;
        assert!(endpoint
            .expire_incoming(deadline - Duration::from_nanos(1))
            .is_empty());
        let expired = endpoint.expire_incoming(deadline);
        assert!(
            matches!(expired.as_slice(), [Out::Cancel { offer_id, .. }] if *offer_id == [1; 16])
        );
        assert!(endpoint.incoming.is_empty());
        assert!(endpoint.pending_request.is_none());
        endpoint.received.clear();
        offer_and_data(&mut endpoint, peer, [2; 16], b"text").await;
        endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Cancel {
                    offer_id: [2; 16],
                    reason: protocol::wire::ErrorCode::ClipboardSuperseded,
                },
            )
            .await;
        assert!(endpoint.incoming.is_empty());
        endpoint.received.clear();
        offer_and_data(&mut endpoint, peer, [3; 16], b"text").await;
        let _applied = endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id: [3; 16],
                    sha256: cb::sha256(b"text"),
                },
            )
            .await;
        let job = endpoint.incoming[&[3; 16]].job;
        let applied = verified_out(&mut endpoint, job).await;
        assert!(applied
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        assert!(endpoint.incoming.is_empty());
        endpoint.received.clear();
        offer_and_data(&mut endpoint, peer, [4; 16], b"text").await;
        endpoint.on_link_down(peer).await;
        assert!(endpoint.incoming.is_empty());
        assert!(endpoint.pending_request.is_none());
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn incoming_offers_have_count_and_aggregate_reservation_limits() {
        for size in [0, cb::MAX_BYTES] {
            let (mut endpoint, root) = started_clipboard("incoming-budget").await;
            let peer: NodeId = [0x21; 32].into();
            for version in 1u64..=64 {
                let mut offer_id = [0; 16];
                offer_id[..8].copy_from_slice(&version.to_be_bytes());
                endpoint
                    .on_peer_message(
                        peer,
                        ClipboardMessage::Offer {
                            offer_id,
                            origin_node_id: *peer.as_bytes(),
                            origin_version: version,
                            mime: cb::MIME.to_owned(),
                            size_bytes: size,
                            secret_marked: false,
                            sender_epoch: Vec::new(),
                            relayed: false,
                        },
                    )
                    .await;
            }
            assert_eq!(
                endpoint.incoming.len(),
                if size == 0 { 8 } else { 4 },
                "valid offers fill exactly the count or shared-byte budget"
            );
            assert!(
                endpoint
                    .incoming
                    .values()
                    .map(|i| i.offer.size)
                    .sum::<u64>()
                    <= 4 * cb::MAX_BYTES,
                "declared payload reservations must have a shared budget"
            );
            endpoint.runtime.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn expired_reservations_and_refusals_do_not_wait_on_a_full_outbound_queue() {
        let (mut endpoint, root) = started_clipboard("expiry-backpressure").await;
        let peer: NodeId = [0x21; 32].into();
        offer_and_data(&mut endpoint, peer, [1; 16], b"text").await;
        let deadline = endpoint.incoming[&[1; 16]].deadline;
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(QueuedMessage {
            message: ClipboardMessage::Request { offer_id: [9; 16] },
            written: None,
        })
        .unwrap();
        let reader = tokio::spawn(std::future::pending::<()>());
        let writer = tokio::spawn(std::future::pending::<()>());
        endpoint.runtime.inner.lock().await.conns.insert(
            peer,
            Conn {
                generation: 1,
                tx,
                parent: [1; 16],
                progress: test_progress(),
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
                transfers: BTreeMap::new(),
            },
        );
        let expired = endpoint.expire_incoming(deadline);
        assert!(endpoint.incoming.is_empty());
        let [Out::Cancel {
            offer_id, reason, ..
        }] = expired.as_slice()
        else {
            panic!("expiry returns one refusal");
        };
        tokio::time::timeout(
            Duration::from_millis(100),
            endpoint.cancel(peer, *offer_id, *reason),
        )
        .await
        .expect("full channel cannot block expiry refusal");
        endpoint.runtime.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_and_new_local_copy_abort_active_paced_delivery() {
        for new_copy in [false, true] {
            let (mut endpoint, root) = started_clipboard("abort-addressed").await;
            let peer: NodeId = [0x21; 32].into();
            let offer_id = [0x41; 16];
            let payload = vec![b'x'; cb::CHUNK_BYTES * 3];
            let hash = cb::sha256(&payload).try_into().unwrap();
            let (tx, mut rx) = mpsc::channel::<QueuedMessage>(4);
            let reader = tokio::spawn(std::future::pending::<()>());
            let writer = tokio::spawn(std::future::pending::<()>());
            endpoint.runtime.inner.lock().await.conns.insert(
                peer,
                Conn {
                    generation: 1,
                    tx,
                    parent: [1; 16],
                    progress: test_progress(),
                    reader: reader.abort_handle(),
                    writer: writer.abort_handle(),
                    transfers: BTreeMap::new(),
                },
            );
            endpoint.deliveries.insert(peer, (offer_id, 1, hash));
            endpoint
                .runtime
                .send_transfer(
                    peer,
                    offer_id,
                    (1, hash, cb::ContentKind::Text),
                    payload.into(),
                    cb::CHUNK_BYTES as u64 * 4,
                    false,
                )
                .await
                .unwrap();
            let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("first chunk")
                .expect("channel");
            assert!(matches!(
                first.message,
                ClipboardMessage::Data { chunk_index: 0, .. }
            ));
            first.written.unwrap().send(true).unwrap();
            if new_copy {
                endpoint
                    .on_local_copy(b"secret".to_vec(), vec![cb::Marker::ExcludeFromMonitor])
                    .await;
            } else {
                endpoint.cancel_delivery(peer, 1, hash).await;
            }
            assert!(!endpoint.runtime.inner.lock().await.conns[&peer]
                .transfers
                .contains_key(&offer_id));
            let cancel = rx.recv().await.expect("peer cancellation");
            assert!(
                matches!(cancel.message, ClipboardMessage::Cancel { offer_id: id, .. } if id == offer_id)
            );
            assert!(rx.try_recv().is_err(), "no further payload is queued");
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn cancelling_delivery_is_exact_and_channel_down_cannot_replay_it() {
        let (mut endpoint, root) = started_clipboard("cancel-delivery").await;
        let peer: NodeId = [0x21; 32].into();
        endpoint
            .on_link_up(peer, [1; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        endpoint
            .on_local_copy(b"current".to_vec(), Vec::new())
            .await;
        let hash = endpoint.carried.as_ref().unwrap().1;
        endpoint.deliver_carried(peer, hash).await.unwrap();
        let first_version = endpoint.source.version();
        endpoint
            .send_text_to(peer, "new destination text")
            .await
            .unwrap();
        let (_, current_version, current_hash) = endpoint.deliveries[&peer];
        endpoint.cancel_delivery(peer, first_version, hash).await;
        assert_eq!(&*endpoint.waiting[&peer].payload, b"new destination text");
        endpoint
            .cancel_delivery(peer, current_version, [0; 32])
            .await;
        assert!(endpoint.waiting.contains_key(&peer));
        endpoint
            .cancel_delivery(peer, current_version, current_hash)
            .await;
        assert!(!endpoint.waiting.contains_key(&peer));
        assert!(!endpoint.offered.contains_key(&peer));
        assert!(!endpoint.deliveries.contains_key(&peer));
        endpoint.deliver_carried(peer, hash).await.unwrap();
        endpoint.on_channel_down(peer).await;
        assert!(!endpoint.waiting.contains_key(&peer));
        assert!(!endpoint.offered.contains_key(&peer));
        assert!(!endpoint.deliveries.contains_key(&peer));
        endpoint.on_bound(peer, [3; 16]).await;
        assert!(!endpoint.offered.contains_key(&peer));
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn addressed_push_and_broadcast_request_support_deferred_legacy_receivers() {
        for immediate in [true, false] {
            for bound in [true, false] {
                let (mut sender, root) = started_clipboard("addressed-no-request").await;
                sender.settings.rate_bytes_per_s = cb::CHUNK_BYTES as u64 * 4;
                sender.settings.routing_policy = if immediate {
                    config::ClipboardRoutingPolicy::Follow
                } else {
                    config::ClipboardRoutingPolicy::Broadcast
                };
                let (mut receiver, receiver_root) = started_clipboard("deferred-receiver").await;
                assert!(!receiver.settings.eager);
                let peer: NodeId = [0x21; 32].into();
                receiver.source = cb::Source::new(*peer.as_bytes());
                sender
                    .on_link_up(peer, [1; 16], true, "127.0.0.1".parse().unwrap())
                    .await;
                if bound {
                    sender.peers.get_mut(&peer).unwrap().channel = Some([3; 16]);
                }
                let (tx, mut rx) = mpsc::channel::<QueuedMessage>(4);
                let reader = tokio::spawn(std::future::pending::<()>());
                let writer = tokio::spawn(std::future::pending::<()>());
                sender.runtime.inner.lock().await.conns.insert(
                    peer,
                    Conn {
                        generation: 1,
                        tx,
                        parent: [1; 16],
                        progress: test_progress(),
                        reader: reader.abort_handle(),
                        writer: writer.abort_handle(),
                        transfers: BTreeMap::new(),
                    },
                );
                let out = if immediate {
                    sender.send_text_to(peer, "text").await.unwrap()
                } else {
                    sender.on_local_copy(b"text".to_vec(), Vec::new()).await
                };
                assert_eq!(
                    out.iter()
                        .any(|event| matches!(event, Out::Delivery { .. })),
                    immediate
                );
                let (_, version, hash) = sender.deliveries[&peer];
                assert!(sender.runtime.inner.lock().await.conns[&peer]
                    .transfers
                    .is_empty());
                sender.start_delivery(peer, version, hash).await;
                if !bound {
                    assert!(
                        rx.try_recv().is_err(),
                        "data waits for the authorized channel"
                    );
                    sender.on_bound(peer, [3; 16]).await;
                }
                let offer = rx.recv().await.expect("offer precedes data");
                let ClipboardMessage::Offer {
                    origin_node_id,
                    offer_id,
                    ..
                } = &offer.message
                else {
                    panic!("expected offer");
                };
                let origin: NodeId = (*origin_node_id).into();
                let offer_id = *offer_id;
                receiver
                    .on_link_up(origin, [1; 16], true, "127.0.0.1".parse().unwrap())
                    .await;
                receiver.on_peer_message(origin, offer.message).await;
                assert_eq!(receiver.pending_request, Some((origin, offer_id)));
                if !immediate {
                    assert!(
                        sender.runtime.inner.lock().await.conns[&peer]
                            .transfers
                            .is_empty(),
                        "broadcast still waits for receiver intent"
                    );
                    let (requests, mut request_rx) = mpsc::channel::<QueuedMessage>(4);
                    let request_reader = tokio::spawn(std::future::pending::<()>());
                    let request_writer = tokio::spawn(std::future::pending::<()>());
                    receiver.runtime.inner.lock().await.conns.insert(
                        origin,
                        Conn {
                            generation: 1,
                            tx: requests,
                            parent: [1; 16],
                            progress: test_progress(),
                            reader: request_reader.abort_handle(),
                            writer: request_writer.abort_handle(),
                            transfers: BTreeMap::new(),
                        },
                    );
                    receiver.on_focus().await;
                    let request = request_rx.recv().await.expect("receiver requests on focus");
                    assert!(matches!(
                        request.message,
                        ClipboardMessage::Request { offer_id: requested } if requested == offer_id
                    ));
                    sender.on_peer_message(peer, request.message).await;
                }
                loop {
                    let queued = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                        .await
                        .expect("wire data starts after delivery intent")
                        .expect("channel");
                    let complete = matches!(&queued.message, ClipboardMessage::Complete { .. });
                    let _queued = receiver.on_peer_message(origin, queued.message).await;
                    if let Some(written) = queued.written {
                        written.send(true).unwrap();
                    }
                    if complete {
                        let job = receiver.incoming[&offer_id].job;
                        let applied = verified_out(&mut receiver, job).await;
                        assert!(applied
                            .iter()
                            .any(|event| matches!(event, Out::AgentPayload { .. })));
                        assert!(applied.iter().any(|event| matches!(
                        event,
                        Out::History { text, direction: ipc::ClipboardHistoryDirection::Received, .. }
                            if text == "text"
                    )));
                        assert!(receiver.pending_request.is_none());
                        break;
                    }
                }
                sender
                    .on_peer_message(peer, ClipboardMessage::Request { offer_id })
                    .await;
                assert!(
                    rx.try_recv().is_err(),
                    "a late request cannot duplicate the transfer"
                );
                receiver.on_focus().await;
                assert!(receiver.pending_request.is_none());
                sender.stop().await;
                receiver.stop().await;
                let _ = std::fs::remove_dir_all(root);
                let _ = std::fs::remove_dir_all(receiver_root);
            }
        }
    }

    #[tokio::test]
    async fn follow_carries_without_fanout_or_late_join_delivery() {
        let (mut endpoint, root) = started_clipboard("follow-carry").await;
        let first: NodeId = [0x21; 32].into();
        let late: NodeId = [0x22; 32].into();
        endpoint
            .on_link_up(first, [1; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        let out = endpoint.on_local_copy(b"carry".to_vec(), Vec::new()).await;
        let (offer, hash) = endpoint.carried.clone().expect("carry");
        assert!(out.iter().any(|event| matches!(event,
            Out::Carried { origin, version, sha256 }
                if *origin == NodeId::from(offer.origin) && *version == offer.version && *sha256 == hash
        )));
        assert!(out.iter().any(|event| matches!(event,
            Out::History { direction: ipc::ClipboardHistoryDirection::Copied, text, secret: false, .. }
                if text == "carry"
        )));
        assert!(!out
            .iter()
            .any(|event| matches!(event, Out::Grant { .. } | Out::Delivery { .. })));
        assert!(endpoint.waiting.is_empty());
        assert!(endpoint.offered.is_empty());
        assert!(endpoint
            .on_link_up(late, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await
            .is_empty());
        assert_eq!(endpoint.source.version(), offer.version);
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn broadcast_offers_to_members_and_late_joiners() {
        let (mut endpoint, root) = started_clipboard("broadcast-carry").await;
        endpoint.settings.routing_policy = config::ClipboardRoutingPolicy::Broadcast;
        let first: NodeId = [0x21; 32].into();
        let late: NodeId = [0x22; 32].into();
        endpoint
            .on_link_up(first, [1; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        let out = endpoint
            .on_local_copy(b"broadcast".to_vec(), Vec::new())
            .await;
        assert!(out.iter().any(|event| matches!(event, Out::CarryCleared)));
        assert!(!out.iter().any(|event| matches!(event, Out::Carried { .. })));
        assert!(out.iter().any(|event| matches!(event,
            Out::History { direction: ipc::ClipboardHistoryDirection::Copied, text, secret: false, .. }
                if text == "broadcast"
        )));
        assert!(out
            .iter()
            .any(|event| matches!(event, Out::Grant { peer, .. } if *peer == first)));
        assert!(endpoint.waiting.contains_key(&first));
        let version = endpoint.source.version();
        let out = endpoint
            .on_link_up(late, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        assert!(out
            .iter()
            .any(|event| matches!(event, Out::Grant { peer, .. } if *peer == late)));
        assert!(endpoint.waiting.contains_key(&late));
        assert_eq!(endpoint.source.version(), version);
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn addressed_content_cannot_reappear_after_policy_change_and_late_join() {
        let (mut endpoint, root) = started_clipboard("addressed-lifetime").await;
        endpoint
            .on_local_copy(b"current".to_vec(), Vec::new())
            .await;
        let hash: [u8; 32] = cb::sha256(b"current").try_into().unwrap();
        assert!(!endpoint.address_carried([0; 32]));
        assert!(endpoint.carried.is_some());
        assert!(endpoint.address_carried(hash));
        endpoint.settings.routing_policy = config::ClipboardRoutingPolicy::Broadcast;
        let out = endpoint
            .on_link_up(
                [0x21; 32].into(),
                [1; 16],
                true,
                "127.0.0.1".parse().unwrap(),
            )
            .await;
        assert!(out.is_empty());
        assert!(endpoint.payload.is_empty());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn addressed_carry_and_history_send_target_only_one_peer() {
        let (mut endpoint, root) = started_clipboard("addressed").await;
        let target: NodeId = [0x21; 32].into();
        let other: NodeId = [0x22; 32].into();
        let incapable: NodeId = [0x23; 32].into();
        for (peer, capable) in [(target, true), (other, true), (incapable, false)] {
            endpoint
                .on_link_up(peer, [1; 16], capable, "127.0.0.1".parse().unwrap())
                .await;
        }
        endpoint
            .on_local_copy(b"current".to_vec(), Vec::new())
            .await;
        let (offer, hash) = endpoint.carried.clone().unwrap();
        assert!(endpoint.deliver_carried(target, [0; 32]).await.is_err());
        assert!(endpoint.deliver_carried(incapable, hash).await.is_err());
        let out = endpoint
            .deliver_carried(target, hash)
            .await
            .expect("addressed carry");
        assert!(out.iter().any(|event| matches!(event,
            Out::Delivery { peer, version, sha256 } if *peer == target && *version > offer.version && *sha256 == hash
        )));
        assert!(endpoint.source.version() > offer.version);
        assert_eq!(endpoint.waiting.len(), 1);
        assert_eq!(&*endpoint.waiting[&target].payload, b"current");
        assert!(!endpoint.waiting.contains_key(&other));
        endpoint.history_to_agent(
            Arc::new(b"local history".to_vec()),
            cb::sha256(b"local history").try_into().unwrap(),
        );
        assert_eq!(&*endpoint.payload, b"current");
        assert_eq!(endpoint.carried, Some((offer.clone(), hash)));
        assert!(endpoint.source.applied_from_peer());
        let out = endpoint
            .send_text_to(target, "old history")
            .await
            .expect("history send");
        assert!(out.iter().any(|event| matches!(event,
            Out::Delivery { peer, version, .. } if *peer == target && *version > offer.version
        )));
        assert_eq!(endpoint.waiting.len(), 1);
        assert_eq!(&*endpoint.waiting[&target].payload, b"old history");
        assert_eq!(&*endpoint.payload, b"current");
        assert_eq!(endpoint.carried, Some((offer.clone(), hash)));
        assert!(endpoint.source.applied_from_peer());
        let history_version = endpoint.source.version();
        let out = endpoint
            .deliver_carried(target, hash)
            .await
            .expect("carry after old history");
        assert!(out.iter().any(|event| matches!(event,
            Out::Delivery { peer, version, sha256 }
                if *peer == target && *version > history_version && *sha256 == hash
        )));
        assert_eq!(&*endpoint.waiting[&target].payload, b"current");
        assert_eq!(endpoint.carried, Some((offer, hash)));
        assert!(endpoint.source.applied_from_peer());
        endpoint.on_link_down(target).await;
        assert!(endpoint.send_text_to(target, "offline").await.is_err());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn secret_and_invalid_copies_clear_carry_and_queued_payloads() {
        let (mut endpoint, root) = started_clipboard("secret-carry").await;
        endpoint.settings.policy.skip_secret_marked = false;
        let peer: NodeId = [0x21; 32].into();
        endpoint
            .on_link_up(peer, [1; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        for secret in [true, false] {
            endpoint.on_local_copy(b"safe".to_vec(), Vec::new()).await;
            let hash = endpoint.carried.as_ref().unwrap().1;
            endpoint.deliver_carried(peer, hash).await.unwrap();
            let (payload, markers) = if secret {
                (b"password".to_vec(), vec![cb::Marker::ExcludeFromMonitor])
            } else {
                (vec![0xff], Vec::new())
            };
            let out = endpoint.on_local_copy(payload, markers).await;
            assert!(out.iter().any(|event| matches!(event, Out::CarryCleared)));
            assert!(!out.iter().any(|event| matches!(
                event,
                Out::Carried { .. }
                    | Out::History { .. }
                    | Out::Grant { .. }
                    | Out::Delivery { .. }
            )));
            assert!(endpoint.carried.is_none());
            assert!(endpoint.payload.is_empty());
            assert!(endpoint.waiting.is_empty());
            assert!(endpoint.offered.is_empty());
            assert!(endpoint.deliver_carried(peer, hash).await.is_err());
        }
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn refused_agent_frames_invalidate_carry_before_any_payload_is_stored() {
        let (mut endpoint, root) = started_clipboard("refused-agent-copy").await;
        endpoint.settings.policy.images = true;
        endpoint.settings.policy.max_image_bytes = cb::MAX_IMAGE_BYTES;
        endpoint.local_image_support = true;
        for mode in 0..6 {
            endpoint
                .on_local_copy(b"previous carry".to_vec(), Vec::new())
                .await;
            let mut changed = ipc::ClipboardChanged {
                origin_version: 100 + mode,
                chunk_count: 1,
                size_bytes: 3,
                data: b"new".to_vec(),
                sha256: cb::sha256(b"new"),
                ..Default::default()
            };
            match mode {
                // An agent-side read refusal or a copy beyond the IPC framing ceiling.
                0 => {
                    changed = ipc::ClipboardChanged {
                        origin_version: changed.origin_version,
                        ..Default::default()
                    }
                }
                1 => changed.sha256.clear(),
                2 => changed.markers.push("kde_secret".to_owned()),
                // Even the first chunk of a new copy makes the previous carry obsolete.
                3 => {
                    changed.chunk_count = 2;
                    changed.size_bytes = 6;
                    changed.sha256.clear();
                }
                4 => {
                    changed.chunk_count = 48;
                    changed.size_bytes = 1536 * 1024;
                    changed.data = vec![b'x'; 32 * 1024];
                    changed.sha256.clear();
                }
                5 => {
                    changed.mime = cb::IMAGE_MIME.to_owned();
                    changed.chunk_count = 1024;
                    changed.size_bytes = cb::MAX_IMAGE_BYTES + 1;
                    changed.data = b"\x89PNG\r\n\x1a\n".to_vec();
                    changed.sha256.clear();
                }
                _ => unreachable!(),
            }
            let out = endpoint.on_agent_frame(&changed).await;
            assert!(out.iter().any(|event| matches!(event, Out::CarryCleared)));
            assert!(!out.iter().any(|event| matches!(
                event,
                Out::Carried { .. } | Out::History { .. } | Out::Delivery { .. }
            )));
            assert!(endpoint.carried.is_none());
            assert!(endpoint.payload.is_empty());
            if mode == 4 {
                // The asymmetric 2 MiB report is framed successfully, then core rejects it.
                let full_text = "x".repeat(changed.size_bytes as usize);
                let digest = cb::sha256(full_text.as_bytes());
                for index in 1..changed.chunk_count {
                    changed.chunk_index = index;
                    changed.sha256 = if index + 1 == changed.chunk_count {
                        digest.clone()
                    } else {
                        Vec::new()
                    };
                    let out = endpoint.on_agent_frame(&changed).await;
                    assert!(!out.iter().any(|out| matches!(out, Out::Carried { .. })));
                }
                let event = tokio::time::timeout(
                    Duration::from_secs(2),
                    endpoint.test_events.as_mut().unwrap().recv(),
                )
                .await
                .expect("oversize report verification finishes")
                .expect("verification result");
                let Inbound::LocalPrepared {
                    generation,
                    prepared,
                } = event
                else {
                    panic!("expected local preparation");
                };
                assert!(matches!(&prepared, Err(cb::ClipboardError::TooLarge)));
                let out = endpoint.on_local_prepared(generation, prepared).await;
                assert!(!out.iter().any(|out| matches!(out, Out::Carried { .. })));
                assert!(endpoint.carried.is_none());
                assert!(endpoint.payload.is_empty());
            }
            if mode != 3 {
                assert!(endpoint.local.is_none());
            }
        }
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn sent_history_requires_successful_final_write_and_keeps_secret_flag() {
        for (secret, success) in [(false, true), (true, true), (true, false)] {
            let (mut endpoint, root) = started_clipboard("history-sent").await;
            let (events, mut received) = mpsc::channel(4);
            endpoint.runtime.events = events;
            let peer = [0x21; 32].into();
            let (tx, mut rx) = mpsc::channel::<QueuedMessage>(4);
            let reader = tokio::spawn(std::future::pending::<()>());
            let writer = tokio::spawn(std::future::pending::<()>());
            endpoint.runtime.inner.lock().await.conns.insert(
                peer,
                Conn {
                    generation: 1,
                    tx,
                    parent: [1; 16],
                    progress: test_progress(),
                    reader: reader.abort_handle(),
                    writer: writer.abort_handle(),
                    transfers: BTreeMap::new(),
                },
            );
            endpoint
                .runtime
                .send_transfer(
                    peer,
                    [0x41; 16],
                    (
                        1,
                        cb::sha256(b"text").try_into().unwrap(),
                        cb::ContentKind::Text,
                    ),
                    Arc::new(b"text".to_vec()),
                    cb::MAX_BYTES,
                    secret,
                )
                .await
                .expect("sender");
            let data = rx.recv().await.expect("data");
            assert!(matches!(data.message, ClipboardMessage::Data { .. }));
            data.written.expect("ack").send(true).expect("sender");
            let complete = rx.recv().await.expect("complete");
            assert!(matches!(
                complete.message,
                ClipboardMessage::Complete { .. }
            ));
            assert!(
                received.try_recv().is_err(),
                "enqueueing Complete is not completion"
            );
            complete
                .written
                .expect("ack")
                .send(success)
                .expect("sender");
            let event = tokio::time::timeout(Duration::from_millis(100), received.recv()).await;
            if success {
                assert!(
                    matches!(event, Ok(Some(Inbound::Sent { peer: sent_peer, history: Some((text, _)), secret: marked, .. }))
                    if sent_peer == peer && text == "text" && marked == secret)
                );
            } else {
                assert!(matches!(
                    event,
                    Ok(Some(Inbound::Failed { version: 1, .. }))
                ));
            }
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }
    #[test]
    fn history_copy_is_local_full_sized_and_monotonic() {
        let mut source = cb::Source::new([1; 32]);
        let text = "я".repeat(cb::MAX_BYTES as usize / 2);
        let payload = Arc::new(text.as_bytes().to_vec());
        let digest = cb::sha256(payload.as_ref()).try_into().unwrap();
        for expected_version in 1..=2 {
            let out = history_to_agent(&mut source, Arc::clone(&payload), digest);
            assert_eq!(out.len(), 1, "the bulk producer owns chunk allocation");
            let Out::AgentPayload {
                version,
                payload: received,
                sha256,
                ..
            } = &out[0]
            else {
                panic!("history copy must only queue prepared text for the local agent");
            };
            assert_eq!(*version, expected_version);
            assert!(Arc::ptr_eq(received, &payload));
            assert_eq!(*sha256, digest);
            assert!(
                source.applied_from_peer(),
                "history is never a network source"
            );
        }
    }

    #[tokio::test]
    async fn local_preparation_does_not_wait_for_the_payload_worker() {
        let (mut endpoint, root) = started_clipboard("local-preparation-liveness").await;
        let (events, mut received) = mpsc::channel(8);
        endpoint.runtime.events = events;
        let gate = Arc::clone(&endpoint.local_preparation.gate);
        let permit = gate
            .acquire_owned()
            .await
            .expect("hold the preparation worker");
        let changed = ipc::ClipboardChanged {
            origin_version: 1,
            chunk_count: 1,
            data: b"new copy".to_vec(),
            size_bytes: 8,
            sha256: cb::sha256(b"new copy"),
            ..Default::default()
        };
        let out = tokio::time::timeout(
            Duration::from_millis(100),
            endpoint.on_agent_frame(&changed),
        )
        .await
        .expect("session handler must not await preparation");
        assert!(out.iter().any(|out| matches!(out, Out::CarryCleared)));
        assert!(endpoint.carried.is_none());
        assert!(received.try_recv().is_err());
        drop(permit);
        let event = tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .expect("worker finishes")
            .expect("completion");
        let Inbound::LocalPrepared {
            generation,
            prepared,
        } = event
        else {
            panic!("expected local preparation");
        };
        let out = endpoint.on_local_prepared(generation, prepared).await;
        assert!(out.iter().any(|out| matches!(out, Out::Carried { .. })));
        assert_eq!(&*endpoint.payload, b"new copy");
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_local_preparation_cannot_restore_a_cleared_carry() {
        let (mut endpoint, root) = started_clipboard("stale-local-preparation").await;
        let generation = endpoint.local_generation;
        let prepared = tokio::task::spawn_blocking(|| {
            prepare_copy(
                vec![b"old copy".to_vec()],
                None,
                Vec::new(),
                cb::ContentKind::Text,
            )
        })
        .await
        .expect("prepare old copy");
        endpoint.clear_carry().await;
        assert!(endpoint
            .on_local_prepared(generation, prepared)
            .await
            .is_empty());
        assert!(endpoint.carried.is_none());
        assert!(endpoint.payload.is_empty());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn verified_received_history_and_apply_metadata_exclude_secrets() {
        for (policy, secret) in [
            (config::ClipboardRoutingPolicy::Follow, false),
            (config::ClipboardRoutingPolicy::Follow, true),
            (config::ClipboardRoutingPolicy::Broadcast, false),
            (config::ClipboardRoutingPolicy::Broadcast, true),
        ] {
            let (mut endpoint, root) = started_clipboard("history-received").await;
            endpoint.settings.policy.skip_secret_marked = false;
            endpoint.settings.routing_policy = policy;
            endpoint.on_local_copy(b"local".to_vec(), Vec::new()).await;
            let peer = [0x21; 32].into();
            let offer_id = [0x41; 16];
            endpoint
                .on_offer(
                    peer,
                    ClipboardMessage::Offer {
                        offer_id,
                        origin_node_id: *peer.as_bytes(),
                        origin_version: 1,
                        mime: cb::MIME.to_owned(),
                        size_bytes: 4,
                        secret_marked: secret,
                        sender_epoch: Vec::new(),
                        relayed: false,
                    },
                )
                .await;
            endpoint
                .on_peer_message(
                    peer,
                    ClipboardMessage::Data {
                        offer_id,
                        chunk_index: 0,
                        chunk_count: 1,
                        data: b"text".to_vec(),
                    },
                )
                .await;
            let out = endpoint
                .on_peer_message(
                    peer,
                    ClipboardMessage::Complete {
                        offer_id,
                        sha256: cb::sha256(b"text"),
                    },
                )
                .await;
            if secret {
                assert!(out.is_empty(), "secret offers are never assembled");
                assert!(endpoint.incoming.is_empty());
                endpoint.stop().await;
                let _ = std::fs::remove_dir_all(root);
                continue;
            }
            let job = endpoint.incoming[&offer_id].job;
            let out = verified_out(&mut endpoint, job).await;
            assert!(matches!(out.first(), Some(Out::ApplyPending {
                peer: sender, version: 1, sha256, kind: cb::ContentKind::Text,
            }) if *sender == peer && sha256.as_slice() == cb::sha256(b"text").as_slice()));
            assert!(out.iter().any(|event| matches!(event, Out::History {
                peer: sender, direction: ipc::ClipboardHistoryDirection::Received,
                text, secret: marked, ..
            } if *sender == peer && text == "text" && *marked == secret)));
            assert_eq!(
                endpoint.carried.is_some(),
                policy == config::ClipboardRoutingPolicy::Follow
            );
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn peer_cancel_stops_only_the_matching_paced_sender() {
        let (mut endpoint, root) = started_clipboard("paced-cancel").await;
        let peer: NodeId = [0x21; 32].into();
        let other: NodeId = [0x22; 32].into();
        let offer_id = [0x41; 16];
        let (tx, mut rx) = mpsc::channel(1);
        let reader = tokio::spawn(std::future::pending::<()>());
        let writer = tokio::spawn(std::future::pending::<()>());
        endpoint.runtime.inner.lock().await.conns.insert(
            peer,
            Conn {
                generation: 1,
                tx,
                parent: [1; 16],
                progress: test_progress(),
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
                transfers: BTreeMap::new(),
            },
        );
        let offer = ClipboardMessage::Offer {
            offer_id,
            origin_node_id: *peer.as_bytes(),
            origin_version: 1,
            mime: cb::MIME.to_owned(),
            size_bytes: (cb::CHUNK_BYTES * 3) as u64,
            secret_marked: false,
            sender_epoch: Vec::new(),
            relayed: false,
        };
        endpoint.offered.insert(
            peer,
            PendingOffer {
                message: offer.clone(),
                payload: Arc::new(Vec::new()),
                immediate: false,
            },
        );
        endpoint.waiting.insert(
            peer,
            PendingOffer {
                message: offer,
                payload: Arc::new(Vec::new()),
                immediate: false,
            },
        );
        endpoint
            .runtime
            .send_transfer(
                peer,
                offer_id,
                (
                    1,
                    cb::sha256(&vec![b'x'; cb::CHUNK_BYTES * 3])
                        .try_into()
                        .unwrap(),
                    cb::ContentKind::Text,
                ),
                vec![b'x'; cb::CHUNK_BYTES * 3].into(),
                cb::CHUNK_BYTES as u64 * 4,
                false,
            )
            .await
            .expect("sender starts");
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first chunk")
            .expect("channel");
        assert!(matches!(
            first.message,
            ClipboardMessage::Data { chunk_index: 0, .. }
        ));
        first
            .written
            .expect("write acknowledgement")
            .send(true)
            .expect("sender alive");
        for (sender, id) in [(other, offer_id), (peer, [0x42; 16])] {
            endpoint
                .on_peer_message(
                    sender,
                    ClipboardMessage::Cancel {
                        offer_id: id,
                        reason: protocol::wire::ErrorCode::ClipboardSuperseded,
                    },
                )
                .await;
            assert!(endpoint.offered.contains_key(&peer));
            assert!(endpoint.waiting.contains_key(&peer));
            assert!(endpoint.runtime.inner.lock().await.conns[&peer]
                .transfers
                .contains_key(&offer_id));
        }
        endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Cancel {
                    offer_id,
                    reason: protocol::wire::ErrorCode::ClipboardSuperseded,
                },
            )
            .await;
        assert!(!endpoint.offered.contains_key(&peer));
        assert!(!endpoint.waiting.contains_key(&peer));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .is_err(),
            "cancelled sender must not enqueue another chunk or Complete"
        );
        endpoint
            .runtime
            .send_transfer(
                peer,
                offer_id,
                (
                    1,
                    cb::sha256(&vec![b'x'; cb::CHUNK_BYTES * 3])
                        .try_into()
                        .unwrap(),
                    cb::ContentKind::Text,
                ),
                vec![b'x'; cb::CHUNK_BYTES * 3].into(),
                1,
                false,
            )
            .await
            .expect("another paced transfer");
        endpoint.runtime.close(peer).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("revocation aborts the paced sender")
            .is_none());
        endpoint.stop().await;
        assert!(reader
            .await
            .expect_err("reader aborted on revoke")
            .is_cancelled());
        assert!(writer
            .await
            .expect_err("writer aborted on revoke")
            .is_cancelled());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn every_local_copy_cancels_each_incoming_offer_even_when_refused() {
        for mode in 0..4 {
            let (mut endpoint, root) = started_clipboard("cancel-copy").await;
            let peer: NodeId = [0x21; 32].into();
            let first = [0x41; 16];
            let second = [0x42; 16];
            offer_and_data(&mut endpoint, peer, first, b"old").await;
            assert!(endpoint
                .on_offer(
                    peer,
                    ClipboardMessage::Offer {
                        offer_id: second,
                        origin_node_id: *peer.as_bytes(),
                        origin_version: 10,
                        mime: cb::MIME.to_owned(),
                        size_bytes: 5,
                        secret_marked: false,
                        sender_epoch: Vec::new(),
                        relayed: false,
                    }
                )
                .await
                .is_empty());
            assert_eq!(endpoint.incoming.len(), 2);
            if mode == 2 {
                endpoint.settings.enabled = false;
                endpoint.settings.policy.enabled = false;
            }
            if mode == 3 {
                endpoint.settings.policy.max_bytes = 1;
            }
            let markers = if mode == 1 {
                vec![cb::Marker::ExcludeFromMonitor]
            } else {
                Vec::new()
            };
            let out = endpoint
                .on_local_copy(b"new local copy".to_vec(), markers)
                .await;
            for id in [first, second] {
                assert_eq!(out.iter().filter(|event| matches!(event,
                    Out::Cancel { peer: p, offer_id, reason: cb::ClipboardError::Superseded }
                        if *p == peer && *offer_id == id
                )).count(), 1, "mode {mode} must cancel each incoming offer once");
            }
            assert!(endpoint.incoming.is_empty());
            assert!(endpoint.pending_request.is_none());
            assert!(!out
                .iter()
                .any(|event| matches!(event, Out::AgentPayload { .. })));
            endpoint.stop().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test]
    async fn ten_start_stop_cycles_rebind_the_exact_port() {
        let (mut endpoint, root) = started_clipboard("stop-rebind").await;
        let address = endpoint.runtime.listen_address;
        let identity = Arc::clone(&endpoint.runtime.identity);
        let settings = endpoint.settings;
        for cycle in 0..10 {
            endpoint.stop().await;
            let (next, _events) = Clipboard::start(
                identity.node_id(),
                settings,
                Arc::clone(&identity),
                Arc::new(std::sync::RwLock::new(BTreeSet::new())),
                address,
                4,
            )
            .await
            .unwrap_or_else(|error| panic!("cycle {cycle} failed to rebind: {error}"));
            endpoint = next;
        }
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stopping_closes_a_silent_incoming_handshake() {
        use tokio::io::AsyncReadExt;
        let (endpoint, root) = started_clipboard("silent-accept-stop").await;
        let address = endpoint.runtime.listen_address;
        let mut silent = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        // Let the accept task spawn its TLS handshake; transport's own regression explicitly
        // asserts that pending handshakes exist before exercising the same shutdown method.
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_secs(1), endpoint.stop())
            .await
            .expect("stop must not wait for the TLS handshake deadline");
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), silent.read(&mut byte))
            .await
            .expect("silent accepted socket closes");
        assert!(matches!(read, Ok(0) | Err(_)));
        let rebound = tokio::net::TcpListener::bind(address)
            .await
            .expect("listener port released");
        drop(rebound);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stopping_aborts_a_silent_dial_and_refuses_late_registration() {
        let (endpoint, root) = started_clipboard("silent-stop").await;
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = silent.local_addr().expect("address");
        let worker = endpoint.runtime.clone();
        worker
            .dial([0x21; 32].into(), address, [1; 16], [2; 16], [3; 32], 4)
            .await;
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), silent.accept())
            .await
            .expect("dial starts")
            .expect("accept");
        // Read the ClientHello first: EOF must come from stop, not the dial timeout.
        use tokio::io::AsyncReadExt;
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).await.expect("client hello") > 0);
        tokio::time::timeout(Duration::from_secs(1), endpoint.stop())
            .await
            .expect("stop does not wait for the dial timeout");
        let read = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut bytes))
            .await
            .expect("dial socket closes");
        assert!(matches!(read, Ok(0) | Err(_)), "dial socket survived stop");
        let registered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&registered);
        worker.spawn(async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::task::yield_now().await;
        assert!(!registered.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A runtime whose grants are checked on the clock they were issued on (audit F5).
    ///
    /// `Handle::start` needs a listener, so the endpoint is bound to an ephemeral port of the
    /// loopback interface and dropped with the test.
    async fn test_handle() -> Handle {
        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-clock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(root))
                .expect("identity")
                .0,
        );
        let (handle, _events) = Handle::start(
            identity,
            Arc::new(std::sync::RwLock::new(std::collections::BTreeSet::new())),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard runtime starts");
        handle
    }

    /// Audit F5: the grant's expiry and the check that accepts the bind are on one clock.
    ///
    /// The two `Instant` origins used to differ (`Clipboard::epoch` stamped the expiry,
    /// `Handle::epoch` checked it), so the 60 s window was really `60 s - <startup delta>`
    /// and the two numbers were not comparable.
    #[tokio::test]
    async fn a_grant_expires_on_the_clock_that_accepts_it() {
        let handle = test_handle().await;
        let parent = [0x11u8; 16];
        let before = handle.now_ms();
        let (channel, token) = handle.issue(parent).await;
        let after = handle.now_ms();
        let expires = handle
            .grant_expiry_ms(&channel)
            .await
            .expect("the grant is open");
        let ttl = cb::CHANNEL_TTL_MS;
        assert!(
            (before.saturating_add(ttl)..=after.saturating_add(ttl)).contains(&expires),
            "the expiry must be stamped from the accepting clock ({before}..={after} + {ttl} ms), \
             got {expires}"
        );
        assert!(
            handle.accepts(&parent, &channel, &token, expires - 1).await,
            "one millisecond before the ttl the bind is accepted"
        );
        assert!(
            !handle.accepts(&parent, &channel, &token, expires + 1).await,
            "one millisecond after it is refused"
        );
    }

    /// R2: revocation must close the clipboard connection the peer already holds.
    ///
    /// `Handle::close` used to drop only the channel's sender, which ends the writer task but
    /// leaves the reader task holding half of the TLS stream (`tokio::io::split` keeps the
    /// socket open until both halves are gone), so a revoked peer kept its bound channel. The
    /// reader task is now aborted too, and the peer observes the connection close.
    #[tokio::test]
    async fn revoking_a_peer_closes_its_bound_channel() {
        bound_peer_observes_close(false).await;
    }

    #[tokio::test]
    async fn stopping_closes_a_bound_peers_channel() {
        bound_peer_observes_close(true).await;
    }

    async fn bound_peer_observes_close(stop: bool) {
        use std::collections::BTreeSet;
        use std::sync::RwLock;

        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-revoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let load = |name: &str| {
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                root.join(name),
            ))
            .expect("identity")
            .0
        };
        let identity_a = Arc::new(load("a"));
        let identity_b = load("b");
        let id_a = identity_a.node_id();
        let id_b = identity_b.node_id();

        let a_pins = Arc::new(RwLock::new(BTreeSet::from([id_b])));
        let (handle, mut events) = Handle::start(
            Arc::clone(&identity_a),
            Arc::clone(&a_pins),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard runtime starts");

        let parent = [7u8; 16];
        let (channel, token) = handle.issue(parent).await;
        let address = handle.listen_address;
        let b_pins = Arc::new(RwLock::new(BTreeSet::from([id_a])));
        let dialer = tokio::spawn(async move {
            dial_clipboard(&identity_b, b_pins, id_a, address, parent, channel, token).await
        });

        // The runtime binds the channel before the test revokes it.
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if matches!(event, Inbound::Bound { peer, .. } if peer == id_b) {
                    return;
                }
            }
            panic!("the clipboard runtime stopped before the channel bound");
        })
        .await
        .expect("the peer's channel must bind");

        if stop {
            handle.stop().await;
        } else {
            handle.close(id_b).await;
        }

        let connection = dialer
            .await
            .expect("the dial task")
            .expect("the dial succeeded");
        let (mut reader, _writer) = connection.into_split();
        let closed = tokio::time::timeout(Duration::from_secs(5), reader.recv()).await;
        assert!(
            matches!(closed, Ok(Err(_))),
            "a revoked peer's bound clipboard channel must be closed, got {closed:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// R5: a local copy must cancel the incoming transfer, or a late `Complete` overwrites it.
    ///
    /// `on_local_copy` used to clear only `offered` (the outgoing side), leaving `incoming`
    /// and `pending_request` alive: the peer's `ClipboardComplete` then called
    /// `deliver_to_agent`, replacing the newer local copy and handing the stale payload to the
    /// agent.
    #[tokio::test]
    async fn a_local_copy_cancels_the_incoming_transfer_before_a_late_complete() {
        use std::collections::BTreeSet;
        use std::sync::RwLock;

        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-local-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(&root))
                .expect("identity")
                .0,
        );
        let settings = Settings {
            enabled: true,
            policy: cb::Policy {
                enabled: true,
                skip_secret_marked: true,
                max_bytes: cb::MAX_BYTES,
                images: false,
                max_image_bytes: cb::DEFAULT_MAX_IMAGE_BYTES,
            },
            routing_policy: config::ClipboardRoutingPolicy::Follow,
            settle_ms: 700,
            rate_bytes_per_s: 0,
            image_rate_bytes_per_s: cb::DEFAULT_IMAGE_RATE_BYTES_PER_S,
            // `transfer = "on_focus_enter"`: the offer is held, which is the deferred path.
            eager: false,
        };
        let (mut clipboard, _events) = Clipboard::start(
            identity.node_id(),
            settings,
            identity,
            Arc::new(RwLock::new(BTreeSet::new())),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard endpoint starts");

        let peer: NodeId = [0x21u8; 32].into();
        let origin = *peer.as_bytes();
        let offer_id = [0x41u8; 16];
        let remote = b"the peer's payload".to_vec();
        let size = remote.len() as u64;

        // The offer arrives and is held for focus.
        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Offer {
                    offer_id,
                    origin_node_id: origin,
                    origin_version: 9,
                    mime: cb::MIME.to_owned(),
                    size_bytes: size,
                    secret_marked: false,
                    sender_epoch: Vec::new(),
                    relayed: false,
                },
            )
            .await;
        assert!(out.is_empty());
        assert!(clipboard.pending_request.is_some(), "the offer is held");

        // Its data is already in flight.
        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Data {
                    offer_id,
                    chunk_index: 0,
                    chunk_count: 1,
                    data: remote.clone(),
                },
            )
            .await;
        assert!(out.is_empty());

        // The user copies something newer on this node.
        let local = b"a newer local copy".to_vec();
        let _ = clipboard.on_local_copy(local.clone(), Vec::new()).await;

        // The peer's `Complete` arrives late.
        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id,
                    sha256: cb::sha256(&remote),
                },
            )
            .await;
        assert!(
            out.iter()
                .all(|out| !matches!(out, Out::AgentPayload { .. })),
            "a superseded incoming transfer must not reach the agent"
        );
        assert_eq!(
            &*clipboard.payload,
            local.as_slice(),
            "the newer local copy must not be overwritten"
        );
        assert!(
            clipboard.incoming.is_empty(),
            "the incoming transfer is gone"
        );
        assert!(clipboard.pending_request.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    async fn started_clipboard(tag: &str) -> (Clipboard, std::path::PathBuf) {
        use std::collections::BTreeSet;
        use std::sync::RwLock;
        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(&root))
                .expect("identity")
                .0,
        );
        let settings = Settings {
            enabled: true,
            policy: cb::Policy {
                enabled: true,
                skip_secret_marked: true,
                max_bytes: cb::MAX_BYTES,
                images: false,
                max_image_bytes: cb::DEFAULT_MAX_IMAGE_BYTES,
            },
            routing_policy: config::ClipboardRoutingPolicy::Follow,
            settle_ms: 700,
            rate_bytes_per_s: 0,
            image_rate_bytes_per_s: cb::DEFAULT_IMAGE_RATE_BYTES_PER_S,
            eager: false,
        };
        let (mut clipboard, events) = Clipboard::start(
            identity.node_id(),
            settings,
            identity,
            Arc::new(RwLock::new(BTreeSet::new())),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard endpoint starts");
        clipboard.test_events = Some(events);
        (clipboard, root)
    }

    async fn offer_and_data(
        clipboard: &mut Clipboard,
        peer: NodeId,
        offer_id: [u8; 16],
        text: &[u8],
    ) {
        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Offer {
                    offer_id,
                    origin_node_id: *peer.as_bytes(),
                    origin_version: 9,
                    mime: cb::MIME.to_owned(),
                    size_bytes: text.len() as u64,
                    secret_marked: false,
                    sender_epoch: Vec::new(),
                    relayed: false,
                },
            )
            .await;
        assert!(out.is_empty());
        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Data {
                    offer_id,
                    chunk_index: 0,
                    chunk_count: 1,
                    data: text.to_vec(),
                },
            )
            .await;
        assert!(out.is_empty());
    }

    /// Review R2': the transfer a peer had in flight when it was revoked must not land in the
    /// clipboard after the revoke, even if its `Complete` was already queued.
    #[tokio::test]
    async fn a_revoked_peers_queued_complete_is_not_applied() {
        let (mut clipboard, root) = started_clipboard("revoked-queue").await;
        let peer: NodeId = [0x21u8; 32].into();
        let offer_id = [0x41u8; 16];
        let remote = b"from a peer the user just revoked".to_vec();
        offer_and_data(&mut clipboard, peer, offer_id, &remote).await;

        clipboard.on_link_down(peer).await;
        assert!(clipboard.incoming.is_empty(), "its transfers go with it");

        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id,
                    sha256: cb::sha256(&remote),
                },
            )
            .await;
        assert!(out
            .iter()
            .all(|out| !matches!(out, Out::AgentPayload { .. })));
        // A frame the dead connection had queued must not start a new transfer either.
        offer_and_data(&mut clipboard, peer, [0x42u8; 16], &remote).await;
        assert!(
            clipboard.incoming.is_empty(),
            "nothing is accepted from a closed peer"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review R3': a local copy the policy refuses (a password) still supersedes the wire:
    /// the peer's late `Complete` must not overwrite what the user just copied.
    #[tokio::test]
    async fn a_refused_local_copy_still_cancels_the_incoming_transfer() {
        let (mut clipboard, root) = started_clipboard("refused-copy").await;
        let peer: NodeId = [0x21u8; 32].into();
        let offer_id = [0x41u8; 16];
        let remote = b"the peer's payload".to_vec();
        offer_and_data(&mut clipboard, peer, offer_id, &remote).await;

        let _ = clipboard
            .on_local_copy(b"hunter2".to_vec(), vec![cb::Marker::ExcludeFromMonitor])
            .await;

        let out = clipboard
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id,
                    sha256: cb::sha256(&remote),
                },
            )
            .await;
        assert!(
            out.iter()
                .all(|out| !matches!(out, Out::AgentPayload { .. })),
            "the password the user copied must not be overwritten"
        );
        assert!(clipboard.incoming.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review R1': two connections of one peer (both sides dialled) must both die on revoke,
    /// and the older one's exit must not unregister the newer one.
    #[tokio::test]
    async fn revoking_a_peer_closes_every_connection_it_holds() {
        use std::collections::BTreeSet;
        use std::sync::RwLock;

        let root = std::env::temp_dir().join(format!(
            "lws-clipboard-two-conn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let load = |name: &str| {
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                root.join(name),
            ))
            .expect("identity")
            .0
        };
        let identity_a = Arc::new(load("a"));
        let identity_b = Arc::new(load("b"));
        let id_a = identity_a.node_id();
        let id_b = identity_b.node_id();
        let a_pins = Arc::new(RwLock::new(BTreeSet::from([id_b])));
        let (handle, mut events) = Handle::start(
            Arc::clone(&identity_a),
            Arc::clone(&a_pins),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard runtime starts");
        let address = handle.listen_address;
        let b_pins = Arc::new(RwLock::new(BTreeSet::from([id_a])));

        let mut dialers = Vec::new();
        for parent in [[7u8; 16], [8u8; 16]] {
            let (channel, token) = handle.issue(parent).await;
            let identity_b = Arc::clone(&identity_b);
            let b_pins = Arc::clone(&b_pins);
            dialers.push(tokio::spawn(async move {
                dial_clipboard(&identity_b, b_pins, id_a, address, parent, channel, token).await
            }));
        }
        let mut bound = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if matches!(event, Inbound::Bound { peer, .. } if peer == id_b) {
                    bound += 1;
                    if bound == 2 {
                        return;
                    }
                }
            }
        })
        .await
        .expect("both channels bind");

        handle.close(id_b).await;

        for dialer in dialers {
            let connection = dialer.await.expect("dial task").expect("dialled");
            let (mut reader, _writer) = connection.into_split();
            let closed = tokio::time::timeout(Duration::from_secs(3), reader.recv()).await;
            assert!(
                matches!(closed, Ok(Err(_))),
                "every connection of a revoked peer must close, got {closed:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Review R4': a peer that accepts the TCP connection and then says nothing must not stall
    /// the caller — `dial` runs on the session loop, where a stall freezes input, lease renewals
    /// and the GUI.
    #[tokio::test]
    async fn dialing_a_silent_peer_does_not_block_the_caller() {
        let handle = test_handle().await;
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a silent listener");
        let address = silent.local_addr().expect("its address");
        let accepter = tokio::spawn(async move {
            // Accept and hold the socket open without ever answering the TLS handshake.
            let held = silent.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(held);
        });
        let returned = tokio::time::timeout(
            Duration::from_millis(500),
            handle.dial(
                [0x55u8; 32].into(),
                address,
                [1u8; 16],
                [2u8; 16],
                [3u8; 32],
                4,
            ),
        )
        .await;
        assert!(
            returned.is_ok(),
            "dial must hand the work to a task and return"
        );
        accepter.abort();
    }

    #[test]
    fn every_marker_name_maps_to_its_marker() {
        // The names are the IPC contract (`docs/ipc.proto`, `ClipboardChanged.markers`), so a
        // rename on either side has to fail here rather than silently stop skipping secrets.
        let names: Vec<String> = MARKER_NAMES.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(
            markers_of(&names),
            vec![
                cb::Marker::ExcludeFromMonitor,
                cb::Marker::NoHistory,
                cb::Marker::ViewerIgnore,
                cb::Marker::KdeSecret,
                cb::Marker::PortalSecret,
            ]
        );
        assert!(markers_of(&["something_else".to_owned()]).is_empty());
    }

    #[test]
    fn refusals_carry_the_documented_wire_codes() {
        use protocol::wire::ErrorCode;
        assert_eq!(
            wire_error(cb::ClipboardError::Disabled),
            ErrorCode::ClipboardDisabled
        );
        assert_eq!(
            wire_error(cb::ClipboardError::TooLarge),
            ErrorCode::ClipboardTooLarge
        );
        assert_eq!(
            wire_error(cb::ClipboardError::Superseded),
            ErrorCode::ClipboardSuperseded
        );
        assert_eq!(ErrorCode::ClipboardSuperseded as i32, 26);
        for reason in [
            cb::ClipboardError::HashMismatch,
            cb::ClipboardError::Invalid,
            cb::ClipboardError::Secret,
        ] {
            assert_eq!(wire_error(reason), ErrorCode::InvalidFrame);
        }
    }

    fn image_offer(peer: NodeId, id: u8, size: u64) -> ClipboardMessage {
        ClipboardMessage::Offer {
            offer_id: [id; 16],
            origin_node_id: *peer.as_bytes(),
            origin_version: u64::from(id),
            mime: cb::IMAGE_MIME.to_owned(),
            size_bytes: size,
            secret_marked: false,
            sender_epoch: Vec::new(),
            relayed: false,
        }
    }

    async fn image_endpoint(tag: &str, peer: NodeId) -> (Clipboard, std::path::PathBuf) {
        let (mut endpoint, root) = started_clipboard(tag).await;
        endpoint.settings.policy.images = true;
        endpoint.settings.policy.max_image_bytes = cb::MAX_IMAGE_BYTES;
        endpoint.set_local_image_support(true).await;
        endpoint
            .on_link_up(peer, [1; 16], false, "127.0.0.1".parse().unwrap())
            .await;
        endpoint.set_peer_image_support(peer, true).await;
        (endpoint, root)
    }

    async fn finish_image(
        endpoint: &mut Clipboard,
        peer: NodeId,
        id: u8,
        bytes: &[u8],
    ) -> Verified {
        let count = bytes.len().div_ceil(cb::CHUNK_BYTES) as u32;
        for (index, chunk) in bytes.chunks(cb::CHUNK_BYTES).enumerate() {
            assert!(endpoint
                .on_peer_message(
                    peer,
                    ClipboardMessage::Data {
                        offer_id: [id; 16],
                        chunk_index: index as u32,
                        chunk_count: count,
                        data: chunk.to_vec(),
                    }
                )
                .await
                .is_empty());
        }
        endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id: [id; 16],
                    sha256: cb::sha256(bytes),
                },
            )
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = endpoint
                    .test_events
                    .as_mut()
                    .unwrap()
                    .recv()
                    .await
                    .expect("runtime open");
                if let Inbound::Verified(verified) = event {
                    return verified;
                }
            }
        })
        .await
        .expect("image verification finishes")
    }

    #[tokio::test]
    async fn poisoned_reservations_still_admit_and_release() {
        let (mut endpoint, root) = started_clipboard("poisoned-reservations").await;
        let reservations = Arc::clone(&endpoint.reservations);
        let poison = Arc::clone(&reservations);
        assert!(std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison the reservation mutex");
        })
        .join()
        .is_err());
        let peer = [0x21; 32].into();
        offer_and_data(&mut endpoint, peer, [3; 16], b"text").await;
        let job = endpoint.incoming[&[3; 16]].job;
        endpoint
            .on_peer_message(
                peer,
                ClipboardMessage::Complete {
                    offer_id: [3; 16],
                    sha256: cb::sha256(b"text"),
                },
            )
            .await;
        assert!(verified_out(&mut endpoint, job)
            .await
            .iter()
            .any(|out| matches!(out, Out::AgentPayload { .. })));
        assert!(reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_bytes_apply_without_history_and_malformed_png_is_refused() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("image-bytes", peer).await;
        let mut bytes = vec![0xff; cb::MAX_BYTES as usize + 1];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(endpoint
            .on_peer_message(peer, image_offer(peer, 1, bytes.len() as u64))
            .await
            .is_empty());
        let verified = finish_image(&mut endpoint, peer, 1, &bytes).await;
        let out = endpoint.on_verified(verified);
        assert!(out.iter().any(|out| matches!(out,
            Out::AgentPayload { kind: cb::ContentKind::Png, payload, .. } if payload.as_slice() == bytes.as_slice())));
        assert!(out
            .iter()
            .any(|out| matches!(out, Out::ApplyPending { .. })));
        assert!(out.iter().all(|out| !matches!(out, Out::History { .. })));
        assert!(endpoint.source.applied_from_peer());
        assert!(endpoint
            .on_peer_message(peer, image_offer(peer, 1, 8))
            .await
            .is_empty());
        assert!(
            endpoint.incoming.is_empty(),
            "accepted image versions suppress echoes"
        );
        assert!(endpoint
            .on_peer_message(peer, image_offer(peer, 2, 8))
            .await
            .is_empty());
        let malformed = finish_image(&mut endpoint, peer, 2, b"not PNG!").await;
        assert!(matches!(
            endpoint.on_verified(malformed).as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::Invalid,
                ..
            }]
        ));
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_admission_requires_all_three_permissions_and_configured_size() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("image-permissions", peer).await;
        for missing in 0..3 {
            endpoint.local_image_support = missing != 0;
            endpoint.settings.policy.images = missing != 1;
            endpoint.peers.get_mut(&peer).unwrap().image_clipboard = missing != 2;
            assert!(matches!(
                endpoint
                    .on_peer_message(peer, image_offer(peer, 1, 8))
                    .await
                    .as_slice(),
                [Out::Cancel {
                    reason: cb::ClipboardError::Disabled,
                    ..
                }]
            ));
            assert!(endpoint.incoming.is_empty());
            assert!(endpoint.reservations.lock().unwrap().is_empty());
        }
        endpoint.local_image_support = true;
        endpoint.settings.policy.images = true;
        endpoint.peers.get_mut(&peer).unwrap().image_clipboard = true;
        for maximum in [0, cb::DEFAULT_MAX_IMAGE_BYTES, cb::MAX_IMAGE_BYTES] {
            endpoint.settings.policy.max_image_bytes = maximum;
            assert!(matches!(
                endpoint
                    .on_peer_message(peer, image_offer(peer, 1, maximum + 1))
                    .await
                    .as_slice(),
                [Out::Cancel {
                    reason: cb::ClipboardError::TooLarge,
                    ..
                }]
            ));
        }
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_and_text_reservations_share_slots_but_not_byte_budgets() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("image-budget", peer).await;
        endpoint.peers.get_mut(&peer).unwrap().text_clipboard = true;
        for id in 1..=2 {
            assert!(endpoint
                .on_peer_message(peer, image_offer(peer, id, cb::MAX_IMAGE_BYTES))
                .await
                .is_empty());
        }
        assert!(matches!(
            endpoint
                .on_peer_message(peer, image_offer(peer, 3, 8))
                .await
                .as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::TooLarge,
                ..
            }]
        ));
        for id in 3..=8 {
            let mut message = image_offer(peer, id, if id <= 6 { cb::MAX_BYTES } else { 0 });
            if let ClipboardMessage::Offer { mime, .. } = &mut message {
                *mime = cb::MIME.to_owned();
            }
            assert!(endpoint.on_peer_message(peer, message).await.is_empty());
        }
        assert_eq!(endpoint.incoming.len(), 8);
        {
            let reservations = endpoint.reservations.lock().unwrap();
            assert_eq!(reservations.len(), 8);
            assert_eq!(
                reservations.values().map(|(_, size)| size).sum::<u64>(),
                68 * 1024 * 1024
            );
        }
        let mut ninth = image_offer(peer, 9, 0);
        if let ClipboardMessage::Offer { mime, .. } = &mut ninth {
            *mime = cb::MIME.to_owned();
        }
        assert!(matches!(
            endpoint.on_peer_message(peer, ninth).await.as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::TooLarge,
                ..
            }]
        ));
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stale_image_results_keep_reservations_until_actual_release() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("image-verification-budget", peer).await;
        endpoint
            .on_peer_message(peer, image_offer(peer, 1, 8))
            .await;
        let first = finish_image(&mut endpoint, peer, 1, b"\x89PNG\r\n\x1a\n").await;
        endpoint
            .on_peer_message(peer, image_offer(peer, 2, 8))
            .await;
        let second = finish_image(&mut endpoint, peer, 2, b"\x89PNG\r\n\x1a\n").await;
        endpoint.peers.get_mut(&peer).unwrap().text_clipboard = true;
        offer_and_data(&mut endpoint, peer, [9; 16], b"pending text").await;
        assert_eq!(endpoint.set_peer_image_support(peer, false).await.len(), 2);
        assert!(
            endpoint.incoming.contains_key(&[9; 16]),
            "peer image withdrawal preserves text"
        );
        assert_eq!(endpoint.pending_request, Some((peer, [9; 16])));
        endpoint.set_peer_image_support(peer, true).await;
        assert!(matches!(
            endpoint
                .on_peer_message(peer, image_offer(peer, 3, 8))
                .await
                .as_slice(),
            [Out::Cancel {
                reason: cb::ClipboardError::TooLarge,
                ..
            }]
        ));
        assert!(
            endpoint.on_verified(first).is_empty(),
            "withdrawn work cannot revive after re-enable"
        );
        assert!(endpoint
            .on_peer_message(peer, image_offer(peer, 3, 8))
            .await
            .is_empty());
        assert!(endpoint.on_verified(second).is_empty());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_local_carry_gates_old_peers_and_preserves_text_on_withdrawal() {
        let peer = [0x21; 32].into();
        let old_peer = [0x22; 32].into();
        let (mut endpoint, root) = image_endpoint("image-carry", peer).await;
        endpoint
            .on_link_up(old_peer, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        let prepared = prepare_copy(
            vec![b"\x89PNG\r\n\x1a\n".to_vec()],
            None,
            Vec::new(),
            cb::ContentKind::Png,
        )
        .unwrap();
        let hash = prepared.sha256;
        let out = endpoint.accept_local_copy(prepared).await;
        assert!(out.iter().any(|out| matches!(out, Out::Carried { .. })));
        assert!(out.iter().all(|out| !matches!(out, Out::History { .. })));
        assert!(endpoint.deliver_carried(old_peer, hash).await.is_err());
        assert!(!endpoint.waiting.contains_key(&old_peer));
        assert!(
            endpoint.deliver_carried(peer, hash).await.is_ok(),
            "cap15 alone supports images"
        );
        assert!(Arc::ptr_eq(
            &endpoint.waiting[&peer].payload,
            &endpoint.payload
        ));
        assert_eq!(endpoint.carried.as_ref().unwrap().0.mime, cb::IMAGE_MIME);
        endpoint
            .send_text_to(old_peer, "retained text")
            .await
            .unwrap();
        let out = endpoint.set_local_image_support(false).await;
        assert!(out.iter().any(|out| matches!(out, Out::CarryCleared)));
        assert!(!endpoint.waiting.contains_key(&peer));
        assert!(
            endpoint.waiting.contains_key(&old_peer),
            "text delivery survives image withdrawal"
        );
        assert!(endpoint.peers[&old_peer].parent.is_some());
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_consent_toggle_revokes_without_withdrawing_backend_support() {
        let peer = [0x21; 32].into();
        let text_peer = [0x22; 32].into();
        let (mut endpoint, root) = image_endpoint("image-consent", peer).await;
        endpoint
            .on_link_up(text_peer, [2; 16], true, "127.0.0.1".parse().unwrap())
            .await;
        let prepared = prepare_copy(
            vec![b"\x89PNG\r\n\x1a\n".to_vec()],
            None,
            Vec::new(),
            cb::ContentKind::Png,
        )
        .unwrap();
        let hash = prepared.sha256;
        endpoint.accept_local_copy(prepared).await;
        endpoint.deliver_carried(peer, hash).await.unwrap();
        endpoint
            .send_text_to(text_peer, "retained text")
            .await
            .unwrap();
        let out = endpoint.set_images(false).await;
        assert!(!endpoint.settings.policy.images);
        assert!(endpoint.local_image_support);
        assert!(endpoint.peers[&peer].image_clipboard);
        assert!(out.iter().any(|out| matches!(out, Out::CarryCleared)));
        assert!(!endpoint.waiting.contains_key(&peer));
        assert!(endpoint.waiting.contains_key(&text_peer));
        assert!(endpoint.carried.is_none());
        assert!(endpoint.set_images(true).await.is_empty());
        assert!(endpoint.settings.policy.images);
        assert!(
            endpoint.carried.is_none(),
            "re-enabling must wait for the next OS copy"
        );
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn local_image_ipc_pins_mime_count_size_and_secret_generation() {
        let peer = [0x21; 32].into();
        let (mut endpoint, root) = image_endpoint("image-ipc", peer).await;
        for (kind, count, allowed) in [
            (cb::ContentKind::Text, 65, false),
            (cb::ContentKind::Png, 1024, true),
            (cb::ContentKind::Png, 1025, false),
        ] {
            let message = ipc::ClipboardChanged {
                origin_version: u64::from(count),
                chunk_count: count,
                size_bytes: 8,
                mime: kind.mime().to_owned(),
                ..Default::default()
            };
            assert_eq!(endpoint.take_chunk(&message).is_ok(), allowed);
            endpoint.local = None;
        }
        for mismatch in 0..4 {
            let mut message = ipc::ClipboardChanged {
                origin_version: 1,
                chunk_count: 2,
                size_bytes: 8,
                mime: cb::IMAGE_MIME.to_owned(),
                ..Default::default()
            };
            assert!(endpoint.take_chunk(&message).unwrap().is_none());
            message.chunk_index = 1;
            message.data = b"\x89PNG\r\n\x1a\n".to_vec();
            message.sha256 = cb::sha256(&message.data);
            match mismatch {
                0 => message.mime = cb::MIME.to_owned(),
                1 => message.chunk_count = 3,
                2 => message.size_bytes = 9,
                _ => message.markers = vec!["kde_secret".to_owned()],
            }
            assert!(matches!(
                endpoint.take_chunk(&message),
                Err(cb::ClipboardError::Invalid)
            ));
        }
        for marker in [
            "exclude_from_monitor",
            "no_history",
            "viewer_ignore",
            "kde_secret",
            "portal_secret",
        ] {
            let generation = endpoint.local_generation;
            let prepared = prepare_copy(
                vec![b"\x89PNG\r\n\x1a\n".to_vec()],
                None,
                Vec::new(),
                cb::ContentKind::Png,
            );
            let secret = ipc::ClipboardChanged {
                origin_version: generation + 2,
                chunk_count: 1,
                size_bytes: 8,
                mime: cb::IMAGE_MIME.to_owned(),
                markers: vec![marker.to_owned()],
                ..Default::default()
            };
            endpoint.on_agent_frame(&secret).await;
            assert!(endpoint
                .on_local_prepared(generation, prepared)
                .await
                .is_empty());
            assert!(endpoint.carried.is_none());
            assert!(endpoint.local.is_none());
        }
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn image_success_reports_sent_identity_without_history_payload() {
        let (endpoint, root) = started_clipboard("image-sent").await;
        let peer = [0x21; 32].into();
        let (tx, mut rx) = mpsc::channel::<QueuedMessage>(4);
        let reader = tokio::spawn(std::future::pending::<()>());
        let writer = tokio::spawn(std::future::pending::<()>());
        endpoint.runtime.inner.lock().await.conns.insert(
            peer,
            Conn {
                generation: 1,
                tx,
                parent: [1; 16],
                progress: test_progress(),
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
                transfers: BTreeMap::new(),
            },
        );
        let bytes = Arc::new(b"\x89PNG\r\n\x1a\n".to_vec());
        let hash = cb::sha256(&bytes).try_into().unwrap();
        endpoint
            .runtime
            .send_transfer(
                peer,
                [1; 16],
                (7, hash, cb::ContentKind::Png),
                bytes,
                cb::DEFAULT_IMAGE_RATE_BYTES_PER_S,
                false,
            )
            .await
            .unwrap();
        for _ in 0..2 {
            rx.recv()
                .await
                .unwrap()
                .written
                .unwrap()
                .send(true)
                .unwrap();
        }
        let mut endpoint = endpoint;
        let event = tokio::time::timeout(
            Duration::from_secs(2),
            endpoint.test_events.as_mut().unwrap().recv(),
        )
        .await
        .unwrap();
        assert!(
            matches!(event, Some(Inbound::Sent { peer: sent_peer, version: 7,
            sha256, history: None, secret: false }) if sent_peer == peer && sha256 == hash)
        );
        endpoint.stop().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
