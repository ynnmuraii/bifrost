//! One readiness-blocking UDP reader shared by up to three pinned media sessions.
//! Routing validates framing only; authentication and replay state belong to each session.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use audio_model::packet::{validate_datagram, HEADER_LEN, MAX_DATAGRAM_LEN, TAG_LEN};
use mio::{Events, Interest, Poll, Token, Waker};
use rtrb::{Consumer, Producer, RingBuffer};

const MAX_ROUTES: usize = 3;
const PACKET_QUEUE_CAPACITY: usize = 64;

/// Single-worker notification. Producers, including native callbacks, never lock or allocate.
#[derive(Debug, Default)]
pub struct Notification {
    pending: AtomicBool,
    worker: OnceLock<Thread>,
}

impl Notification {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn notify(&self) {
        self.pending.store(true, Ordering::Release);
        if let Some(worker) = self.worker.get() {
            worker.unpark();
        }
    }

    /// Only one worker may wait on a notification over its lifetime.
    /// A pending notification is consumed without parking; spurious wakes are allowed.
    pub fn wait_timeout(&self, timeout: Duration) {
        self.worker.get_or_init(thread::current);
        if !self.pending.swap(false, Ordering::AcqRel) {
            // An intervening notify leaves an unpark token, even before park begins.
            thread::park_timeout(timeout);
        }
    }
}

struct Packet {
    len: usize,
    bytes: [u8; MAX_DATAGRAM_LEN],
}

struct Route {
    session_id: [u8; 16],
    peer: SocketAddr,
    token: Arc<()>,
    packets: Producer<Box<Packet>>,
    recycled: Consumer<Box<Packet>>,
    rejected: Arc<AtomicU64>,
    notify: Arc<Notification>,
}

struct State {
    socket: UdpSocket,
    routes: Mutex<[Option<Route>; MAX_ROUTES]>,
    stop: AtomicBool,
    rejected: AtomicU64,
}

/// Owns the socket and reader lifetime. The reader owns only `State`, never this owner.
pub struct MediaSocket {
    state: Arc<State>,
    wake: Waker,
    reader: Option<JoinHandle<()>>,
}

impl MediaSocket {
    pub fn new(socket: UdpSocket) -> io::Result<Arc<Self>> {
        socket.set_nonblocking(true)?;
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        let poll = Poll::new()?;
        let mut receiver_socket = mio::net::UdpSocket::from_std(socket.try_clone()?);
        poll.registry()
            .register(&mut receiver_socket, Token(0), Interest::READABLE)?;
        let wake = Waker::new(poll.registry(), Token(1))?;
        let state = Arc::new(State {
            socket,
            routes: Mutex::new(std::array::from_fn(|_| None)),
            stop: AtomicBool::new(false),
            rejected: AtomicU64::new(0),
        });
        let receiver = Arc::clone(&state);
        let reader = thread::Builder::new()
            .name("audio-udp".into())
            .spawn(move || receive_loop(receiver, poll, receiver_socket))?;
        Ok(Arc::new(Self {
            state,
            wake,
            reader: Some(reader),
        }))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.state.socket.local_addr()
    }

    pub fn rejected_datagrams(&self) -> u64 {
        self.state.rejected.load(Ordering::Relaxed)
    }

    pub fn register(
        self: &Arc<Self>,
        session_id: [u8; 16],
        peer: SocketAddr,
        rejected: Arc<AtomicU64>,
        notify: Arc<Notification>,
    ) -> io::Result<SessionEndpoint> {
        let mut routes = self.state.routes.lock().unwrap_or_else(|e| e.into_inner());
        if routes.iter().flatten().any(|r| r.session_id == session_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "media session already registered",
            ));
        }
        let slot = routes.iter_mut().find(|r| r.is_none()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::StorageFull,
                "media socket has three sessions",
            )
        })?;
        let (packets, consumer) = RingBuffer::new(PACKET_QUEUE_CAPACITY);
        // Transfer pointers through the rings; initialize packet storage only at registration.
        let (mut recycle, recycled) = RingBuffer::new(PACKET_QUEUE_CAPACITY);
        for _ in 0..PACKET_QUEUE_CAPACITY {
            assert!(recycle
                .push(Box::new(Packet {
                    len: 0,
                    bytes: [0; MAX_DATAGRAM_LEN],
                }))
                .is_ok());
        }
        let token = Arc::new(());
        *slot = Some(Route {
            session_id,
            peer,
            token: Arc::clone(&token),
            packets,
            recycled,
            rejected,
            notify: Arc::clone(&notify),
        });
        Ok(SessionEndpoint {
            owner: Arc::clone(self),
            token,
            peer,
            packets: consumer,
            recycle,
            notify,
        })
    }
}

