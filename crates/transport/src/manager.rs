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

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use focus_router as fr;
use geometry::NodeId;
use identity::{peer_node_id, trust::TrustStore, NodeIdentity};
use protocol::wire;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::convert::PeerInfo;
use crate::link::{
    Direction, LinkConfig, LinkDownReason, LinkEvent, LinkHandle, LinkReport, Stream,
};
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
}

/// What the manager keeps per live link.
struct LiveLink {
    handle: LinkHandle,
    initiator: NodeId,
    /// The link's task, so [`LinkManager::kill_all`] can drop it without a `Shutdown`.
    task: tokio::task::JoinHandle<()>,
}

/// A registered-but-not-yet-established link.
struct PendingLink {
    handle: LinkHandle,
    initiator: NodeId,
    task: tokio::task::JoinHandle<()>,
}

/// Messages from the accept/dial tasks into the manager loop.
enum ManagerMessage {
    /// A link was spawned; the manager takes ownership of its handle.
    Register {
        peer: NodeId,
        direction: Direction,
        handle: LinkHandle,
        task: tokio::task::JoinHandle<()>,
    },
    /// A link's own report.
    Report(LinkReport),
}

/// The mesh of links around this node.
pub struct LinkManager {
    config: ManagerConfig,
    messages_tx: mpsc::Sender<ManagerMessage>,
    /// Taken by [`LinkManager::run`]; behind a mutex so `run` can take `&self` and the
    /// manager can live behind an `Arc` while the application queries it.
    messages_rx: Mutex<mpsc::Receiver<ManagerMessage>>,
    events_tx: mpsc::Sender<LinkEvent>,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    links: BTreeMap<NodeId, LiveLink>,
    pending: BTreeMap<NodeId, PendingLink>,
}

