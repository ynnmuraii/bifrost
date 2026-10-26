//! Discovery has no authority: only a verified expected-peer TLS route can change last_address.
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use geometry::NodeId;
use tokio::sync::{mpsc, watch};

use super::Session;
use crate::discovery::{Advertisement, Beacon, Received, Table};

struct Runtime {
    state: watch::Sender<Advertisement>,
    events: mpsc::Receiver<Received>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Refresh {
    task: tokio::task::JoinHandle<()>,
    result: mpsc::Receiver<(
        NodeId,
        SocketAddr,
        Result<transport::manager::DiscoveryRoute, transport::manager::DiscoveryRouteError>,
    )>,
}

impl Drop for Refresh {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
pub(super) struct Discovery {
    pub enabled: bool,
    advertise: bool,
    network: Option<config::NetworkConfig>,
    adapters: Vec<config::first_run::Candidate>,
    next_adapter_scan: Option<Instant>,
    next_service: Option<Instant>,
    runtime: Option<Runtime>,
    pub table: Table,
    refresh: Option<Refresh>,
    // Only trusted ids enter this map, so a forged stream cannot allocate arbitrary state.
    attempts: BTreeMap<NodeId, Instant>,
    last_warning: Option<Instant>,
}

impl Session {
    pub fn set_discovery_source(
        &mut self,
        settings: config::DiscoveryConfig,
        network: config::NetworkConfig,
    ) {
        self.discovery.enabled = settings.enabled;
        self.discovery.advertise = settings.advertise;
        self.discovery.adapters = config::discoverable_interfaces(&network);
        self.discovery.network = Some(network);
        self.start_discovery();
    }

    fn discovery_advertisement(&self) -> Advertisement {
        Advertisement {
            enabled: self.discovery.enabled,
            advertise: self.discovery.advertise,
            beacon: Beacon {
                format: 1,
                node_id: crate::command::node_id_hex(&self.node_id),
                display_name: protocol::sanitize_system_name(&self.display_name),
                port: self.listen.port(),
                protocol_version: protocol::PROTOCOL_MAJOR,
                pairing_open: self.pairing.window().is_some(),
                system_name: protocol::sanitize_system_name(&self.local_hello.system_name),
                supports_pairing_requests: true,
            },
        }
    }

    fn start_discovery(&mut self) {
        self.discovery.runtime = None;
        if !self.discovery.enabled {
            return;
        }
        let interfaces: Vec<_> = self
            .discovery
            .adapters
            .iter()
            .map(|interface| {
                let mask = if interface.prefix_len == 0 {
                    0
                } else {
                    u32::MAX << (32 - interface.prefix_len)
                };
                (interface.address, Ipv4Addr::from(mask))
            })
            .collect();
        if interfaces.is_empty() {
            tracing::warn!("LAN discovery: no eligible configured IPv4 adapter");
            return;
        }
        match crate::discovery::spawn(interfaces, self.discovery_advertisement()) {
            Ok((state, events, task)) => {
                self.discovery.runtime = Some(Runtime {
                    state,
                    events,
                    task,
                });
                tracing::info!(port = crate::discovery::PORT, "LAN discovery started");
            }
            Err(error) => {
                tracing::warn!(%error, "LAN discovery unavailable; manual pairing remains available")
            }
        }
    }

    /// Follow DHCP on the already selected adapter, never choose a different LAN or VPN.
    async fn refresh_discovery_adapters(&mut self) {
        let Some(network) = self.discovery.network.clone() else {
            return;
        };
        let available = config::first_run::lan_interfaces();
        let adapters: Vec<_> = if self.listen.ip().is_unspecified() {
            config::discoverable_interfaces(&network)
        } else {
            follow_selected_adapters(&self.discovery.adapters, &available)
        };
        if adapters.is_empty() || adapters == self.discovery.adapters {
            return;
        }
        if !self.listen.ip().is_unspecified() {
            let Some(selected) = adapters.first() else {
                return;
            };
            let address = SocketAddr::new(selected.address.into(), self.listen.port());
            if address != self.listen {
                match self.manager.rebind_listener(address).await {
                    Ok(bound) => self.listen = bound,
                    Err(error) => {
                        tracing::warn!(%error, "LAN discovery cannot rebind selected DHCP adapter");
                        return;
                    }
                }
                let value = selected.address.to_string();
                let path = self.paths.config_file();
                let persisted = value.clone();
                let saved = tokio::task::spawn_blocking(move || {
                    config::lock::update_locked(&path, |config| {
                        config.network.listen_address = persisted
                    })
                })
                .await;
                if !matches!(saved, Ok(Ok(()))) {
                    tracing::warn!("cannot persist selected adapter DHCP address");
                }
                if let Some(network) = self.discovery.network.as_mut() {
                    network.listen_address = value;
                }
            }
        }
        self.discovery.adapters = adapters;
        self.start_discovery();
    }