impl Drop for MediaSocket {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        // This does not depend on a routable bound address or UDP delivery.
        let _ = self.wake.wake();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// A unique registration and its preallocated single-consumer packet queue.
pub struct SessionEndpoint {
    owner: Arc<MediaSocket>,
    token: Arc<()>,
    peer: SocketAddr,
    packets: Consumer<Box<Packet>>,
    recycle: Producer<Box<Packet>>,
    notify: Arc<Notification>,
}

impl SessionEndpoint {
    /// All sessions send through their owner's one socket, always to their pinned peer.
    pub fn send(&self, bytes: &[u8]) -> io::Result<usize> {
        if !(HEADER_LEN + TAG_LEN..=MAX_DATAGRAM_LEN).contains(&bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid media datagram size",
            ));
        }
        self.owner
            .state
            .socket
            .send_to(bytes, self.peer)
            .map_err(session_send_error)
    }

    pub fn try_recv(&mut self, bytes: &mut [u8]) -> io::Result<Option<usize>> {
        let Ok(packet) = self.packets.peek() else {
            return Ok(None);
        };
        if bytes.len() < packet.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media receive buffer too small",
            ));
        }
        // A short caller buffer does not consume the packet.
        let packet = self.packets.pop().expect("sole consumer peeked a packet");
        bytes[..packet.len].copy_from_slice(&packet.bytes[..packet.len]);
        let len = packet.len;
        assert!(
            self.recycle.push(packet).is_ok(),
            "packet pool cannot overflow"
        );
        Ok(Some(len))
    }

    pub fn notification(&self) -> &Arc<Notification> {
        &self.notify
    }
}

fn session_send_error(error: io::Error) -> io::Error {
    // Asynchronous ICMP may refer to another session's earlier send on this socket.
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
    ) || (cfg!(windows) && error.raw_os_error() == Some(10054))
    {
        io::ErrorKind::WouldBlock.into()
    } else {
        error
    }
}

impl Drop for SessionEndpoint {
    fn drop(&mut self) {
        let mut routes = self
            .owner
            .state
            .routes
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = routes.iter_mut().find(|route| {
            route
                .as_ref()
                .is_some_and(|r| Arc::ptr_eq(&r.token, &self.token))
        }) {
            *slot = None;
        }
    }
}

