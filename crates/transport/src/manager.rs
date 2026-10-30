//! The link manager: one listener, one dialer per configured peer, mesh dedup.
//!
//! `docs/06-development-plan.md`, KVM-070: «Полный mesh среди членов: все узлы слушают
//! `47901`/`47902` и дозваниваются до каждого доверенного пира; одновременный dial
//! разрешается линком, инициированным большим `node_id`; … `Ping` каждую 1 с по каждому
//! линку, офлайн через 3 с без `Pong`; пятый член получает `MEMBERSHIP_FULL`».
//!
//! Everything the manager decides is delegated to [`crate::policy`] (pure) so the rules can
//! be read without a socket: the dedup winner, the reconnect schedule, the liveness
//! transitions and the membership gate.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use focus_router as fr;
use geometry::NodeId;
use identity::{peer_node_id, trust::TrustStore, NodeIdentity, TrustPolicy, TrustedPeer};
use protocol::{limits, wire};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};

use crate::convert::PeerInfo;
use crate::link::{
    Direction, LeaseTransportCounters, LeaseTransportSnapshot, LinkConfig, LinkDownReason,
    LinkEvent, LinkHandle, LinkReport, Stream,
};
use crate::pairing::{PairingConnection, PairingGate};
use crate::policy;
use crate::TransportError;

/// Bound unauthenticated work independently on each network listener.
pub(crate) const MAX_PENDING_HANDSHAKES: usize = 32;

/// Kept outside the accept future so cancellation by `select!` cannot skip a retry delay.
#[derive(Default)]
pub(crate) struct AcceptBackoff {
    retry_at: Option<tokio::time::Instant>,
    delay: Duration,
}

impl AcceptBackoff {
    fn failed(&mut self) {
        self.delay = if self.delay.is_zero() {
            Duration::from_millis(25)
        } else {
            (self.delay * 2).min(Duration::from_secs(1))
        };
        self.retry_at = Some(tokio::time::Instant::now() + self.delay);
    }
}

/// Resource errors are local to accepting: established links and manager messages stay live.
pub(crate) async fn accept_with_backoff<T, F, Fut>(mut accept: F, backoff: &mut AcceptBackoff) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    loop {
        if let Some(deadline) = backoff.retry_at {
            tokio::time::sleep_until(deadline).await;
        }
        match accept().await {
            Ok(connection) => {
                *backoff = AcceptBackoff::default();
                return connection;
            }
            Err(error) => {
                backoff.failed();
                tracing::warn!(kind = ?error.kind(), "listener accept failed; retrying with backoff");
            }
        }
    }
}

/// How a manager is configured.
pub struct ManagerConfig {
    /// This node's identity (keys and certificate).
    pub identity: Arc<NodeIdentity>,
    /// The active trust records; only these peers complete a handshake.
    pub trust: TrustStore,
    /// The `Hello` this node sends on every link.
    pub local_hello: wire::Hello,
    /// Where to listen (default `0.0.0.0:47901`, or port 0 in tests).
    pub listen: SocketAddr,
    /// Peers to dial.
    pub peers: Vec<SocketAddr>,
    /// `network.max_members` (capped at [`protocol::limits::N_MAX`]).
    pub max_members: u8,
    /// Bounded outbound queue capacity per link.
    pub queue_capacity: usize,
    /// The liveness cadence every link runs with: `workspace.link_ping_ms` /
    /// `workspace.link_timeout_ms` from the application's configuration.
    pub liveness: policy::LinkLiveness,
    /// The reconnect backoff bounds: `network.reconnect_backoff_ms` from the application's
    /// configuration.
    pub backoff: policy::BackoffBounds,
    /// The pairing window the listener may serve on `47901` by ALPN (`pair/1`); closed by
    /// default, so no unknown peer reaches the listener until the application opens it.
    pub pairing: PairingGate,
}

/// Result of an untrusted discovery hint; only `Verified` may update persisted addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryRoute {
    Verified,
    LinkPresent,
    Unchanged,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryRouteError {
    #[error("discovery peer is not trusted")]
    Untrusted,
    #[error("discovery identity mismatch: expected {expected}, got {actual}")]
    IdentityMismatch { expected: NodeId, actual: NodeId },
    #[error("discovery route handshake timed out")]
    Timeout,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// A monotonic millisecond clock.
///
/// `crates/transport` has no clock helper, and the pairing window is bounded in milliseconds
/// (`docs/05-protocol.md`: «окно сопряжения … на 120 s»), so the manager keeps its own
/// `Instant` origin — the same idiom `apps/bifrost-core/src/session.rs` uses. A monotonic origin
/// means the window never jumps with the wall clock.
#[derive(Debug, Clone, Copy)]
struct Clock {
    origin: Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    /// Milliseconds since the origin.
    fn now_ms(&self) -> u64 {
        Instant::now().duration_since(self.origin).as_millis() as u64
    }

    /// Nanoseconds since the origin, for formation ordering finer than the pairing clock.
    fn now_ns(&self) -> u128 {
        Instant::now().duration_since(self.origin).as_nanos()
    }
}

/// What the manager keeps per live link.
struct LiveLink {
    /// What the manager knows this link by (see [`LinkConfig::key`]).
    ///
    /// The map is keyed by peer, because the dedup keeps exactly one live link per peer; the
    /// key is what a report is matched against, and `(peer, direction)` cannot do that job: a
    /// peer that dials twice is two links in the same direction.
    key: [u8; 16],
    handle: LinkHandle,
    initiator: NodeId,
    /// When this link finished forming, on the manager's monotonic clock.
    ///
    /// A newcomer that registered before this instant dialled while this link was still
    /// forming — the simultaneous race [`policy::prefer_incoming`] exists for. A newcomer
    /// registered after it is a late redial and is refused instead.
    up_at_ns: u128,
    /// The link's task, so [`LinkManager::kill_all`] can drop it without a `Shutdown`.
    task: tokio::task::JoinHandle<()>,
}

/// The settings every link task needs: the outbound queue capacity, the liveness cadence
/// and the reconnect backoff bounds. One bundle instead of three more parameters, so the
/// task signatures stay readable.
#[derive(Debug, Clone, Copy)]
struct LinkSettings {
    queue_capacity: usize,
    liveness: policy::LinkLiveness,
    backoff: policy::BackoffBounds,
}

/// A registered-but-not-yet-established link.
struct PendingLink {
    handle: LinkHandle,
    initiator: NodeId,
    /// The capabilities in this link's handshake Hello, before any cache refresh.
    local_capabilities: Vec<i32>,
    task: tokio::task::JoinHandle<()>,
    /// The peer this link is for, so a report can be matched without the live map's key.
    peer: NodeId,
    /// When the manager registered it, on its monotonic clock.
    ///
    /// Compared against a live link's [`LiveLink::up_at_ns`] to tell a simultaneous dial (the
    /// newcomer was already forming) from a late redial (it started afterwards).
    registered_at_ns: u128,
    /// Discovery never participates in simultaneous-link replacement.
    replace_existing: bool,
}

/// Messages from the accept/dial tasks into the manager loop.
enum ManagerMessage {
    PeerRejected {
        peer: NodeId,
    },
    /// A link was spawned; the manager takes ownership of its handle.
    Register {
        key: [u8; 16],
        peer: NodeId,
        direction: Direction,
        local_capabilities: Vec<i32>,
        handle: LinkHandle,
        task: tokio::task::JoinHandle<()>,
    },
    /// A link's own report.
    Report(LinkReport),
    /// A dialer asks for its turn to dial `peer`: granted once no link to that peer is left.
    ///
    /// The dialer learns a peer's identity from the first handshake with it, so this is the
    /// only way it can know not to dial while that peer is already linked — the link may have
    /// been dialled by the peer, or by a link this dialer's own attempt lost.
    DialPermit {
        peer: NodeId,
        reply: oneshot::Sender<DialTurn>,
    },
    /// A freshly paired peer to trust and dial without restarting the manager.
    AddPeer {
        peer: NodeId,
        address: SocketAddr,
    },
    /// Stops the dialer created for a freshly paired peer.
    RemovePeer {
        peer: NodeId,
    },
    /// Moves only the listening socket after a local adapter address change.
    RebindListener {
        address: SocketAddr,
        reply: oneshot::Sender<Result<SocketAddr, TransportError>>,
    },
}

/// What a granted dial permit tells the waiting dialer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialTurn {
    /// The peer had no link at all: dial, and keep the reconnect backoff where it is.
    Free,
    /// The peer had a link that went away: a fresh start, retry promptly.
    Waited,
}