    pub(super) async fn set_discovery(&mut self, enabled: bool) -> protocol::ipc::OpResult {
        let path = self.paths.config_file();
        let saved = tokio::task::spawn_blocking(move || {
            config::lock::update_locked(&path, |config| config.discovery.enabled = enabled)
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            return protocol::ipc::OpResult {
                ok: false,
                error_code: "DISCOVERY_WRITE".into(),
            };
        }
        self.discovery.enabled = enabled;
        self.discovery.table.clear();
        self.discovery.refresh = None;
        self.start_discovery();
        protocol::ipc::OpResult {
            ok: true,
            error_code: String::new(),
        }
    }

    pub(super) async fn service_discovery(&mut self) {
        let now = Instant::now();
        if self.discovery.next_service.is_some_and(|due| now < due) {
            return;
        }
        self.discovery.next_service = Some(now + Duration::from_millis(50));
        self.discovery.table.expire(now);
        if !self.discovery.enabled {
            return;
        }
        if self
            .discovery
            .next_adapter_scan
            .is_none_or(|due| now >= due)
        {
            self.discovery.next_adapter_scan = Some(now + Duration::from_secs(5));
            self.refresh_discovery_adapters().await;
        }
        let pairing_open = self.pairing.window().is_some();
        if let Some(runtime) = self.discovery.runtime.as_ref() {
            runtime.state.send_if_modified(|current| {
                if current.beacon.pairing_open == pairing_open {
                    false
                } else {
                    current.beacon.pairing_open = pairing_open;
                    true
                }
            });
        }
        let result = self
            .discovery
            .refresh
            .as_mut()
            .and_then(|refresh| refresh.result.try_recv().ok());
        if let Some((peer, address, result)) = result {
            self.discovery.refresh = None;
            match result {
                Ok(
                    transport::manager::DiscoveryRoute::Verified
                    | transport::manager::DiscoveryRoute::Unchanged,
                ) if self.trust.is_trusted(&peer) => {
                    let mut updated = self.trust.clone();
                    updated.set_last_address(&peer, address.to_string());
                    match updated.save(&self.paths.trust_file()) {
                        Ok(_) => {
                            self.trust = updated;
                            tracing::info!(%peer, %address, "LAN discovery authenticated new peer route");
                        }
                        Err(error) => {
                            tracing::warn!(%error, "cannot persist authenticated discovery route")
                        }
                    }
                }
                Err(error) if warning_due(self.discovery.last_warning, now) => {
                    tracing::warn!(%peer, %address, %error, "LAN discovery route rejected; trust and last_address unchanged");
                    self.discovery.last_warning = Some(now);
                }
                _ => {}
            }
        }
        self.discovery.attempts.retain(|peer, at| {
            self.trust.is_trusted(peer)
                && now.saturating_duration_since(*at) < Duration::from_secs(30)
        });
        for _ in 0..32 {
            let received = self
                .discovery
                .runtime
                .as_mut()
                .and_then(|runtime| runtime.events.try_recv().ok());
            let Some(received) = received else {
                break;
            };
            let Some(peer) = self.discovery.table.insert(self.node_id, &received, now) else {
                continue;
            };
            let Some(record) = self.trust.get(&peer).filter(|record| record.is_active()) else {
                continue;
            };
            if self.discovery.refresh.is_some()
                || record
                    .last_address
                    .as_deref()
                    .and_then(|address| address.parse::<SocketAddr>().ok())
                    == Some(received.address)
                || self.discovery.attempts.contains_key(&peer)
                || self.manager.members().await.contains(&peer)
            {
                continue;
            }
            self.discovery.attempts.insert(peer, now);
            let manager = std::sync::Arc::clone(&self.manager);
            let address = received.address;
            let (tx, result) = mpsc::channel(1);
            let task = tokio::spawn(async move {
                let outcome = manager.refresh_discovered_route(peer, address).await;
                let _ = tx.send((peer, address, outcome)).await;
            });
            self.discovery.refresh = Some(Refresh { task, result });
        }
    }
}

fn warning_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(30))
}

fn follow_selected_adapters(
    selected: &[config::Candidate],
    available: &[config::Candidate],
) -> Vec<config::Candidate> {
    selected
        .iter()
        .filter_map(|old| {
            available
                .iter()
                .find(|candidate| {
                    candidate.index == old.index
                        && candidate.name == old.name
                        && candidate.address == old.address
                })
                .or_else(|| {
                    available.iter().find(|candidate| {
                        candidate.index == old.index && candidate.name == old.name
                    })
                })
                .cloned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_route_warns_once_per_thirty_seconds() {
        let now = Instant::now();
        assert!(warning_due(None, now));
        assert!(!warning_due(Some(now), now));
        assert!(!warning_due(Some(now), now + Duration::from_secs(29)));
        assert!(warning_due(Some(now), now + Duration::from_secs(30)));
    }

    #[test]
    fn dhcp_follows_only_the_selected_adapter_and_keeps_an_existing_address() {
        let candidate = |index, name: &str, address: &str| config::Candidate {
            index,
            name: name.into(),
            address: address.parse().unwrap(),
            prefix_len: 24,
            is_up: true,
            has_default_route: true,
        };
        let old = candidate(2, "Ethernet", "192.168.1.5");
        let new = candidate(2, "Ethernet", "192.168.1.6");
        let vpn = candidate(3, "WireGuard", "10.0.0.2");
        assert_eq!(
            follow_selected_adapters(std::slice::from_ref(&old), &[vpn.clone(), new.clone()]),
            vec![new.clone()]
        );
        assert_eq!(
            follow_selected_adapters(std::slice::from_ref(&old), &[new, old.clone()]),
            vec![old.clone()]
        );
        assert!(follow_selected_adapters(&[old], &[vpn]).is_empty());
    }
}
