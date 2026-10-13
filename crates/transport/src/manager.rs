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
use std::net::SocketAddr;
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

/// A monotonic millisecond clock.
///
/// `crates/transport` has no clock helper, and the pairing window is bounded in milliseconds
/// (`docs/05-protocol.md`: «окно сопряжения … на 120 s»), so the manager keeps its own
/// `Instant` origin — the same idiom `apps/lws-core/src/session.rs` uses. A monotonic origin
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
    task: tokio::task::JoinHandle<()>,
    /// The peer this link is for, so a report can be matched without the live map's key.
    peer: NodeId,
    /// When the manager registered it, on its monotonic clock.
    ///
    /// Compared against a live link's [`LiveLink::up_at_ns`] to tell a simultaneous dial (the
    /// newcomer was already forming) from a late redial (it started afterwards).
    registered_at_ns: u128,
}

/// Messages from the accept/dial tasks into the manager loop.
enum ManagerMessage {
    /// A link was spawned; the manager takes ownership of its handle.
    Register {
        key: [u8; 16],
        peer: NodeId,
        direction: Direction,
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
    AddPeer { peer: NodeId, address: SocketAddr },
    /// Stops the dialer created for a freshly paired peer.
    RemovePeer { peer: NodeId },
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
    /// Replaced by display updates; every future link uses the latest Hello.
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
    /// Dial permits not yet granted, per peer: the dialers waiting for that peer's link to go.
    ///
    /// This is what makes a dialer idle while its peer is linked. Without it, a dialer whose
    /// link lost the dedup looped forever: redial, get refused at `Up`, redial again — a TLS
    /// handshake every backoff, for the life of the mesh.
    dial_waiters: BTreeMap<NodeId, Vec<oneshot::Sender<DialTurn>>>,
}

impl Inner {
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
                clock: Clock::new(),
            },
            events_rx,
        ))
    }

    /// Adds the newly committed trust pin and starts dialing its recorded address.
    pub async fn add_peer(&self, peer: TrustedPeer) -> Result<(), TransportError> {
        let raw_address = peer.last_address.as_deref().ok_or_else(|| {
            TransportError::InvalidPeerAddress("the paired peer has no address".to_owned())
        })?;
        let address = raw_address.parse::<SocketAddr>().map_err(|error| {
            TransportError::InvalidPeerAddress(format!("{raw_address}: {error}"))
        })?;
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

    /// Removes a peer from the live verifier and stops its dynamic dialer.
    pub async fn remove_peer(&self, peer: &NodeId) -> Result<(), TransportError> {
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

    /// Replaces the Hello carried by links formed from now on.
    pub async fn update_local_hello(&self, hello: wire::Hello) {
        *self.local_hello.write().await = hello;
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

    /// Runs until the process ends: listens, dials, dedups, supervises.
    pub async fn run(&self) -> Result<(), TransportError> {
        // The verifier reads this shared set for each new certificate, so pairing updates
        // take effect without rebuilding the listener or restarting the process.
        let link_config = Arc::new(identity::server_config(
            &self.identity,
            TrustPolicy::PinnedShared(Arc::clone(&self.trusted_peers)),
            &identity::LINK_ALPN,
        )?);
        let pairing_config = Arc::new(identity::pairing_server_config(&self.identity)?);
        let listener = bind_listener(self.listen).await?;
        let local = self.identity.node_id();
        let settings = self.settings;

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
                accepted = listener.accept() => {
                    let (tcp, address) = accepted?;
                    // Lease frames must not wait for a delayed TCP ACK during idle input.
                    tcp.set_nodelay(true)?;
                    let messages = self.messages_tx.clone();
                    let events = self.events_tx.clone();
                    let identity = Arc::clone(&self.identity);
                    let link_config = Arc::clone(&link_config);
                    let pairing_config = Arc::clone(&pairing_config);
                    let gate = self.pairing.clone();
                    let hello = self.local_hello.read().await.clone();
                    let clock = self.clock;
                    tokio::spawn(async move {
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
                        )
                        .await;
                    });
                }
                message = async { self.messages_rx.lock().await.recv().await } => {
                    let Some(message) = message else { return Ok(()) };
                    self.on_message(message, local).await;
                }
            }
        }
    }

    async fn on_message(&self, message: ManagerMessage, local: NodeId) {
        match message {
            ManagerMessage::AddPeer { peer, address } => {
                let messages = self.messages_tx.clone();
                let identity = Arc::clone(&self.identity);
                let trusted_peers = Arc::clone(&self.trusted_peers);
                let local_hello = Arc::clone(&self.local_hello);
                let settings = self.settings;
                let dialer = tokio::spawn(async move {
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
                });
                if let Some(previous) = self.inner.lock().await.peer_dialers.insert(peer, dialer) {
                    previous.abort();
                }
            }
            ManagerMessage::RemovePeer { peer } => {
                if let Some(dialer) = self.inner.lock().await.peer_dialers.remove(&peer) {
                    dialer.abort();
                }
            }
            ManagerMessage::Register {
                key,
                peer,
                direction,
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
                        task,
                        registered_at_ns,
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
                    let keep_incoming = simultaneous
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
            let mut inner = self.inner.lock().await;
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
) {
    let acceptor = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), tcp);
    let start = match acceptor.await {
        Ok(start) => start,
        Err(_) => return,
    };
    let wants_pairing = start
        .client_hello()
        .alpn()
        .is_some_and(|mut alpn| alpn.any(|protocol| protocol == limits::ALPN_PAIR.as_bytes()));

    if wants_pairing {
        // Resolve the gate before the handshake: an error here is the documented refusal.
        let window = match gate.admit(clock.now_ms()) {
            Ok(window) => window,
            Err(_reason) => return,
        };
        let stream = match start.into_stream(pairing_config).await {
            Ok(stream) => stream,
            Err(_) => return,
        };
        let _ = events
            .send(LinkEvent::Pairing(Box::new(PairingConnection {
                stream,
                address,
                window,
            })))
            .await;
        return;
    }

    let stream = match start.into_stream(link_config).await {
        Ok(stream) => stream,
        Err(_) => return,
    };
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
            connector
                .connect(identity::server_name(address.ip()), tcp)
                .await
                .ok()
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