/// The mesh of links around this node.
pub struct LinkManager {
    identity: Arc<NodeIdentity>,
    /// The shared pins are read by rustls synchronously during every handshake.
    trusted_peers: Arc<StdRwLock<BTreeSet<NodeId>>>,
    /// Cached Hello; capability updates also reach established links.
    local_hello: Arc<RwLock<wire::Hello>>,
    listen: SocketAddr,
    peers: Vec<SocketAddr>,
    max_members: u8,
    pairing: PairingGate,
    settings: LinkSettings,
    messages_tx: mpsc::Sender<ManagerMessage>,
    /// Taken by [`LinkManager::run`]; behind a mutex so `run` can take `&self` and the
    /// manager can live behind an `Arc` while the application queries it.
    messages_rx: Mutex<mpsc::Receiver<ManagerMessage>>,
    events_tx: mpsc::Sender<LinkEvent>,
    inner: Arc<Mutex<Inner>>,
    pending_unpairs: Arc<Mutex<crate::unpair::PendingUnpairs>>,
    /// The monotonic origin the pairing window's millisecond deadline is measured against.
    clock: Clock,
}

#[derive(Default)]
struct Inner {
    links: BTreeMap<NodeId, LiveLink>,
    /// Only the counters survive link removal, for the session's final lease snapshot.
    /// Replaced once on the next established connection; never retains queued payloads.
    last_lease_diagnostics: BTreeMap<NodeId, Arc<LeaseTransportCounters>>,
    /// Keyed by the link's own key ([`LinkConfig::key`]), which is what makes it unique.
    ///
    /// `(peer, direction)` is not: two nodes that dial each other have two links between them
    /// at the same time *and* different directions, and a peer that dials twice has two links
    /// in the *same* direction — the late registration then clobbered the earlier one's
    /// handle, the manager dropped that link's `Up`, and the pair stayed split across two TCP
    /// connections instead of the one the dedup rule picked.
    pending: BTreeMap<[u8; 16], PendingLink>,
    /// The dialers [`LinkManager::run`] spawned, so [`LinkManager::kill_all`] can stop them.
    ///
    /// A crash takes the dialers with it: a dialer that outlives its links keeps re-dialling a
    /// peer that is gone, and every one of those dials is accepted by a survivor that still
    /// trusts it — the dead peer never looks dead.
    dialers: Vec<tokio::task::JoinHandle<()>>,
    /// Dynamic dialers are keyed by peer so re-pairing replaces the old loop.
    peer_dialers: BTreeMap<NodeId, tokio::task::JoinHandle<()>>,
    /// Only addresses authenticated by discovery; never populated by a beacon alone.
    discovered_routes: BTreeMap<NodeId, SocketAddr>,
    /// Dial permits not yet granted, per peer: the dialers waiting for that peer's link to go.
    ///
    /// This is what makes a dialer idle while its peer is linked. Without it, a dialer whose
    /// link lost the dedup looped forever: redial, get refused at `Up`, redial again — a TLS
    /// handshake every backoff, for the life of the mesh.
    dial_waiters: BTreeMap<NodeId, Vec<oneshot::Sender<DialTurn>>>,
}

impl Inner {
    fn discovery_decision(
        &self,
        pins: &BTreeSet<NodeId>,
        peer: NodeId,
        address: SocketAddr,
    ) -> Result<Option<DiscoveryRoute>, DiscoveryRouteError> {
        if !pins.contains(&peer) {
            return Err(DiscoveryRouteError::Untrusted);
        }
        if self.has_link_to(&peer) {
            return Ok(Some(DiscoveryRoute::LinkPresent));
        }
        if self.discovered_routes.get(&peer) == Some(&address) {
            return Ok(Some(DiscoveryRoute::Unchanged));
        }
        Ok(None)
    }

    fn lease_diagnostics(&self, peer: &NodeId) -> Option<LeaseTransportSnapshot> {
        self.links
            .get(peer)
            .map(|link| link.handle.lease_diagnostics())
            .or_else(|| {
                self.last_lease_diagnostics
                    .get(peer)
                    .map(|counters| counters.snapshot())
            })
    }

    fn lease_challenge_trace(
        &self,
        peer: &NodeId,
        lease_id: fr::LeaseId,
    ) -> Option<fr::LeaseTraceSnapshot> {
        self.links
            .get(peer)
            .map(|link| link.handle.lease_challenge_trace(lease_id))
            .or_else(|| {
                self.last_lease_diagnostics
                    .get(peer)
                    .map(|counters| counters.lease_challenge_trace(lease_id))
            })
    }

    /// Whether a link to `peer` exists, live or still forming.
    fn has_link_to(&self, peer: &NodeId) -> bool {
        self.links.contains_key(peer) || self.pending.values().any(|pending| pending.peer == *peer)
    }

    /// Grants every permit waiting on `peer` once no link to it remains.
    ///
    /// The check is the manager's state *at this instant*, so any transition that touches
    /// `peer` may call it — including the one that replaces a live link, where a live link
    /// exists again by the time the old one's removal is noticed.
    fn release_dial_waiters(&mut self, peer: NodeId) {
        if self.has_link_to(&peer) {
            return;
        }
        let Some(waiting) = self.dial_waiters.remove(&peer) else {
            return;
        };
        for reply in waiting {
            let _ = reply.send(DialTurn::Waited);
        }
    }
}

impl LinkManager {
    /// Builds a manager and the receiver of application events.
    pub fn new(config: ManagerConfig) -> Result<(Self, mpsc::Receiver<LinkEvent>), TransportError> {
        let ManagerConfig {
            identity,
            trust,
            local_hello,
            listen,
            peers,
            max_members,
            queue_capacity,
            liveness,
            backoff,
            pairing,
        } = config;
        let trusted_peers = Arc::new(StdRwLock::new(trust.pinned()));
        let local_hello = Arc::new(RwLock::new(local_hello));
        let settings = LinkSettings {
            queue_capacity,
            liveness,
            backoff,
        };
        let (events_tx, events_rx) = mpsc::channel(256);
        let (messages_tx, messages_rx) = mpsc::channel(256);
        Ok((
            Self {
                identity,
                trusted_peers,
                local_hello,
                listen,
                peers,
                max_members,
                pairing,
                settings,
                messages_tx,
                messages_rx: Mutex::new(messages_rx),
                events_tx,
                inner: Arc::new(Mutex::new(Inner::default())),
                pending_unpairs: Arc::new(Mutex::new(crate::unpair::PendingUnpairs::default())),
                clock: Clock::new(),
            },
            events_rx,
        ))
    }

    /// Loads bounded persisted notify-only pins before the listener starts.
    pub async fn load_pending_unpairs(&self, state_path: &Path) -> Result<(), TransportError> {
        let mut pending =
            crate::unpair::PendingUnpairs::load(state_path, crate::unpair::now_secs())?;
        let pins = self
            .trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        pending.discard_trusted(&pins)?;
        *self.pending_unpairs.lock().await = pending;
        Ok(())
    }

    /// Returns unexpired pending notices without granting membership.
    pub async fn pending_unpairs(&self) -> Vec<crate::PendingUnpairNotice> {
        self.pending_unpairs
            .lock()
            .await
            .records(crate::unpair::now_secs())
    }

