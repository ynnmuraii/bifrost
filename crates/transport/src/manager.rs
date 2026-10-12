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
    /// The liveness cadence every link runs with: `workspace.link_ping_ms` /
    /// `workspace.link_timeout_ms` from the application's configuration.
    pub liveness: policy::LinkLiveness,
    /// The reconnect backoff bounds: `network.reconnect_backoff_ms` from the application's
    /// configuration.
    pub backoff: policy::BackoffBounds,
}

/// What the manager keeps per live link.
struct LiveLink {
    handle: LinkHandle,
    direction: Direction,
    initiator: NodeId,
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
    /// Keyed by peer **and** direction: two nodes that dial each other have two links
    /// between them at the same time, and both are registered before either `Hello`
    /// exchange finishes. Keying by the peer alone let the second registration clobber
    /// the first one's handle — which killed the first link, made the manager drop its
    /// `Up`, and left the pair split across two different TCP connections instead of the
    /// one the dedup rule picked.
    pending: BTreeMap<(NodeId, Direction), PendingLink>,
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
        let listener = bind_listener(self.config.listen).await?;
        let local_hello = self.config.local_hello.clone();
        let local = self.config.identity.node_id();

        // One dialer per configured peer.
        let settings = LinkSettings {
            queue_capacity: self.config.queue_capacity,
            liveness: self.config.liveness,
            backoff: self.config.backoff,
        };
        for address in self.config.peers.clone() {
            let messages = self.messages_tx.clone();
            let identity = Arc::clone(&self.config.identity);
            let trust = self.config.trust.clone();
            let hello = local_hello.clone();
            tokio::spawn(async move {
                dial_loop(address, identity, trust, hello, messages, settings).await;
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
                    tokio::spawn(async move {
                        accept_one(acceptor, tcp, address, identity, hello, messages, settings)
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
            ManagerMessage::Register {
                peer,
                direction,
                handle,
                task,
            } => {
                let initiator = if direction.is_outgoing() { local } else { peer };
                self.inner.lock().await.pending.insert(
                    (peer, direction),
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
                    direction,
                    hello,
                } => self.on_up(peer, direction, hello).await,
                LinkReport::Message { peer, message } => {
                    let _ = self
                        .events_tx
                        .send(LinkEvent::Message { peer, message })
                        .await;
                }
                LinkReport::Down {
                    peer,
                    direction,
                    reason,
                } => {
                    let removed = {
                        let mut inner = self.inner.lock().await;
                        inner.pending.remove(&(peer, direction));
                        // Only the link that went down may leave the member list: with two
                        // links to one peer, the other direction's `Down` must not evict the
                        // live one.
                        match inner.links.get(&peer) {
                            Some(live) if live.direction == direction => {
                                inner.links.remove(&peer);
                                true
                            }
                            _ => false,
                        }
                    };
                    if removed || reason == LinkDownReason::MembershipFull {
                        let _ = self.events_tx.send(LinkEvent::Down { peer, reason }).await;
                    }
                }
            },
        }
    }

    async fn on_up(&self, peer: NodeId, direction: Direction, hello: PeerInfo) {
        let incoming = self.inner.lock().await.pending.remove(&(peer, direction));
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
                direction,
                initiator: pending.initiator,
                task: pending.task,
            },
        );
        let _ = self.events_tx.send(LinkEvent::Up { peer, hello }).await;
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
async fn bind_listener(address: SocketAddr) -> std::io::Result<TcpListener> {
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

async fn accept_one(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    address: SocketAddr,
    identity: Arc<NodeIdentity>,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
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
        settings,
    )
    .await;
}

async fn dial_loop(
    address: SocketAddr,
    identity: Arc<NodeIdentity>,
    trust: TrustStore,
    hello: wire::Hello,
    messages: mpsc::Sender<ManagerMessage>,
    settings: LinkSettings,
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
                    settings,
                )
                .await;
                attempt = 0;
            }
        }
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(policy::reconnect_delay(attempt, settings.backoff)).await;
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
    settings: LinkSettings,
) {
    let (reports_tx, mut reports_rx) = mpsc::channel::<LinkReport>(64);
    let Ok(link) = crate::link::spawn(LinkConfig {
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