fn receive_loop(state: Arc<State>, mut poll: Poll, socket: mio::net::UdpSocket) {
    // Receive the entire UDP payload on both OSes (Windows errors on truncation).
    let mut bytes = [0u8; 65_536];
    let mut events = Events::with_capacity(2);
    while !state.stop.load(Ordering::Acquire) {
        let (len, peer) = match socket.recv_from(&mut bytes) {
            Ok(received) => received,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                match poll.poll(&mut events, None) {
                    Ok(()) => continue,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            // UDP ICMP errors concern a past send, not the lifetime of this reader.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(_) => break,
        };
        if state.stop.load(Ordering::Acquire) {
            break;
        }
        if len > MAX_DATAGRAM_LEN {
            state.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let header = match validate_datagram(&bytes[..len]) {
            Ok(header) => header,
            Err(_) => {
                state.rejected.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let mut routes = state.routes.lock().unwrap_or_else(|e| e.into_inner());
        let Some(route) = routes
            .iter_mut()
            .flatten()
            .find(|r| r.session_id == header.session_id)
        else {
            state.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        if route.peer != peer || route.packets.slots() == 0 {
            route.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Ok(mut packet) = route.recycled.pop() else {
            route.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        packet.len = len;
        packet.bytes[..len].copy_from_slice(&bytes[..len]);
        if route.packets.push(packet).is_err() {
            route.rejected.fetch_add(1, Ordering::Relaxed);
        } else {
            route.notify.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_model::packet::{Header, PacketKind, HEADER_LEN, TAG_LEN};
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::{mpsc, Barrier};
    use std::time::Instant;

    fn udp() -> UdpSocket {
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap()
    }

    #[test]
    fn shared_socket_icmp_is_transient_without_hiding_other_send_errors() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::WouldBlock,
        ] {
            assert_eq!(
                session_send_error(kind.into()).kind(),
                io::ErrorKind::WouldBlock,
            );
        }
        #[cfg(windows)]
        assert_eq!(
            session_send_error(io::Error::from_raw_os_error(10054)).kind(),
            io::ErrorKind::WouldBlock,
        );
        assert_eq!(
            session_send_error(io::ErrorKind::PermissionDenied.into()).kind(),
            io::ErrorKind::PermissionDenied,
        );
        assert_eq!(
            session_send_error(io::Error::from_raw_os_error(1234)).raw_os_error(),
            Some(1234),
        );
    }

    fn datagram(id: u8) -> [u8; HEADER_LEN + TAG_LEN] {
        let header = Header {
            kind: PacketKind::Keepalive,
            flags: 0,
            session_id: [id; 16],
            key_epoch: 1,
            packet_number: 0,
            media_timestamp: 0,
            frame_samples: 0,
            payload_len: 0,
        };
        let mut bytes = [0; HEADER_LEN + TAG_LEN];
        bytes[..HEADER_LEN].copy_from_slice(&header.encode().unwrap());
        bytes
    }

    fn register(
        owner: &Arc<MediaSocket>,
        id: u8,
        peer: &UdpSocket,
    ) -> (SessionEndpoint, Arc<AtomicU64>) {
        let rejected = Arc::new(AtomicU64::new(0));
        let endpoint = owner
            .register(
                [id; 16],
                peer.local_addr().unwrap(),
                Arc::clone(&rejected),
                Arc::new(Notification::new()),
            )
            .unwrap();
        (endpoint, rejected)
    }

    fn until(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready() {
            assert!(
                Instant::now() < deadline,
                "UDP reader did not make progress"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn receive(endpoint: &mut SessionEndpoint) -> Vec<u8> {
        let mut bytes = [0; MAX_DATAGRAM_LEN];
        let mut len = None;
        until(|| {
            len = endpoint.try_recv(&mut bytes).unwrap();
            len.is_some()
        });
        bytes[..len.unwrap()].to_vec()
    }

    #[test]
    fn three_routes_share_one_port_in_both_directions() {
        let owner = MediaSocket::new(udp()).unwrap();
        let local = owner.local_addr().unwrap();
        let peers = [udp(), udp(), udp()];
        let mut endpoints: Vec<_> = peers
            .iter()
            .enumerate()
            .map(|(index, peer)| register(&owner, index as u8 + 1, peer).0)
            .collect();
        for (index, peer) in peers.iter().enumerate() {
            peer.send_to(&datagram(index as u8 + 1), local).unwrap();
        }
        for (index, endpoint) in endpoints.iter_mut().enumerate() {
            let bytes = datagram(index as u8 + 1);
            assert_eq!(receive(endpoint), bytes);
            assert_eq!(endpoint.send(&bytes).unwrap(), bytes.len());
            peers[index]
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut received = [0; MAX_DATAGRAM_LEN];
            let (len, sender) = peers[index].recv_from(&mut received).unwrap();
            assert_eq!(sender, local);
            assert_eq!(&received[..len], &bytes);
        }
        assert!(owner
            .register(
                [4; 16],
                peers[0].local_addr().unwrap(),
                Arc::new(AtomicU64::new(0)),
                Arc::new(Notification::new()),
            )
            .is_err());
        assert!(owner
            .register(
                [1; 16],
                peers[0].local_addr().unwrap(),
                Arc::new(AtomicU64::new(0)),
                Arc::new(Notification::new()),
            )
            .is_err());
    }

    #[test]
    fn wrong_tuple_does_not_poison_session_or_packet_number() {
        let owner = MediaSocket::new(udp()).unwrap();
        let a = udp();
        let b = udp();
        let (mut endpoint, rejected) = register(&owner, 1, &a);
        let (_other, other_rejected) = register(&owner, 2, &b);
        let bytes = datagram(1);
        b.send_to(&bytes, owner.local_addr().unwrap()).unwrap();
        until(|| rejected.load(Ordering::Relaxed) == 1);
        assert_eq!(endpoint.try_recv(&mut [0; MAX_DATAGRAM_LEN]).unwrap(), None);
        a.send_to(&bytes, owner.local_addr().unwrap()).unwrap();
        assert_eq!(receive(&mut endpoint), bytes);
        assert_eq!(other_rejected.load(Ordering::Relaxed), 0);
        assert_eq!(owner.rejected_datagrams(), 0);
    }

    #[test]
    fn unknown_malformed_and_oversized_packets_are_silent() {
        let owner = MediaSocket::new(udp()).unwrap();
        let peer = udp();
        let (mut endpoint, rejected) = register(&owner, 1, &peer);
        let local = owner.local_addr().unwrap();
        peer.send_to(&datagram(2), local).unwrap();
        peer.send_to(&[0; 4], local).unwrap();
        peer.send_to(&[0; MAX_DATAGRAM_LEN + 1], local).unwrap();
        until(|| owner.rejected_datagrams() == 3);
        assert_eq!(endpoint.try_recv(&mut [0; MAX_DATAGRAM_LEN]).unwrap(), None);
        assert_eq!(rejected.load(Ordering::Relaxed), 0);
        peer.set_nonblocking(true).unwrap();
        assert_eq!(
            peer.recv_from(&mut [0; MAX_DATAGRAM_LEN])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn queue_overflow_is_bounded_and_session_local() {
        let owner = MediaSocket::new(udp()).unwrap();
        let peer = udp();
        let (mut endpoint, rejected) = register(&owner, 1, &peer);
        // Wait for each arrival so this tests the route queue, not kernel UDP loss.
        for index in 0..=PACKET_QUEUE_CAPACITY {
            peer.send_to(&datagram(1), owner.local_addr().unwrap())
                .unwrap();
            until(|| {
                if index < PACKET_QUEUE_CAPACITY {
                    endpoint.packets.slots() == index + 1
                } else {
                    rejected.load(Ordering::Relaxed) == 1
                }
            });
        }
        for _ in 0..PACKET_QUEUE_CAPACITY {
            assert_eq!(receive(&mut endpoint), datagram(1));
        }
        assert_eq!(endpoint.try_recv(&mut [0; MAX_DATAGRAM_LEN]).unwrap(), None);
        assert_eq!(owner.rejected_datagrams(), 0);
    }

    #[test]
    fn unregister_and_reregister_do_not_reuse_queued_packets_or_peer() {
        let owner = MediaSocket::new(udp()).unwrap();
        let a = udp();
        let b = udp();
        let (old, _) = register(&owner, 1, &a);
        a.send_to(&datagram(1), owner.local_addr().unwrap())
            .unwrap();
        until(|| old.packets.slots() == 1);
        drop(old);
        let (mut new, rejected) = register(&owner, 1, &b);
        assert_eq!(new.try_recv(&mut [0; MAX_DATAGRAM_LEN]).unwrap(), None);
        a.send_to(&datagram(1), owner.local_addr().unwrap())
            .unwrap();
        until(|| rejected.load(Ordering::Relaxed) == 1);
        b.send_to(&datagram(1), owner.local_addr().unwrap())
            .unwrap();
        assert_eq!(receive(&mut new), datagram(1));
    }

    #[test]
    fn invalid_sizes_never_copy_or_consume_a_packet() {
        let owner = MediaSocket::new(udp()).unwrap();
        let peer = udp();
        let (mut endpoint, _) = register(&owner, 1, &peer);
        assert_eq!(
            endpoint
                .send(&[0; MAX_DATAGRAM_LEN + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        peer.send_to(&datagram(1), owner.local_addr().unwrap())
            .unwrap();
        until(|| endpoint.packets.slots() == 1);
        let mut short = [0xaa; HEADER_LEN];
        assert_eq!(
            endpoint.try_recv(&mut short).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(short, [0xaa; HEADER_LEN]);
        assert_eq!(receive(&mut endpoint), datagram(1));
    }

    fn releases_bind(bind: SocketAddr) {
        let owner = MediaSocket::new(UdpSocket::bind(bind).unwrap()).unwrap();
        let local = owner.local_addr().unwrap();
        let state = Arc::downgrade(&owner.state);
        let peer = udp();
        let (endpoint, _) = register(&owner, 1, &peer);
        drop(owner);
        assert!(state.upgrade().is_some(), "endpoint must keep socket alive");
        drop(endpoint);
        assert!(
            state.upgrade().is_none(),
            "reader must be joined before drop returns"
        );
        let _rebound = UdpSocket::bind(local).unwrap();
    }

    #[test]
    fn final_endpoint_reaps_reader_and_releases_ipv4_bind() {
        releases_bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        releases_bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
    }

    #[test]
    fn final_owner_reaps_reader_and_releases_ipv6_bind() {
        for ip in [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED] {
            let socket = match UdpSocket::bind((ip, 0)) {
                Ok(socket) => socket,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                    ) =>
                {
                    return
                }
                Err(error) => panic!("IPv6 bind failed: {error}"),
            };
            let local = socket.local_addr().unwrap();
            let owner = MediaSocket::new(socket).unwrap();
            let state = Arc::downgrade(&owner.state);
            drop(owner);
            assert!(state.upgrade().is_none());
            let _rebound = UdpSocket::bind(local).unwrap();
        }
    }

    #[test]
    fn shutdown_waker_reaps_idle_reader_without_udp_self_delivery() {
        let socket = udp();
        let peer = udp();
        // A connected UDP socket filters out self-sent datagrams: shutdown must
        // use the readiness waker, not successful delivery back to its bind address.
        socket.connect(peer.local_addr().unwrap()).unwrap();
        let local = socket.local_addr().unwrap();
        let owner = MediaSocket::new(socket).unwrap();
        let state = Arc::downgrade(&owner.state);
        let (finished, completion) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            drop(owner);
            finished.send(()).unwrap();
        });
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        shutdown.join().unwrap();
        assert!(state.upgrade().is_none());
        let _rebound = UdpSocket::bind(local).unwrap();
    }

    #[test]
    fn notification_has_no_lost_wake_before_or_during_wait() {
        let notification = Arc::new(Notification::new());
        let barrier = Arc::new(Barrier::new(2));
        let (ready, events) = mpsc::channel();
        let (acknowledge, acknowledgements) = mpsc::channel();
        notification.notify();
        let worker_notification = Arc::clone(&notification);
        let worker_barrier = Arc::clone(&barrier);
        let worker = thread::spawn(move || {
            // The notification predates registration of the cached thread.
            worker_notification.wait_timeout(Duration::from_secs(2));
            ready.send(()).unwrap();
            for _ in 0..100 {
                worker_barrier.wait();
                worker_notification.wait_timeout(Duration::from_secs(2));
                ready.send(()).unwrap();
                acknowledgements.recv().unwrap();
            }
        });
        events.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..100 {
            barrier.wait();
            notification.notify();
            events.recv_timeout(Duration::from_secs(1)).unwrap();
            acknowledge.send(()).unwrap();
        }
        worker.join().unwrap();
    }
}