    /// Persists the captured SPKI pin; remove_peer sends only after trust was saved.
    pub async fn queue_unpair(
        &self,
        peer: NodeId,
        state_path: &Path,
    ) -> Result<(), TransportError> {
        let created = crate::unpair::now_secs();
        if !self
            .trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&peer)
        {
            return Err(TransportError::Protocol(
                "cannot queue a notice for an untrusted peer".into(),
            ));
        }
        self.pending_unpairs
            .lock()
            .await
            .queue(peer, state_path, created)
    }

    async fn flush_unpair(&self, peer: NodeId, created: u64) {
        if let Some(handle) = self.handle(&peer).await {
            let sent = tokio::time::timeout(
                Duration::from_millis(self.settings.liveness.offline_ms.max(1)),
                handle.send_unpaired(crate::unpair::notice(self.identity.node_id())),
            )
            .await;
            if matches!(sent, Ok(Ok(()))) {
                if let Err(error) = self.pending_unpairs.lock().await.complete(peer, created) {
                    tracing::warn!(%error, "could not persist completed unpair notice");
                }
            }
        }
    }

    /// Adds the newly committed trust pin and starts dialing its recorded address.
    pub async fn add_peer(&self, peer: TrustedPeer) -> Result<(), TransportError> {
        let raw_address = peer.last_address.as_deref().ok_or_else(|| {
            TransportError::InvalidPeerAddress("the paired peer has no address".to_owned())
        })?;
        let address = raw_address.parse::<SocketAddr>().map_err(|error| {
            TransportError::InvalidPeerAddress(format!("{raw_address}: {error}"))
        })?;
        let pending = self
            .pending_unpairs
            .lock()
            .await
            .records(crate::unpair::now_secs())
            .into_iter()
            .find(|notice| notice.peer == peer.node_id);
        if let Some(notice) = pending {
            self.pending_unpairs
                .lock()
                .await
                .complete(notice.peer, notice.created_unix_secs)?;
        }
        let peer_id = peer.node_id;
        self.trusted_peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(peer_id);
        if self
            .messages_tx
            .send(ManagerMessage::AddPeer {
                peer: peer_id,
                address,
            })
            .await
            .is_err()
        {
            self.remove_pin(&peer_id);
            return Err(TransportError::LinkClosed);
        }
        Ok(())
    }

    /// Probes one candidate using pinned TLS, requiring the exact advertised identity.
    ///
    /// Does not add pins or trust. The caller rate-limits hints and persists `last_address`
    /// only for `Verified`. Connect + TLS share the normal dial deadline; no attempt is
    /// spawned until authentication succeeds. A concurrent link always wins.
    pub async fn refresh_discovered_route(
        &self,
        expected: NodeId,
        candidate: SocketAddr,
    ) -> Result<DiscoveryRoute, DiscoveryRouteError> {
        if let Some(outcome) = self.discovery_decision(expected, candidate).await? {
            return Ok(outcome);
        }
        let config = identity::client_config(
            &self.identity,
            TrustPolicy::PinnedShared(Arc::clone(&self.trusted_peers)),
            &identity::LINK_ALPN,
        )
        .map_err(TransportError::from)?;
        let connector = TlsConnector::from(Arc::new(config));
        let stream = tokio::time::timeout(
            Duration::from_millis(self.settings.liveness.offline_ms),
            async {
                let tcp = TcpStream::connect(candidate).await?;
                tcp.set_nodelay(true)?;
                connector
                    .connect(identity::server_name(candidate.ip()), tcp)
                    .await
            },
        )
        .await
        .map_err(|_| DiscoveryRouteError::Timeout)?
        .map_err(TransportError::from)?;
        let actual = peer_node_id(stream.get_ref().1.peer_certificates().unwrap_or_default())
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        if actual != expected {
            return Err(DiscoveryRouteError::IdentityMismatch { expected, actual });
        }
        let hello = self.local_hello.read().await.clone();
        let key = identity::random16().map_err(TransportError::from)?;
        let mut inner = self.inner.lock().await;
        // Keep the trust read guard through registration, closing the revocation race.
        let pins = self
            .trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(outcome) = inner.discovery_decision(&pins, expected, candidate)? {
            return Ok(outcome);
        }
        let (reports, mut reports_rx) = mpsc::channel(64);
        let local_capabilities = hello.capabilities.clone();
        let link = crate::link::spawn(LinkConfig {
            key,
            peer: expected,
            local: self.identity.node_id(),
            direction: Direction::Outgoing,
            local_hello: hello,
            stream: Stream::Client(Box::new(stream)),
            reports,
            queue_capacity: self.settings.queue_capacity,
            liveness: self.settings.liveness,
        })?;
        inner.pending.insert(
            key,
            PendingLink {
                handle: link.handle,
                initiator: self.identity.node_id(),
                local_capabilities,
                task: link.task,
                peer: expected,
                registered_at_ns: self.clock.now_ns(),
                replace_existing: false,
            },
        );
        inner.discovered_routes.insert(expected, candidate);
        let dialer = self.spawn_peer_dialer(expected, candidate);
        if let Some(previous) = inner.peer_dialers.insert(expected, dialer) {
            previous.abort();
        }
        let messages = self.messages_tx.clone();
        tokio::spawn(async move {
            while let Some(report) = reports_rx.recv().await {
                if messages.send(ManagerMessage::Report(report)).await.is_err() {
                    return;
                }
            }
        });
        Ok(DiscoveryRoute::Verified)
    }

    async fn discovery_decision(
        &self,
        peer: NodeId,
        address: SocketAddr,
    ) -> Result<Option<DiscoveryRoute>, DiscoveryRouteError> {
        let inner = self.inner.lock().await;
        let pins = self
            .trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.discovery_decision(&pins, peer, address)
    }

    fn spawn_peer_dialer(&self, peer: NodeId, address: SocketAddr) -> tokio::task::JoinHandle<()> {
        let messages = self.messages_tx.clone();
        let identity = Arc::clone(&self.identity);
        let trusted_peers = Arc::clone(&self.trusted_peers);
        let local_hello = Arc::clone(&self.local_hello);
        let settings = self.settings;
        tokio::spawn(async move {
            dial_loop(
                address,
                Some(peer),
                identity,
                trusted_peers,
                local_hello,
                messages,
                settings,
            )
            .await;
        })
    }

    /// Removes a peer from the live verifier and stops its dynamic dialer.
    pub async fn remove_peer(&self, peer: &NodeId) -> Result<(), TransportError> {
        let pending = self
            .pending_unpairs
            .lock()
            .await
            .records(crate::unpair::now_secs())
            .into_iter()
            .find(|notice| notice.peer == *peer);
        if let Some(notice) = pending {
            self.flush_unpair(*peer, notice.created_unix_secs).await;
        }
        self.remove_pin(peer);
        self.messages_tx
            .send(ManagerMessage::RemovePeer { peer: *peer })
            .await
            .map_err(|_| TransportError::LinkClosed)
    }

    fn remove_pin(&self, peer: &NodeId) {
        self.trusted_peers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(peer);
    }

    /// The shared pin set read by TLS verifiers.
    pub fn shared_pins(&self) -> Arc<std::sync::RwLock<BTreeSet<NodeId>>> {
        Arc::clone(&self.trusted_peers)
    }

    /// Replaces the cached Hello and publishes changed capabilities on established links.
    pub async fn update_local_hello(&self, hello: wire::Hello) {
        // Keep this guard through enqueueing: Up catch-up takes the same lock first,
        // so an older snapshot can never be queued after a newer update.
        let mut current = self.local_hello.write().await;
        let changed = current.capabilities != hello.capabilities;
        *current = hello;
        if changed {
            let inner = self.inner.lock().await;
            for link in inner.links.values() {
                let _ = link
                    .handle
                    .send_capabilities(current.capabilities.clone())
                    .await;
            }
        }
    }

    /// The handle to `peer`, when a link to it is up.
    pub async fn handle(&self, peer: &NodeId) -> Option<LinkHandle> {
        self.inner
            .lock()
            .await
            .links
            .get(peer)
            .map(|link| link.handle.clone())
    }

    /// Samples cumulative lease counters for the current or last established connection.
    ///
    /// Counters survive link removal so final lease diagnostics can include link loss.
    /// Returns `None` until a connection has established; the next established connection
    /// resets the counters. Sampling does not clone a handle or allocate.
    pub async fn lease_diagnostics(&self, peer: &NodeId) -> Option<LeaseTransportSnapshot> {
        self.inner.lock().await.lease_diagnostics(peer)
    }

    /// Final bounded lease/nonce trace for the current or last established connection.
    ///
    /// Stage times are ages in milliseconds at sampling, not absolute timestamps.
    /// Survives disconnect until the next established connection replaces diagnostics.
    /// Sampling allocates only the returned snapshot.
    pub async fn lease_challenge_trace(
        &self,
        peer: &NodeId,
        lease_id: fr::LeaseId,
    ) -> Option<fr::LeaseTraceSnapshot> {
        self.inner
            .lock()
            .await
            .lease_challenge_trace(peer, lease_id)
    }

    /// The latency this link measured from its `Ping`/`Pong` pairs, as of now.
    ///
    /// `None` when no link to `peer` is up: latency belongs to a connection, so it is not
    /// retained after one goes down — a reconnected peer starts from an empty window rather
    /// than showing the latency of a socket that no longer exists. Zero fields mean the
    /// connection exists but has not answered enough pings yet
    /// ([`crate::rtt::MIN_SAMPLES`]).
    pub async fn rtt(&self, peer: &NodeId) -> Option<crate::rtt::RttSnapshot> {
        self.inner
            .lock()
            .await
            .links
            .get(peer)
            .map(|link| link.handle.rtt())
    }

    /// The current members (established links).
    pub async fn members(&self) -> Vec<NodeId> {
        self.inner.lock().await.links.keys().copied().collect()
    }

    /// The member count, for the membership gate.
    pub async fn member_count(&self) -> usize {
        self.inner.lock().await.links.len()
    }

    /// Drops every link abruptly, without a `Shutdown`, and stops the dialers with them.
    ///
    /// Each established member is reported to the session as `Down { LocalClosed }`; the
    /// session owns the user-facing link log and cleanup. The peer still sees a crash, and
    /// the dialers stop because this is also the local revocation path.
    pub async fn kill_all(&self) {
        let mut inner = self.inner.lock().await;
        let peers = std::mem::take(&mut inner.links)
            .into_iter()
            .map(|(peer, link)| {
                link.task.abort();
                peer
            })
            .collect::<Vec<_>>();
        for (_, pending) in std::mem::take(&mut inner.pending) {
            pending.task.abort();
        }
        for dialer in std::mem::take(&mut inner.dialers) {
            dialer.abort();
        }
        // The waiters would otherwise hold their dialers' senders until the manager is gone.
        inner.dial_waiters.clear();
        for (_, dialer) in std::mem::take(&mut inner.peer_dialers) {
            dialer.abort();
        }
        drop(inner);
        for peer in peers {
            let _ = self
                .events_tx
                .send(LinkEvent::Down {
                    peer,
                    reason: LinkDownReason::LocalClosed,
                })
                .await;
        }
    }

    /// Moves the listener without disturbing links or trust, returning its bound address.
    ///
    /// The caller selects the allowed adapter. A failed bind leaves the old listener
    /// serving; no address is persisted here. Requires the manager's `run` loop.
    pub async fn rebind_listener(&self, address: SocketAddr) -> Result<SocketAddr, TransportError> {
        let (reply, result) = oneshot::channel();
        self.messages_tx
            .send(ManagerMessage::RebindListener { address, reply })
            .await
            .map_err(|_| TransportError::LinkClosed)?;
        result.await.map_err(|_| TransportError::LinkClosed)?
    }

    /// Runs until the process ends: listens, dials, dedups, supervises.
    pub async fn run(&self) -> Result<(), TransportError> {
        // The verifier reads this shared set for each new certificate, so pairing updates
        // take effect without rebuilding the listener or restarting the process.
        let link_config = Arc::new(identity::server_config(
            &self.identity,
            TrustPolicy::PinnedShared(Arc::clone(&self.trusted_peers)),
            &identity::LINK_ALPN,
        )?);
        let pairing_config = Arc::new(identity::server_config(
            &self.identity,
            TrustPolicy::Pairing,
            &[limits::ALPN_PAIR, limits::ALPN_PAIR_REQUEST],
        )?);
        let mut listener = bind_listener(self.listen).await?;
        let local = self.identity.node_id();
        let settings = self.settings;
        let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_HANDSHAKES));
        let mut accept_backoff = AcceptBackoff::default();

        // One dialer per configured peer.
        let mut dialers = Vec::with_capacity(self.peers.len());
        for address in self.peers.clone() {
            let messages = self.messages_tx.clone();
            let identity = Arc::clone(&self.identity);
            let trusted_peers = Arc::clone(&self.trusted_peers);
            let local_hello = Arc::clone(&self.local_hello);
            dialers.push(tokio::spawn(async move {
                dial_loop(
                    address,
                    None,
                    identity,
                    trusted_peers,
                    local_hello,
                    messages,
                    settings,
                )
                .await;
            }));
        }
        self.inner.lock().await.dialers = dialers;

        loop {
            tokio::select! {
                (tcp, address) = accept_with_backoff(|| listener.accept(), &mut accept_backoff) => {
                    let Ok(permit) = Arc::clone(&handshakes).try_acquire_owned() else {
                        continue;
                    };
                    // Lease frames must not wait for a delayed TCP ACK during idle input.
                    if let Err(error) = tcp.set_nodelay(true) {
                        tracing::warn!(kind = ?error.kind(), "could not configure accepted socket");
                        continue;
                    }
                    let messages = self.messages_tx.clone();
                    let events = self.events_tx.clone();
                    let identity = Arc::clone(&self.identity);
                    let link_config = Arc::clone(&link_config);
                    let pairing_config = Arc::clone(&pairing_config);
                    let gate = self.pairing.clone();
                    let hello = self.local_hello.read().await.clone();
                    let clock = self.clock;
                    let pending_unpairs = Arc::clone(&self.pending_unpairs);
                    let trusted_peers = Arc::clone(&self.trusted_peers);
                    tokio::spawn(async move {
                        let _permit = permit;
                        serve_connection(
                            tcp,
                            address,
                            identity,
                            link_config,
                            pairing_config,
                            gate,
                            events,
                            hello,
                            messages,
                            settings,
                            clock,
                            pending_unpairs,
                            trusted_peers,
                        )
                        .await;
                    });
                }
                message = async { self.messages_rx.lock().await.recv().await } => {
                    let Some(message) = message else { return Ok(()) };
                    self.on_message(message, local, &mut listener).await;
                }
            }
        }
    }

    async fn on_message(&self, message: ManagerMessage, local: NodeId, listener: &mut TcpListener) {
        match message {
            ManagerMessage::PeerRejected { peer } => {
                let _ = self.events_tx.send(LinkEvent::PeerRejected { peer }).await;
            }
            ManagerMessage::RebindListener { address, reply } => {
                let result = async {
                    if listener.local_addr()? == address {
                        return Ok(address);
                    }
                    let replacement = bind_listener(address).await?;
                    let bound = replacement.local_addr()?;
                    *listener = replacement;
                    Ok(bound)
                }
                .await;
                let _ = reply.send(result);
            }
            ManagerMessage::AddPeer { peer, address } => {
                let dialer = self.spawn_peer_dialer(peer, address);
                if let Some(previous) = self.inner.lock().await.peer_dialers.insert(peer, dialer) {
                    previous.abort();
                }
            }
            ManagerMessage::RemovePeer { peer } => {
                let mut inner = self.inner.lock().await;
                inner.discovered_routes.remove(&peer);
                if let Some(dialer) = inner.peer_dialers.remove(&peer) {
                    dialer.abort();
                }
                inner.dial_waiters.remove(&peer);
                let pending_keys: Vec<_> = inner
                    .pending
                    .iter()
                    .filter_map(|(k, p)| if p.peer == peer { Some(*k) } else { None })
                    .collect();
                for key in pending_keys {
                    if let Some(pending) = inner.pending.remove(&key) {
                        pending.task.abort();
                    }
                }
                let live = inner.links.remove(&peer);
                drop(inner);
                if let Some(live) = live {
                    live.handle.close();
                    live.task.abort();
                    let _ = self
                        .events_tx
                        .send(LinkEvent::Down {
                            peer,
                            reason: LinkDownReason::LocalClosed,
                        })
                        .await;
                }
            }
            ManagerMessage::Register {
                key,
                peer,
                direction,
                local_capabilities,
                handle,
                task,
            } => {
                let initiator = if direction.is_outgoing() { local } else { peer };
                let registered_at_ns = self.clock.now_ns();
                self.inner.lock().await.pending.insert(
                    key,
                    PendingLink {
                        peer,
                        handle,
                        initiator,
                        local_capabilities,
                        task,
                        registered_at_ns,
                        replace_existing: true,
                    },
                );
            }
            ManagerMessage::DialPermit { peer, reply } => {
                let mut inner = self.inner.lock().await;
                if inner.has_link_to(&peer) {
                    inner.dial_waiters.entry(peer).or_default().push(reply);
                } else {
                    let _ = reply.send(DialTurn::Free);
                }
            }
            ManagerMessage::Report(report) => match report {
                LinkReport::PeerRejected { peer } => {
                    let _ = self.events_tx.send(LinkEvent::PeerRejected { peer }).await;
                }
                LinkReport::Up {
                    key,
                    peer,
                    hello,
                    connection_id,
                    remote,
                } => self.on_up(key, peer, hello, connection_id, remote).await,
                LinkReport::Message { peer, message } => {
                    let send_started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
                    static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                    let result = self
                        .events_tx
                        .send(LinkEvent::Message { peer, message })
                        .await;
                    let send_wait = send_started.map_or(Duration::ZERO, |start| start.elapsed());
                    if send_wait >= Duration::from_millis(10)
                        && tracing::enabled!(tracing::Level::DEBUG)
                        && LOG_RATE.allow()
                    {
                        tracing::debug!(
                            peer = ?peer,
                            elapsed_ms = send_wait.as_millis(),
                            remaining_capacity = self.events_tx.capacity(),
                            "manager event queue blocked"
                        );
                    }
                    let _ = result;
                }
                LinkReport::Down { key, peer, reason } => {
                    let removed = {
                        let mut inner = self.inner.lock().await;
                        inner.pending.remove(&key);
                        // Only the link that went down may leave the member list: a `Down` is
                        // matched against the *key* the manager handed that link, so a closed
                        // newcomer in the same direction as a live link cannot evict it.
                        let removed = match inner.links.get(&peer) {
                            Some(live) if live.key == key => {
                                inner.links.remove(&peer);
                                true
                            }
                            _ => false,
                        };
                        // Whichever link went away, a dialer waiting for this peer may have
                        // its turn now — the check inside only grants when nothing is left.
                        inner.release_dial_waiters(peer);
                        removed
                    };
                    if removed || reason == LinkDownReason::MembershipFull {
                        let _ = self.events_tx.send(LinkEvent::Down { peer, reason }).await;
                    }
                }
            },
        }
    }

    async fn on_up(
        &self,
        key: [u8; 16],
        peer: NodeId,
        hello: PeerInfo,
        connection_id: [u8; 16],
        remote: std::net::SocketAddr,
    ) {
        let incoming = self.inner.lock().await.pending.remove(&key);
        let Some(pending) = incoming else {
            return;
        };

        // Simultaneous dial: both sides compute the same winner from the two identities, so
        // the losing link is closed on both ends without any negotiation. The rule only
        // decides dials that *overlapped* (`docs/05-protocol.md`, «одновременный взаимный
        // дозвон»): a link that registered after the live link was already up is a late redial
        // — a staggered start, a reconnect that overlapped an old link's last moments, a dial
        // loop that outlived its link — and a late redial is refused, never deduplicated.
        // Replacing a live, healthy link tears down the session it carries: the mesh used to
        // do exactly that under load, both ends logging `Down{LocalClosed}` + `Up` while a
        // lease was Remote. The closed newcomer is not a member loss, so no `Down` is
        // reported for it — the live link keeps its place in silence.
        let (existing_key, replaced) = {
            let inner = self.inner.lock().await;
            match inner.links.get(&peer) {
                // A link cannot replace itself: a repeated `Up` for the same key is not a
                // second link.
                Some(existing) if existing.key != key => {
                    let simultaneous =
                        policy::formation_overlap(existing.up_at_ns, pending.registered_at_ns);
                    let keep_incoming = pending.replace_existing
                        && simultaneous
                        && policy::prefer_incoming(&existing.initiator, &pending.initiator);
                    (Some(existing.key), keep_incoming)
                }
                _ => (None, false),
            }
        };
        if existing_key.is_some() && !replaced {
            // The existing link wins; ask the new one to close.
            pending.handle.close();
            return;
        }
        if replaced {
            // The incoming link wins; the replaced one gets a `Shutdown` and ends by itself.
            if let Some(existing) = self.inner.lock().await.links.remove(&peer) {
                existing.handle.close();
                existing.task.abort();
            }
            // …and the application is told, because that link may be carrying its frames right
            // now. A silent replacement lost an in-flight `LeaveAck` in a three-node run: the
            // peer came back mid-transition with no `Down` before it, and the leaving node sat
            // in `Releasing` with nothing left to read. `LocalClosed` is the reason for exactly
            // this case — «the local side closed it … or a link that replaced this one».
            let _ = self
                .events_tx
                .send(LinkEvent::Down {
                    peer,
                    reason: LinkDownReason::LocalClosed,
                })
                .await;
        }

        // The peer must still be trusted *now*: a link whose handshake completed before a
        // revocation that is racing it would otherwise be admitted after it (audit F16 — the
        // pin set is the live one the verifiers read, and the CLI/GUI revoke drops the live
        // links, so a link forming at that moment has to be refused here).
        let still_trusted = self
            .trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&peer);
        if !still_trusted {
            tracing::warn!(peer = %peer, "a link for a peer that is no longer trusted was refused");
            pending.handle.close();
            return;
        }

        // Membership: «пятый член получает `MEMBERSHIP_FULL`».
        let member_count = {
            let inner = self.inner.lock().await;
            inner.links.len()
        };
        let already_member = {
            let inner = self.inner.lock().await;
            inner.links.contains_key(&peer)
        };
        if !already_member {
            if let Err(full) = policy::admit_member(member_count, self.max_members) {
                let _ = full;
                let _ = pending
                    .handle
                    .send(fr::Message::ProtocolError {
                        code: fr::ErrorCode::MembershipFull,
                        related_seq: 0,
                    })
                    .await;
                // The refusal is the last thing this link sends, and it must not be thrown
                // away: closing here used to end the link immediately, and on Windows a close
                // with anything of the peer's still unread sends a reset — a reset discards
                // the frames the peer has not read, so the refused member saw
                // `Io(... 10054)` instead of `MEMBERSHIP_FULL` in ~1 loaded run in 100
                // (`mesh::the_membership_cap_refuses_the_extra_member`).
                //
                // The peer closes as soon as it has read the refusal, which ends this link
                // through its own reader; a peer that does not is bounded by the liveness
                // timeout («офлайн через 3 s без `Pong`»). The pending entry is already
                // removed, so a retry is not blocked by this one.
                drop(pending);
                let _ = self
                    .events_tx
                    .send(LinkEvent::Down {
                        peer,
                        reason: LinkDownReason::MembershipFull,
                    })
                    .await;
                return;
            }
        }

        let up_at_ns = self.clock.now_ns();
        {
            // A handshake may have captured Hello before the latest AgentReady update.
            // Serialize catch-up and insertion with later updates, including their enqueues.
            let current = self.local_hello.read().await;
            let mut inner = self.inner.lock().await;
            if pending.local_capabilities != current.capabilities {
                let _ = pending
                    .handle
                    .send_capabilities(current.capabilities.clone())
                    .await;
            }
            inner
                .last_lease_diagnostics
                .insert(peer, pending.handle.lease_diagnostics_counters());
            inner.links.insert(
                peer,
                LiveLink {
                    key,
                    handle: pending.handle,
                    initiator: pending.initiator,
                    up_at_ns,
                    task: pending.task,
                },
            );
        }
        let _ = self
            .events_tx
            .send(LinkEvent::Up {
                peer,
                hello,
                connection_id,
                remote,
            })
            .await;
    }
}