impl LinkManager {
    /// Builds a manager and the receiver of application events.
    pub fn new(config: ManagerConfig) -> Result<(Self, mpsc::Receiver<LinkEvent>), TransportError> {
        let (events_tx, events_rx) = mpsc::channel(256);
        let (messages_tx, messages_rx) = mpsc::channel(256);
        Ok((
            Self {
                config,
                messages_tx,
                messages_rx: Mutex::new(messages_rx),
                events_tx,
                inner: Arc::new(Mutex::new(Inner::default())),
            },
            events_rx,
        ))
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

    /// The current members (established links).
    pub async fn members(&self) -> Vec<NodeId> {
        self.inner.lock().await.links.keys().copied().collect()
    }

    /// The member count, for the membership gate.
    pub async fn member_count(&self) -> usize {
        self.inner.lock().await.links.len()
    }

    /// Drops every link abruptly, without a `Shutdown`.
    ///
    /// A peer cannot tell this apart from a crashed process, which is exactly what the e2e
    /// timing of `docs/05-protocol.md` (offline after 3 s without `Pong`) is about. The
    /// graceful path is [`LinkHandle::close`] on each handle plus letting the manager's loop
    /// keep running.
    pub async fn kill_all(&self) {
        let mut inner = self.inner.lock().await;
        for (_, link) in std::mem::take(&mut inner.links) {
            link.task.abort();
        }
        for (_, pending) in std::mem::take(&mut inner.pending) {
            pending.task.abort();
        }
    }

    /// Runs until the process ends: listens, dials, dedups, supervises.
    pub async fn run(&self) -> Result<(), TransportError> {
        let server_config =
            identity::trusted_server_config(&self.config.identity, &self.config.trust)?;
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind(self.config.listen).await?;
        let local_hello = self.config.local_hello.clone();
        let local = self.config.identity.node_id();

        // One dialer per configured peer.
        for address in self.config.peers.clone() {
            let messages = self.messages_tx.clone();
            let identity = Arc::clone(&self.config.identity);
            let trust = self.config.trust.clone();
            let hello = local_hello.clone();
            let capacity = self.config.queue_capacity;
            tokio::spawn(async move {
                dial_loop(address, identity, trust, hello, messages, capacity).await;
            });
        }

        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (tcp, address) = accepted?;
                    let messages = self.messages_tx.clone();
                    let identity = Arc::clone(&self.config.identity);
                    let acceptor = acceptor.clone();
                    let hello = local_hello.clone();
                    let capacity = self.config.queue_capacity;
                    tokio::spawn(async move {
                        accept_one(acceptor, tcp, address, identity, hello, messages, capacity).await;
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
            ManagerMessage::Register {
                peer,
                direction,
                handle,
                task,
            } => {
                let initiator = if direction.is_outgoing() { local } else { peer };
                self.inner.lock().await.pending.insert(
                    peer,
                    PendingLink {
                        handle,
                        initiator,
                        task,
                    },
                );
            }
            ManagerMessage::Report(report) => match report {
                LinkReport::Up {
                    peer,
                    direction: _,
                    hello,
                } => self.on_up(peer, hello).await,
                LinkReport::Message { peer, message } => {
                    let _ = self
                        .events_tx
                        .send(LinkEvent::Message { peer, message })
                        .await;
                }
                LinkReport::Down { peer, reason } => {
                    let removed = {
                        let mut inner = self.inner.lock().await;
                        inner.pending.remove(&peer);
                        inner.links.remove(&peer).is_some()
                    };
                    if removed || reason == LinkDownReason::MembershipFull {
                        let _ = self.events_tx.send(LinkEvent::Down { peer, reason }).await;
                    }
                }
            },
        }
    }

    async fn on_up(&self, peer: NodeId, hello: PeerInfo) {
        let incoming = self.inner.lock().await.pending.remove(&peer);
        let Some(pending) = incoming else {
            return;
        };

        // Simultaneous dial: both sides compute the same winner from the two identities, so
        // the losing link is closed on both ends without any negotiation.
        let (existing_initiator, replaced) = {
            let inner = self.inner.lock().await;
            match inner.links.get(&peer) {
                Some(existing) => {
                    let keep_incoming =
                        policy::prefer_incoming(&existing.initiator, &pending.initiator);
                    (Some(existing.initiator), keep_incoming)
                }
                None => (None, false),
            }
        };
        if existing_initiator.is_some() && !replaced {
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
            if let Err(full) = policy::admit_member(member_count, self.config.max_members) {
                let _ = full;
                let _ = pending
                    .handle
                    .send(fr::Message::ProtocolError {
                        code: fr::ErrorCode::MembershipFull,
                        related_seq: 0,
                    })
                    .await;
                pending.handle.close();
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

        self.inner.lock().await.links.insert(
            peer,
            LiveLink {
                handle: pending.handle,
                initiator: pending.initiator,
                task: pending.task,
            },
        );
        let _ = self.events_tx.send(LinkEvent::Up { peer, hello }).await;
    }
}

async fn accept_one(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    address: SocketAddr,
    identity: Arc<NodeIdentity>,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    queue_capacity: usize,
) {
    let _ = address;
    let stream = match acceptor.accept(tcp).await {
        Ok(stream) => stream,
        Err(_) => return,
    };
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
    start_link(
        peer,
        Direction::Incoming,
        Stream::Server(Box::new(stream)),
        identity.node_id(),
        hello,
        messages,
        queue_capacity,
    )
    .await;
}

async fn dial_loop(
    address: SocketAddr,
    identity: Arc<NodeIdentity>,
    trust: TrustStore,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    queue_capacity: usize,
) {
    let client_config = match identity::trusted_client_config(&identity, &trust) {
        Ok(config) => config,
        Err(_) => return,
    };
    let connector = TlsConnector::from(Arc::new(client_config));
    let mut attempt = 0u32;
    loop {
        let connected = async {
            let tcp = TcpStream::connect(address).await.ok()?;
            connector
                .connect(identity::server_name(address.ip()), tcp)
                .await
                .ok()
        }
        .await;
        if let Some(stream) = connected {
            let peer = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .and_then(|certificate| peer_node_id(std::slice::from_ref(certificate)).ok());
            if let Some(peer) = peer {
                start_link(
                    peer,
                    Direction::Outgoing,
                    Stream::Client(Box::new(stream)),
                    identity.node_id(),
                    hello.clone(),
                    messages.clone(),
                    queue_capacity,
                )
                .await;
                attempt = 0;
            }
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(policy::reconnect_delay(attempt)).await;
    }
}

/// Spawns a link, registers it with the manager and waits for it to end.
async fn start_link(
    peer: NodeId,
    direction: Direction,
    stream: Stream,
    local: NodeId,
    local_hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    queue_capacity: usize,
) {
    let (reports_tx, mut reports_rx) = mpsc::channel::<LinkReport>(64);
    let Ok(link) = crate::link::spawn(LinkConfig {
        peer,
        local,
        direction,
        local_hello,
        stream,
        reports: reports_tx,
        queue_capacity,
    }) else {
        return;
    };
    let handle = link.handle.clone();
    if messages
        .send(ManagerMessage::Register {
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
