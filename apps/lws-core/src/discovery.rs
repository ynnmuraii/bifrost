//! Untrusted LAN routing hints. Only core owns these sockets; trust remains in mTLS/SAS.
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use geometry::NodeId;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

pub(crate) const PORT: u16 = 47902;
const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 47, 90);
const MAX_PACKET: usize = 1024;
const CAP: usize = 128;
const TTL: Duration = Duration::from_secs(30);
const PERIOD: Duration = Duration::from_secs(5);

fn parse_id(text: &str) -> Option<NodeId> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(NodeId::new(bytes))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Beacon {
    pub format: u8,
    pub node_id: String,
    pub display_name: String,
    #[serde(rename = "listen_port")]
    pub port: u16,
    pub protocol_version: u32,
    pub pairing_open: bool,
    pub system_name: String,
}

impl Beacon {
    pub fn encode(&self) -> Option<Vec<u8>> {
        let bytes = serde_json::to_vec(self).ok()?;
        (bytes.len() <= MAX_PACKET).then_some(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() || bytes.len() > MAX_PACKET {
            return None;
        }
        let mut beacon: Self = serde_json::from_slice(bytes).ok()?;
        if beacon.format != 1
            || beacon.port == 0
            || beacon.protocol_version != protocol::PROTOCOL_MAJOR
        {
            return None;
        }
        parse_id(&beacon.node_id)?;
        beacon.display_name = protocol::sanitize_system_name(&beacon.display_name);
        beacon.system_name = protocol::sanitize_system_name(&beacon.system_name);
        if beacon.display_name.is_empty() {
            return None;
        }
        Some(beacon)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Advertisement {
    pub enabled: bool,
    pub advertise: bool,
    pub beacon: Beacon,
}

pub(crate) struct Received {
    pub beacon: Beacon,
    pub address: SocketAddr,
}

struct Entry {
    peer: protocol::ipc::DiscoveredPeer,
    seen: Instant,
}

#[derive(Default)]
pub(crate) struct Table {
    entries: BTreeMap<NodeId, Entry>,
}

impl Table {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, entry| now.saturating_duration_since(entry.seen) < TTL);
    }

    pub fn insert(&mut self, own: NodeId, received: &Received, now: Instant) -> Option<NodeId> {
        self.expire(now);
        let id = parse_id(&received.beacon.node_id)?;
        if id == own || (!self.entries.contains_key(&id) && self.entries.len() >= CAP) {
            return None;
        }
        self.entries.insert(
            id,
            Entry {
                peer: protocol::ipc::DiscoveredPeer {
                    node_id: crate::command::node_id_hex(&id),
                    display_name: received.beacon.display_name.clone(),
                    address: received.address.to_string(),
                    system_name: received.beacon.system_name.clone(),
                    pairing_open: received.beacon.pairing_open,
                    protocol_version: received.beacon.protocol_version,
                },
                seen: now,
            },
        );
        Some(id)
    }

    pub fn peers(&self) -> Vec<protocol::ipc::DiscoveredPeer> {
        self.entries
            .values()
            .map(|entry| entry.peer.clone())
            .collect()
    }
}

/// One reused UDP socket, joined and transmitted only on explicitly eligible LAN adapters.
/// Receive-side subnet filtering also rejects traffic routed from an excluded adapter.
pub(crate) fn spawn(
    interfaces: Vec<(Ipv4Addr, Ipv4Addr)>,
    initial: Advertisement,
) -> std::io::Result<(
    watch::Sender<Advertisement>,
    mpsc::Receiver<Received>,
    tokio::task::JoinHandle<()>,
)> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    for (address, _) in &interfaces {
        socket.join_multicast_v4(&GROUP, address)?;
    }
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket.into())?;
    let (updates, mut state) = watch::channel(initial);
    let (tx, events) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let mut packet = [0u8; MAX_PACKET + 1];
        let mut interval = tokio::time::interval(PERIOD);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_advertised = None;
        loop {
            let changed = tokio::select! {
                result = socket.recv_from(&mut packet) => {
                    match result {
                        Ok((len, SocketAddr::V4(source))) if state.borrow().enabled => {
                            let allowed = interfaces.iter().any(|(ip, mask)|
                                u32::from(*ip) & u32::from(*mask) == u32::from(*source.ip()) & u32::from(*mask));
                            if allowed {
                                if let Some(beacon) = Beacon::decode(&packet[..len]) {
                                    let address = SocketAddr::new((*source.ip()).into(), beacon.port);
                                    let _ = tx.try_send(Received { beacon, address });
                                }
                            }
                        }
                        Ok(_) => {}
                        // Windows reports WSAEMSGSIZE rather than returning a truncated UDP packet.
                        Err(error) if error.raw_os_error() == Some(10040) => {}
                        Err(error) => {
                            tracing::warn!(%error, "LAN discovery receive stopped");
                            break;
                        }
                    }
                    false
                }
                result = state.changed() => {
                    if result.is_err() { break; }
                    true
                }
                _ = interval.tick() => true,
            };
            if !changed {
                continue;
            }
            let current = state.borrow().clone();
            if !current.enabled || !current.advertise {
                continue;
            }
            // Window changes are prompt, but repeated GUI/status work cannot flood the LAN.
            if last_advertised.is_some_and(|at: Instant| at.elapsed() < Duration::from_secs(1)) {
                continue;
            }
            let Some(bytes) = current.beacon.encode() else {
                continue;
            };
            for (interface, _) in &interfaces {
                let reference = socket2::SockRef::from(&socket);
                if reference.set_multicast_if_v4(interface).is_ok() {
                    if let Err(error) = socket.send_to(&bytes, SocketAddrV4::new(GROUP, PORT)).await
                    {
                        tracing::debug!(%error, %interface, "LAN discovery send failed");
                    }
                }
            }
            last_advertised = Some(Instant::now());
        }
    });
    Ok((updates, events, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon(id: u8) -> Beacon {
        Beacon {
            format: 1,
            node_id: crate::command::node_id_hex(&NodeId::new([id; 32])),
            display_name: "PC".into(),
            port: 47901,
            protocol_version: protocol::PROTOCOL_MAJOR,
            pairing_open: true,
            system_name: "Windows".into(),
        }
    }

    #[test]
    fn beacon_roundtrip_rejects_malformed_and_oversized() {
        let original = beacon(2);
        assert_eq!(Beacon::decode(&original.encode().unwrap()), Some(original));
        assert!(Beacon::decode(b"not json").is_none());
        assert!(Beacon::decode(&vec![b' '; MAX_PACKET + 1]).is_none());
        let mut invalid = beacon(2);
        invalid.protocol_version = protocol::PROTOCOL_MAJOR + 1;
        assert!(Beacon::decode(&invalid.encode().unwrap()).is_none());
        invalid = beacon(2);
        invalid.port = 0;
        assert!(Beacon::decode(&invalid.encode().unwrap()).is_none());
        invalid.port = 1;
        invalid.node_id = "fake".into();
        assert!(Beacon::decode(&invalid.encode().unwrap()).is_none());
        invalid = beacon(2);
        invalid.display_name = format!("\u{202e}\n{}", "я".repeat(80));
        let clean = Beacon::decode(&invalid.encode().unwrap()).unwrap();
        assert_eq!(clean.display_name.chars().count(), 32);
    }

    #[test]
    fn table_bounds_self_and_ttl() {
        let own = NodeId::new([0; 32]);
        let now = Instant::now();
        let mut table = Table::default();
        let received = |id| Received {
            beacon: beacon(id),
            address: "192.168.1.2:47901".parse().unwrap(),
        };
        assert!(table.insert(own, &received(0), now).is_none());
        for id in 1..=128 {
            assert!(table.insert(own, &received(id), now).is_some());
        }
        assert!(table.insert(own, &received(129), now).is_none());
        assert_eq!(table.peers().len(), CAP);
        table.expire(now + TTL);
        assert!(table.peers().is_empty());
    }
}