/// Binds the link listener, refusing to share the port with any other socket.
///
/// Unix: `SO_REUSEADDR`, which is what `std`/`tokio` already set there — it only allows a
/// bind while *closed* connections linger in `TIME_WAIT`, and on Unix a second live listener
/// still fails with `EADDRINUSE`.
///
/// Windows: `SO_EXCLUSIVEADDRUSE`, **not** `SO_REUSEADDR`. The two names are misleadingly
/// similar and their Windows meanings are nearly opposite (Microsoft, «Using SO_REUSEADDR
/// and SO_EXCLUSIVEADDRUSE»): with `SO_REUSEADDR` a *second, unprivileged* local process can
/// bind a port that a live listener is already using and take over its traffic — so every
/// dial a peer makes to `47901` could be delivered to a hijacker instead of to core. Core
/// holds exactly one listener per port (docs/05-protocol.md: both nodes listen on `47901`;
/// docs/07-quality-security.md treats a second instance as an error, not as a peer), and
/// `SO_EXCLUSIVEADDRUSE` is how the OS enforces that: the second bind fails with
/// `WSAEADDRINUSE` (10048) instead of silently stealing the port. It also keeps `TIME_WAIT`
/// connections made *by* this port from blocking the rebind, so a core that restarts after a
/// crash still comes up; a bind that fails while another core is alive is the answer we want
/// (the error is reported as `the link manager stopped: … os error 10048` instead of a
/// silent takeover), not a case to paper over. That failure is not retried: the manager
/// stops and the core keeps running without links, so the losing core has to be restarted.
pub(crate) async fn bind_listener(address: SocketAddr) -> std::io::Result<TcpListener> {
    let domain = if address.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    if address.is_ipv6() {
        // Parity with the previous `TcpSocket::new_v6` default.
        socket.set_only_v6(false)?;
    }
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    #[cfg(windows)]
    set_exclusive_address_use(&socket)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;

    let listener = std::net::TcpListener::from(socket);
    // `TcpListener::from_std` requires the caller to have set the mode.
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

/// Windows: `SO_EXCLUSIVEADDRUSE`, the option that actually means «this port is mine».
///
/// Neither `tokio::net::TcpSocket` nor `socket2` 0.6 exposes it, and `SO_REUSEADDR` is its
/// opposite on Windows — a socket that sets *it* may bind over a live listener. One
/// `setsockopt` call is cheaper than a whole option-poking dependency.
#[cfg(windows)]
fn set_exclusive_address_use(socket: &socket2::Socket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock;

    let enabled: u32 = 1;
    // SAFETY: `setsockopt` only reads `enabled` (a live local) and
    // `size_of::<u32>()` bytes of it; the socket handle comes from a live `socket2::Socket`.
    let result = unsafe {
        WinSock::setsockopt(
            socket.as_raw_socket() as WinSock::SOCKET,
            WinSock::SOL_SOCKET,
            WinSock::SO_EXCLUSIVEADDRUSE,
            std::ptr::addr_of!(enabled).cast(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Serves one accepted TCP connection: peeks the ClientHello and picks the TLS config by
/// ALPN (`docs/05-protocol.md`: `pair/1` shares `47901`).
///
/// A `pair/1` ClientHello is admitted through the gate and only then handshaked with
/// [`identity::pairing_server_config`] (unknown peers allowed); `LinkEvent::Pairing` hands
/// the connection to the application, which runs [`identity::accept_stream`]. With no window
/// open (or a spent or expired one — [`PairingGate::admit`] decides) the connection is
/// dropped *before* the handshake: that is the documented refusal, and the client sees the
/// socket close instead of a `PairHello`.
///
/// Everything else — `input/1`, `clipboard/1`, no ALPN — completes the handshake with the
/// pinned link config and takes the existing [`accept_one`] path, so an unknown peer on
/// `input/1` is still refused by the pin.
#[allow(clippy::too_many_arguments)]
async fn serve_connection(
    tcp: TcpStream,
    address: SocketAddr,
    identity: Arc<NodeIdentity>,
    link_config: Arc<rustls::ServerConfig>,
    pairing_config: Arc<rustls::ServerConfig>,
    gate: PairingGate,
    events: mpsc::Sender<LinkEvent>,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
    clock: Clock,
    pending_unpairs: Arc<Mutex<crate::unpair::PendingUnpairs>>,
    trusted_peers: Arc<StdRwLock<BTreeSet<NodeId>>>,
) {
    let handshake_timeout = Duration::from_millis(settings.liveness.offline_ms.max(1));
    let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), tcp);
    let start = match tokio::time::timeout(handshake_timeout, acceptor).await {
        Ok(Ok(start)) => start,
        // A connection that says nothing (or sends a malformed ClientHello) is dropped
        // rather than pinning a task for ever.
        _ => return,
    };
    let wants_pairing = start
        .client_hello()
        .alpn()
        .is_some_and(|mut alpn| alpn.any(|protocol| protocol == limits::ALPN_PAIR.as_bytes()));
    let wants_request = start.client_hello().alpn().is_some_and(|mut alpn| {
        alpn.any(|protocol| protocol == limits::ALPN_PAIR_REQUEST.as_bytes())
    });
    // The TLS config prefers pair/1 when a client offers both; dispatch must agree.
    if wants_request && !wants_pairing {
        let stream = match tokio::time::timeout(
            handshake_timeout,
            start.into_stream(pairing_config),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            _ => return,
        };
        let Ok(peer) = peer_node_id(stream.get_ref().1.peer_certificates().unwrap_or_default())
        else {
            return;
        };
        let _ = events
            .send(LinkEvent::PairingRequest(Box::new(
                crate::pairing::RequestConnection {
                    stream,
                    address,
                    peer,
                },
            )))
            .await;
        return;
    }

    if wants_pairing {
        // Resolve the gate before the handshake: an error here is the documented refusal.
        let window = match gate.admit(clock.now_ms()) {
            Ok(window) => window,
            Err(_reason) => return,
        };
        // Every early return past this point releases the admitted slot; only a connection
        // actually handed to the application disarms it, because only then does a ceremony
        // own it and call `PairingGate::finish`.
        let guard = gate.admit_guard(*window.attempt_id());
        let pairing_config = if let Some(requester) = window.requester() {
            match identity::server_config(
                &identity,
                TrustPolicy::PairingRequester(requester),
                &[limits::ALPN_PAIR],
            ) {
                Ok(config) => Arc::new(config),
                Err(_) => return,
            }
        } else {
            pairing_config
        };
        let stream = match tokio::time::timeout(
            handshake_timeout,
            start.into_stream(pairing_config),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            _ => return,
        };
        let _ = events
            .send(LinkEvent::Pairing(Box::new(PairingConnection {
                stream,
                address,
                window,
            })))
            .await;
        guard.disarm();
        return;
    }

    // Pending pins are admitted only here, never by shared_pins or clipboard TLS.
    let notices = pending_unpairs
        .lock()
        .await
        .records(crate::unpair::now_secs());
    let link_config = if notices.is_empty() {
        link_config
    } else {
        let mut pins = trusted_peers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        pins.extend(notices.iter().map(|notice| notice.peer));
        let Ok(config) =
            identity::server_config(&identity, TrustPolicy::Pinned(pins), &identity::LINK_ALPN)
        else {
            return;
        };
        Arc::new(config)
    };
    let stream = match tokio::time::timeout(handshake_timeout, start.into_stream(link_config)).await
    {
        Ok(Ok(stream)) => stream,
        _ => return,
    };
    let Ok(peer) = peer_node_id(stream.get_ref().1.peer_certificates().unwrap_or_default()) else {
        return;
    };
    let notice = pending_unpairs
        .lock()
        .await
        .records(crate::unpair::now_secs())
        .into_iter()
        .find(|notice| {
            notice.peer == peer
                && !trusted_peers
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&peer)
        });
    if let Some(notice) = notice {
        let mut stream = stream;
        let sent = tokio::time::timeout(
            handshake_timeout,
            crate::unpair::notify_only(&mut stream, peer, hello),
        )
        .await;
        if matches!(sent, Ok(Ok(()))) {
            if let Err(error) = pending_unpairs
                .lock()
                .await
                .complete(peer, notice.created_unix_secs)
            {
                tracing::warn!(%error, "could not persist completed unpair notice");
            }
        }
        return;
    }
    if !trusted_peers
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(&peer)
    {
        return;
    }
    accept_one(stream, identity, hello, messages, settings).await;
}

/// Runs the pinned link path on an already-handshaked stream: reads the peer `node_id` from
/// its certificate and spawns the link.
async fn accept_one(
    stream: tokio_rustls::server::TlsStream<TcpStream>,
    identity: Arc<NodeIdentity>,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
) {
    let peer = match stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
    {
        Some(certificate) => match peer_node_id(std::slice::from_ref(certificate)) {
            Ok(peer) => peer,
            Err(_) => return,
        },
        None => return,
    };
    let Ok(key) = identity::random16() else {
        return;
    };
    start_link(
        LinkSpawn {
            key,
            peer,
            direction: Direction::Incoming,
        },
        Stream::Server(Box::new(stream)),
        identity.node_id(),
        hello,
        messages,
        settings,
    )
    .await;
}

async fn dial_loop(
    address: SocketAddr,
    expected_peer: Option<NodeId>,
    identity: Arc<NodeIdentity>,
    trusted_peers: Arc<StdRwLock<BTreeSet<NodeId>>>,
    local_hello: Arc<RwLock<wire::Hello>>,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
) {
    let client_config = match identity::client_config(
        &identity,
        TrustPolicy::PinnedShared(Arc::clone(&trusted_peers)),
        &identity::LINK_ALPN,
    ) {
        Ok(config) => config,
        Err(_) => return,
    };
    let connector = TlsConnector::from(Arc::new(client_config));
    // How long one dial may take.
    //
    // `TcpStream::connect` and the TLS handshake have no deadline of their own, and a peer that
    // accepts the connection and then says nothing — a blackholed route, a suspended core, a
    // half-open path — parks the dialer for ever; with it the whole reconnect schedule stops,
    // and the node never dials that peer again while it lives. That is how a partition failed to
    // heal on the two-CPU CI runner: both ends logged `link down reason=Timeout` and then no
    // `link up` for the rest of the test. The window is the one the protocol already gives a
    // peer to answer a `Ping` («офлайн через 3 s без `Pong`»): a handshake that cannot finish
    // inside it belongs to an unreachable peer, not a slow one.
    let dial_timeout = Duration::from_millis(settings.liveness.offline_ms);
    // A pairing-created dialer knows which key must answer its address; a configured dialer
    // learns that only after the first handshake.
    let mut peer = expected_peer;
    let mut attempt = 0u32;
    loop {
        // One dial per peer, at most one link per peer: once the peer is known, dialing is the
        // *manager's* turn to give. While the peer has a link — dialled by it, or by a link
        // this loop's own attempt lost — a dial would only build a connection the manager
        // refuses at its `Up`; a dialer that looped anyway burned a handshake every backoff for
        // the life of the mesh. When the link goes, the manager hands the turn over and this
        // loop heals it.
        if let Some(known) = peer {
            match request_dial_permit(&messages, known).await {
                // The peer was linked and stopped being: reconnect promptly, not on the
                // backoff a long-unreachable peer would demand.
                Some(DialTurn::Waited) => attempt = 0,
                // Nothing linked at all: the backoff keeps growing towards its ceiling.
                Some(DialTurn::Free) => {}
                // The manager stopped listening; there is nothing left to dial for.
                None => return,
            }
        }
        let connected = tokio::time::timeout(dial_timeout, async {
            let tcp = TcpStream::connect(address).await.ok()?;
            tcp.set_nodelay(true).ok()?;
            match connector
                .connect(identity::server_name(address.ip()), tcp)
                .await
            {
                Ok(stream) => Some(stream),
                Err(error) => {
                    if crate::unpair::unknown_peer_alert(&error) {
                        if let Some(peer) = peer {
                            let _ = messages.send(ManagerMessage::PeerRejected { peer }).await;
                        }
                    }
                    None
                }
            }
        })
        .await
        .ok()
        .flatten();
        if let Some(stream) = connected {
            let dialed = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .and_then(|certificate| peer_node_id(std::slice::from_ref(certificate)).ok());
            if let Some(dialed) =
                dialed.filter(|dialed| expected_peer.is_none_or(|expected| expected == *dialed))
            {
                peer = Some(dialed);
                if let Ok(key) = identity::random16() {
                    let hello = local_hello.read().await.clone();
                    start_link(
                        LinkSpawn {
                            key,
                            peer: dialed,
                            direction: Direction::Outgoing,
                        },
                        Stream::Client(Box::new(stream)),
                        identity.node_id(),
                        hello,
                        messages.clone(),
                        settings,
                    )
                    .await;
                    attempt = 0;
                }
            }
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(policy::reconnect_delay(attempt, settings.backoff)).await;
    }
}

/// Asks the manager for the turn to dial `peer`, resolving once no link to it is left.
///
/// `None` means the manager stopped listening — its channel is gone, so this dialer has nobody
/// left to report a link to and may as well stop.
async fn request_dial_permit(
    messages: &mpsc::Sender<ManagerMessage>,
    peer: NodeId,
) -> Option<DialTurn> {
    let (reply, granted) = oneshot::channel();
    messages
        .send(ManagerMessage::DialPermit { peer, reply })
        .await
        .ok()?;
    granted.await.ok()
}

/// What one spawned link is: which peer, which way it was dialled, and the key the manager
/// knows it by (see [`LinkConfig::key`]).
struct LinkSpawn {
    key: [u8; 16],
    peer: NodeId,
    direction: Direction,
}

/// Spawns a link, registers it with the manager and waits for it to end.
async fn start_link(
    spawn: LinkSpawn,
    stream: Stream,
    local: NodeId,
    local_hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
) {
    let LinkSpawn {
        key,
        peer,
        direction,
    } = spawn;
    let (reports_tx, mut reports_rx) = mpsc::channel::<LinkReport>(64);
    let local_capabilities = local_hello.capabilities.clone();
    let Ok(link) = crate::link::spawn(LinkConfig {
        key,
        peer,
        local,
        direction,
        local_hello,
        stream,
        reports: reports_tx,
        queue_capacity: settings.queue_capacity,
        liveness: settings.liveness,
    }) else {
        return;
    };
    let handle = link.handle.clone();
    if messages
        .send(ManagerMessage::Register {
            key,
            peer,
            direction,
            local_capabilities,
            handle,
            task: link.task,
        })
        .await
        .is_err()
    {
        return;
    }
    // Forward reports until the link's task is gone (the channel closes) or the manager
    // stops listening.
    while let Some(report) = reports_rx.recv().await {
        if messages.send(ManagerMessage::Report(report)).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accept_errors_back_off_and_recover() {
        let attempts = std::cell::Cell::new(0);
        let mut backoff = AcceptBackoff::default();
        let result = accept_with_backoff(
            || {
                attempts.set(attempts.get() + 1);
                std::future::ready(if attempts.get() <= 2 {
                    Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory))
                } else {
                    Ok(7)
                })
            },
            &mut backoff,
        )
        .await;
        assert_eq!(result, 7);
        assert_eq!(attempts.get(), 3);
        assert!(backoff.delay.is_zero(), "success resets the failure streak");
        assert!(backoff.retry_at.is_none());
    }

    #[tokio::test]
    async fn accept_backoff_survives_select_cancellation_and_is_capped() {
        let mut backoff = AcceptBackoff::default();
        for expected_ms in [25, 50, 100, 200, 400, 800, 1000, 1000] {
            backoff.failed();
            assert_eq!(backoff.delay, Duration::from_millis(expected_ms));
        }
        let attempts = std::cell::Cell::new(0);
        for _ in 0..3 {
            let result = tokio::time::timeout(
                Duration::from_millis(1),
                accept_with_backoff(
                    || {
                        attempts.set(attempts.get() + 1);
                        std::future::ready(Err::<(), _>(std::io::Error::from(
                            std::io::ErrorKind::OutOfMemory,
                        )))
                    },
                    &mut backoff,
                ),
            )
            .await;
            assert!(result.is_err());
        }
        assert_eq!(
            attempts.get(),
            0,
            "cancellation must not bypass the deadline"
        );
    }

    #[tokio::test]
    async fn input_admission_is_bounded_and_manager_stays_responsive() {
        use tokio::io::AsyncReadExt;

        let (root, local, _, _) = discovery_fixture("input-admission");
        let manager = Arc::new(discovery_manager(local, &[]));
        let runner = Arc::clone(&manager);
        let task = tokio::spawn(async move { runner.run().await });
        let address = tokio::time::timeout(
            Duration::from_secs(1),
            manager.rebind_listener("127.0.0.1:0".parse().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        let mut clients = Vec::new();
        for _ in 0..40 {
            clients.push(TcpStream::connect(address).await.unwrap());
        }
        let mut readers = tokio::task::JoinSet::new();
        for mut client in clients {
            readers.spawn(async move {
                let mut byte = [0];
                let result =
                    tokio::time::timeout(Duration::from_millis(100), client.read(&mut byte)).await;
                (client, matches!(result, Ok(Ok(0)) | Ok(Err(_))))
            });
        }
        let mut closed = 0;
        let mut clients = Vec::new();
        while let Some(result) = readers.join_next().await {
            let (client, was_closed) = result.unwrap();
            closed += usize::from(was_closed);
            clients.push(client);
        }
        assert_eq!(
            closed, 8,
            "only 32 stalled input sockets may remain admitted"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), manager.rebind_listener(address),)
                .await
                .expect("saturation must not block manager commands")
                .unwrap(),
            address
        );
        drop(clients);
        task.abort();
        let _ = task.await;
        manager.kill_all().await;
        let _ = std::fs::remove_dir_all(root);
    }

    fn discovery_fixture(
        label: &str,
    ) -> (
        std::path::PathBuf,
        Arc<NodeIdentity>,
        Arc<NodeIdentity>,
        Arc<NodeIdentity>,
    ) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "bifrost-discovery-{}-{}-{label}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let identity = |name: &str| {
            Arc::new(
                NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(root.join(name)))
                    .expect("identity")
                    .0,
            )
        };
        let local = identity("local");
        let expected = identity("expected");
        let other = identity("other");
        (root, local, expected, other)
    }

    fn discovery_manager(local: Arc<NodeIdentity>, peers: &[NodeId]) -> LinkManager {
        let mut trust = TrustStore::new();
        for peer in peers {
            trust.trust(TrustedPeer::paired(*peer, "peer", None, 0));
        }
        let id = local.node_id();
        LinkManager::new(ManagerConfig {
            identity: local,
            trust,
            local_hello: wire::Hello {
                node_id: id.as_bytes().to_vec(),
                ..Default::default()
            },
            listen: "127.0.0.1:0".parse().unwrap(),
            peers: Vec::new(),
            max_members: 4,
            queue_capacity: 16,
            liveness: policy::LinkLiveness::default(),
            backoff: policy::BackoffBounds::default(),
            pairing: PairingGate::default(),
        })
        .expect("manager")
        .0
    }

    fn insert_live(inner: &mut Inner, peer: NodeId) {
        inner.links.insert(
            peer,
            LiveLink {
                key: [5; 16],
                handle: LinkHandle::for_counter_test(peer),
                initiator: peer,
                up_at_ns: 0,
                task: tokio::spawn(std::future::pending()),
            },
        );
    }

    #[tokio::test]
    async fn listener_rebind_preserves_links_and_keeps_old_listener_on_failure() {
        let (root, local, peer, _other) = discovery_fixture("rebind");
        let local_id = local.node_id();
        let peer_id = peer.node_id();
        let manager = discovery_manager(local, &[peer_id]);
        insert_live(&mut *manager.inner.lock().await, peer_id);
        let mut listener = bind_listener("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let original = listener.local_addr().unwrap();
        let occupied = bind_listener("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let blocked_address = occupied.local_addr().unwrap();
        for target in [blocked_address, "127.0.0.1:0".parse().unwrap()] {
            let before = listener.local_addr().unwrap();
            let (result, ()) = tokio::join!(manager.rebind_listener(target), async {
                let message = manager.messages_rx.lock().await.recv().await.unwrap();
                manager.on_message(message, local_id, &mut listener).await;
            },);
            if target == blocked_address {
                assert!(matches!(result, Err(TransportError::Io(_))));
                assert_eq!(listener.local_addr().unwrap(), before);
            } else {
                let bound = result.unwrap();
                assert_ne!(bound, original);
                assert_eq!(listener.local_addr().unwrap(), bound);
                let released = bind_listener(original)
                    .await
                    .expect("old listening socket released");
                drop(released);
            }
            let client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (accepted, _) = listener.accept().await.unwrap();
            drop((client, accepted));
            assert_eq!(manager.inner.lock().await.links[&peer_id].key, [5; 16]);
            assert_eq!(
                *manager.trusted_peers.read().unwrap(),
                BTreeSet::from([peer_id])
            );
        }
        manager.kill_all().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn discovery_requires_trusted_different_route_and_down_link() {
        let peer = NodeId::new([1; 32]);
        let old = "127.0.0.1:47901".parse().unwrap();
        let new = "127.0.0.2:47901".parse().unwrap();
        let mut inner = Inner::default();
        assert!(matches!(
            inner.discovery_decision(&BTreeSet::new(), peer, new),
            Err(DiscoveryRouteError::Untrusted)
        ));
        let pins = BTreeSet::from([peer]);
        inner.discovered_routes.insert(peer, old);
        assert_eq!(
            inner.discovery_decision(&pins, peer, old).unwrap(),
            Some(DiscoveryRoute::Unchanged)
        );
        assert_eq!(inner.discovery_decision(&pins, peer, new).unwrap(), None);
        insert_live(&mut inner, peer);
        assert_eq!(
            inner.discovery_decision(&pins, peer, new).unwrap(),
            Some(DiscoveryRoute::LinkPresent)
        );
        assert_eq!(inner.discovered_routes[&peer], old);
        assert_eq!(inner.links[&peer].key, [5; 16]);
        inner.links.remove(&peer).unwrap().task.abort();
    }

    #[tokio::test]
    async fn discovery_authenticates_exact_identity_before_updating_route() {
        let (root, local, expected, other) = discovery_fixture("identity");
        let manager = discovery_manager(Arc::clone(&local), &[expected.node_id(), other.node_id()]);
        for (server, matches_expected) in
            [(Arc::clone(&other), false), (Arc::clone(&expected), true)]
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let config = identity::server_config(
                &server,
                TrustPolicy::Pinned(BTreeSet::from([local.node_id()])),
                &identity::LINK_ALPN,
            )
            .unwrap();
            let server_task = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                tokio_rustls::TlsAcceptor::from(Arc::new(config))
                    .accept(tcp)
                    .await
                    .unwrap()
            });
            let result = manager
                .refresh_discovered_route(expected.node_id(), address)
                .await;
            let server_stream = server_task.await.unwrap();
            if matches_expected {
                assert_eq!(result.unwrap(), DiscoveryRoute::Verified);
                let inner = manager.inner.lock().await;
                assert_eq!(inner.discovered_routes[&expected.node_id()], address);
                assert!(inner.has_link_to(&expected.node_id()));
                assert!(inner
                    .pending
                    .values()
                    .all(|pending| !pending.replace_existing));
                assert!(inner.peer_dialers.contains_key(&expected.node_id()));
            } else {
                assert!(
                    matches!(result, Err(DiscoveryRouteError::IdentityMismatch { expected: id, actual })
                    if id == expected.node_id() && actual == server.node_id())
                );
                let inner = manager.inner.lock().await;
                assert!(inner.discovered_routes.is_empty());
                assert!(inner.pending.is_empty());
                assert!(inner.peer_dialers.is_empty());
            }
            drop(server_stream);
        }
        assert_eq!(
            *manager.trusted_peers.read().unwrap(),
            BTreeSet::from([expected.node_id(), other.node_id()])
        );
        manager.kill_all().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn discovery_rechecks_link_down_after_tls_before_registration() {
        let (root, local, expected, _other) = discovery_fixture("race");
        let manager = Arc::new(discovery_manager(Arc::clone(&local), &[expected.node_id()]));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = identity::server_config(
            &expected,
            TrustPolicy::Pinned(BTreeSet::from([local.node_id()])),
            &identity::LINK_ALPN,
        )
        .unwrap();
        let (accepted, connected) = oneshot::channel();
        let (proceed, ready) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            ready.await.unwrap();
            tokio_rustls::TlsAcceptor::from(Arc::new(config))
                .accept(tcp)
                .await
                .unwrap()
        });
        let probe_manager = Arc::clone(&manager);
        let peer = expected.node_id();
        let probe =
            tokio::spawn(
                async move { probe_manager.refresh_discovered_route(peer, address).await },
            );
        connected.await.unwrap();
        insert_live(&mut *manager.inner.lock().await, peer);
        proceed.send(()).unwrap();
        assert_eq!(probe.await.unwrap().unwrap(), DiscoveryRoute::LinkPresent);
        let server_stream = server.await.unwrap();
        {
            let inner = manager.inner.lock().await;
            assert_eq!(inner.links[&peer].key, [5; 16]);
            assert!(inner.pending.is_empty());
            assert!(inner.discovered_routes.is_empty());
            assert!(inner.peer_dialers.is_empty());
        }
        manager.kill_all().await;
        drop(server_stream);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn lease_diagnostics_retain_nonzero_counts_after_link_removal() {
        let peer = NodeId::new([7; 32]);
        let handle = LinkHandle::for_counter_test(peer);
        let counters = handle.lease_diagnostics_counters();
        let renew = wire::envelope::Payload::LeaseRenew(Default::default());
        counters.record(Some(&renew), true);
        let mut inner = Inner::default();
        assert_eq!(inner.lease_diagnostics(&peer), None);
        inner
            .last_lease_diagnostics
            .insert(peer, Arc::clone(&counters));
        inner.links.insert(
            peer,
            LiveLink {
                key: [1; 16],
                handle,
                initiator: peer,
                up_at_ns: 0,
                task: tokio::spawn(async {}),
            },
        );
        assert_eq!(inner.lease_diagnostics(&peer).unwrap().renew_written, 1);
        let removed = inner.links.remove(&peer).unwrap();
        removed.task.abort();
        drop(removed);
        counters.record(Some(&renew), false);
        let final_snapshot = inner.lease_diagnostics(&peer).unwrap();
        assert_eq!(final_snapshot.renew_written, 1);
        assert_eq!(final_snapshot.renew_decoded, 1);
        let next = LinkHandle::for_counter_test(peer);
        inner
            .last_lease_diagnostics
            .insert(peer, next.lease_diagnostics_counters());
        assert_eq!(
            inner.lease_diagnostics(&peer),
            Some(LeaseTransportSnapshot::default())
        );
    }

    /// The port belongs to exactly one listener, even to a process that asks to share it.
    ///
    /// On Windows `SO_REUSEADDR` lets a *second* socket bind a port a live listener is using
    /// — that is the hijack [`bind_listener`] refuses by setting `SO_EXCLUSIVEADDRUSE`, and
    /// this test performs the hijack attempt (it binds with `SO_REUSEADDR` set). On Unix a
    /// second live bind fails either way.
    #[tokio::test]
    async fn a_live_listener_cannot_be_bound_over() {
        let first = bind_listener("127.0.0.1:0".parse().expect("addr"))
            .await
            .expect("bind");
        let address = first.local_addr().expect("local address");

        let hijack = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .expect("socket");
        hijack.set_reuse_address(true).expect("SO_REUSEADDR");
        let result = hijack.bind(&address.into());

        assert!(
            result.is_err(),
            "a second bind of {address} was allowed while core is listening on it"
        );
    }

    /// A core that restarts after it had connections on its port must be able to bind that
    /// port again — the whole point of the exclusive flag is that it refuses *live*
    /// competition, not that it keeps a dead core's port reserved.
    #[tokio::test]
    async fn the_listener_rebinds_after_a_connection() {
        let first = bind_listener("127.0.0.1:0".parse().expect("addr"))
            .await
            .expect("bind");
        let address = first.local_addr().expect("local address");

        let client = TcpStream::connect(address).await.expect("connect");
        let (accepted, _) = first.accept().await.expect("accept");
        // The listener "crashes": both sockets go, the connection lingers.
        drop(accepted);
        drop(client);
        drop(first);

        let second = bind_listener(address)
            .await
            .expect("the port must be usable again right away");
        assert_eq!(second.local_addr().expect("addr").port(), address.port());
    }
}
