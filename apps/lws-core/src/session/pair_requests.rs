//! Prompt-only requests never write trust; acceptance enters the existing SAS ceremony.
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use geometry::NodeId;
use protocol::{ipc, wire};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, Semaphore};

use super::Session;

const WAIT: Duration = Duration::from_secs(60);
const READ: Duration = Duration::from_secs(5);
const RATE_MS: u64 = 30_000;
const MUTE_MS: u64 = 600_000;
const MAX_SOURCES: usize = 256;

#[derive(Default)]
struct Policy {
    nodes: BTreeMap<NodeId, u64>,
    ips: BTreeMap<IpAddr, u64>,
    muted: BTreeMap<NodeId, u64>,
}

impl Policy {
    fn admit(
        &mut self,
        peer: NodeId,
        ip: IpAddr,
        now: u64,
        enabled: bool,
        trusted: bool,
        busy: bool,
    ) -> &'static str {
        self.nodes.retain(|_, until| *until > now);
        self.ips.retain(|_, until| *until > now);
        self.muted.retain(|_, until| *until > now);
        if !enabled {
            return "disabled";
        }
        if trusted || self.muted.contains_key(&peer) {
            return "declined";
        }
        if busy
            || self.nodes.contains_key(&peer)
            || self.ips.contains_key(&ip)
            || self.nodes.len() >= MAX_SOURCES
            || self.ips.len() >= MAX_SOURCES
            || self.muted.len() >= MAX_SOURCES
        {
            return "busy";
        }
        self.nodes.insert(peer, now.saturating_add(RATE_MS));
        self.ips.insert(ip, now.saturating_add(RATE_MS));
        "pending"
    }

    fn mute(&mut self, peer: NodeId, now: u64) -> bool {
        self.muted.retain(|_, until| *until > now);
        if self.muted.len() >= MAX_SOURCES && !self.muted.contains_key(&peer) {
            return false;
        }
        self.muted.insert(peer, now.saturating_add(MUTE_MS));
        true
    }
}

struct Incoming {
    prompt: ipc::IncomingPairingRequest,
    deadline: u64,
    decisions: mpsc::Sender<&'static str>,
}

impl Incoming {
    fn expired(&self, now: u64) -> bool {
        self.decisions.is_closed() || now >= self.deadline
    }
}

enum Event {
    Incoming {
        peer: NodeId,
        address: SocketAddr,
        request: wire::PairRequest,
        decisions: mpsc::Sender<&'static str>,
    },
    Outgoing {
        generation: u64,
        state: String,
        address: String,
    },
}

pub(super) struct Requests {
    pub enabled: bool,
    policy: Policy,
    incoming: Option<Incoming>,
    outgoing: Option<ipc::OutgoingPairingRequest>,
    outgoing_task: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    slots: Arc<Semaphore>,
}

impl Default for Requests {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            enabled: true,
            policy: Policy::default(),
            incoming: None,
            outgoing: None,
            outgoing_task: None,
            generation: 0,
            tx,
            rx,
            slots: Arc::new(Semaphore::new(8)),
        }
    }
}

impl Drop for Requests {
    fn drop(&mut self) {
        if let Some(task) = self.outgoing_task.take() {
            task.abort();
        }
    }
}

impl Session {
    pub(super) fn mute_pair_requester(&mut self, node: &str) -> ipc::OpResult {
        let Some(peer) = parse_node(node) else {
            return result(false, "BAD_NODE_ID");
        };
        let now = self.now_ms();
        let policy = &mut self.pair_requests.policy;
        policy.muted.retain(|_, until| *until > now);
        // Reserve the last mute slot for declining the prompt already on screen.
        let reserved =
            self.pair_requests.incoming.as_ref().is_some_and(|request| {
                request.prompt.requester_node_id.as_slice() != peer.as_bytes()
            });
        if (reserved && policy.muted.len() >= MAX_SOURCES - 1 && !policy.muted.contains_key(&peer))
            || !policy.mute(peer, now)
        {
            return result(false, "PAIRING_REQUEST_BUSY");
        }
        if self
            .pair_requests
            .incoming
            .as_ref()
            .is_some_and(|request| request.prompt.requester_node_id.as_slice() == peer.as_bytes())
        {
            self.clear_pair_request("declined");
        }
        result(true, "")
    }

    pub fn set_pairing_request_source(&mut self, settings: config::PairingConfig) {
        self.pair_requests.enabled = settings.accept_requests;
    }

    pub(super) fn request_prompt(&self) -> Option<ipc::IncomingPairingRequest> {
        self.pair_requests.incoming.as_ref().map(|incoming| {
            let mut prompt = incoming.prompt.clone();
            prompt.seconds_remaining = incoming
                .deadline
                .saturating_sub(self.now_ms())
                .div_ceil(1000) as u32;
            prompt
        })
    }

    pub(super) fn outgoing_request(&self) -> Option<ipc::OutgoingPairingRequest> {
        self.pair_requests.outgoing.clone()
    }

    pub(super) fn receive_pair_request(
        &mut self,
        connection: transport::pairing::RequestConnection,
    ) {
        let Ok(permit) = Arc::clone(&self.pair_requests.slots).try_acquire_owned() else {
            return;
        };
        let tx = self.pair_requests.tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut stream = connection.stream;
            let operation = async {
                let envelope = tokio::time::timeout(READ, read_frame(&mut stream))
                    .await
                    .ok()?
                    .ok()?;
                let Some(wire::pairing_envelope::Payload::Request(request)) = envelope.payload
                else {
                    return None;
                };
                if envelope.major != protocol::PROTOCOL_MAJOR
                    || !envelope.attempt_id.is_empty()
                    || request.requester_node_id.as_slice() != connection.peer.as_bytes()
                    || request.display_name.chars().count() > 32
                    || request.display_name.chars().any(char::is_control)
                    || request.system_name.chars().count() > 32
                    || request.system_name.chars().any(char::is_control)
                    || request.app_version.len() > 32
                {
                    return None;
                }
                let (decisions, mut receive) = mpsc::channel(2);
                tx.send(Event::Incoming {
                    peer: connection.peer,
                    address: connection.address,
                    request,
                    decisions,
                })
                .await
                .ok()?;
                let mut byte = [0u8; 1];
                loop {
                    let status = tokio::select! {
                        status = receive.recv() => status?,
                        _ = stream.read(&mut byte) => return None,
                    };
                    write_ack(&mut stream, status).await.ok()?;
                    if status != "pending" {
                        break;
                    }
                }
                Some(())
            };
            if tokio::time::timeout(WAIT, operation).await.is_err() {
                let _ = tokio::time::timeout(READ, write_ack(&mut stream, "timeout")).await;
            }
        });
    }

    pub(super) fn service_pair_requests(&mut self) {
        while let Ok(event) = self.pair_requests.rx.try_recv() {
            match event {
                Event::Incoming {
                    peer,
                    address,
                    request,
                    decisions,
                } => {
                    let now = self.now_ms();
                    let status = self.pair_requests.policy.admit(
                        peer,
                        address.ip(),
                        now,
                        self.pair_requests.enabled,
                        self.trust.is_trusted(&peer),
                        self.pair_requests.incoming.is_some() || self.pairing.window().is_some(),
                    );
                    let _ = decisions.try_send(status);
                    if status == "pending" && !decisions.is_closed() {
                        let prompt = ipc::IncomingPairingRequest {
                            requester_node_id: peer.as_bytes().to_vec(),
                            display_name: protocol::sanitize_system_name(&request.display_name),
                            system_name: protocol::sanitize_system_name(&request.system_name),
                            fingerprint: identity::fingerprint_grouped(&peer),
                            seconds_remaining: 60,
                        };
                        tracing::info!(requester = %crate::command::node_id_hex(&peer), "pairing request: prompt");
                        self.publish(ipc::gui_frame::Msg::IncomingPairingRequest(prompt.clone()));
                        self.pair_requests.incoming = Some(Incoming {
                            prompt,
                            deadline: now.saturating_add(60_000),
                            decisions,
                        });
                    }
                }
                Event::Outgoing {
                    generation,
                    state,
                    address,
                } => {
                    if generation != self.pair_requests.generation {
                        continue;
                    }
                    if let Some(outgoing) = &mut self.pair_requests.outgoing {
                        outgoing.state = state.clone();
                        let event = outgoing.clone();
                        self.publish(ipc::gui_frame::Msg::OutgoingPairingRequest(event));
                    }
                    if state == "accepted" {
                        tracing::info!("pairing request: accepted; starting SAS");
                    }
                    if matches!(state.as_str(), "accepted" | "legacy") {
                        self.spawn_pair_dial(address);
                    }
                    if state != "waiting" {
                        self.pair_requests.outgoing_task = None;
                    }
                }
            }
        }
        if self
            .pair_requests
            .incoming
            .as_ref()
            .is_some_and(|incoming| incoming.expired(self.now_ms()))
        {
            self.clear_pair_request("timeout");
        }
    }

    fn clear_pair_request(&mut self, status: &'static str) {
        if let Some(incoming) = self.pair_requests.incoming.take() {
            let _ = incoming.decisions.try_send(status);
            tracing::info!(status, "pairing request: cleared");
            self.publish(ipc::gui_frame::Msg::IncomingPairingRequest(
                ipc::IncomingPairingRequest::default(),
            ));
        }
    }

    pub(super) fn answer_pair_request(
        &mut self,
        answer: ipc::AnswerPairingRequest,
    ) -> ipc::OpResult {
        let Some(incoming) = &self.pair_requests.incoming else {
            return result(false, "NO_PAIRING_REQUEST");
        };
        if incoming.prompt.requester_node_id != answer.requester_node_id
            || incoming.decisions.is_closed()
            || self.now_ms() >= incoming.deadline
        {
            return result(false, "NO_PAIRING_REQUEST");
        }
        let Ok(bytes) = <[u8; 32]>::try_from(answer.requester_node_id.as_slice()) else {
            return result(false, "BAD_NODE_ID");
        };
        let peer = NodeId::new(bytes);
        if answer.accept {
            match self.open_pairing_window_for(120, Some(peer)) {
                Ok(_) => self.clear_pair_request("accepted"),
                Err(_) => {
                    self.clear_pair_request("busy");
                    return result(false, "PAIRING_WINDOW_OPEN");
                }
            }
        } else {
            let now = self.now_ms();
            if !self.pair_requests.policy.mute(peer, now) {
                return result(false, "PAIRING_REQUEST_BUSY");
            }
            self.clear_pair_request("declined");
        }
        result(true, "")
    }

    pub(super) fn cancel_pair_request(&mut self) {
        if let Some(task) = self.pair_requests.outgoing_task.take() {
            task.abort();
        }
        self.pair_requests.generation = self.pair_requests.generation.wrapping_add(1);
        if let Some(outgoing) = &mut self.pair_requests.outgoing {
            outgoing.state = "cancelled".into();
            let event = outgoing.clone();
            self.publish(ipc::gui_frame::Msg::OutgoingPairingRequest(event));
        }
    }

    pub(super) fn start_pair_request(&mut self, request: ipc::RequestPairing) -> ipc::OpResult {
        if self.pair_requests.outgoing_task.is_some() {
            return result(false, "PAIRING_REQUEST_BUSY");
        }
        let Ok(address) = request.address.parse::<SocketAddr>() else {
            return result(false, "BAD_ADDRESS");
        };
        let Some(peer) = parse_node(&request.node_id) else {
            return result(false, "BAD_NODE_ID");
        };
        if self.trust.is_trusted(&peer) {
            return result(false, "ALREADY_TRUSTED");
        }
        let state = if request.supports_requests {
            "waiting"
        } else {
            "legacy"
        };
        let outgoing = ipc::OutgoingPairingRequest {
            node_id: request.node_id,
            display_name: protocol::sanitize_system_name(&request.display_name),
            state: state.into(),
        };
        self.publish(ipc::gui_frame::Msg::OutgoingPairingRequest(
            outgoing.clone(),
        ));
        self.pair_requests.outgoing = Some(outgoing);
        if !request.supports_requests {
            self.spawn_pair_dial(request.address);
            return result(true, "");
        }
        self.pair_requests.generation = self.pair_requests.generation.wrapping_add(1);
        let generation = self.pair_requests.generation;
        let tx = self.pair_requests.tx.clone();
        let identity = Arc::clone(&self.identity);
        let hello = wire::PairRequest {
            requester_node_id: self.node_id.as_bytes().to_vec(),
            display_name: protocol::sanitize_system_name(&self.display_name),
            system_name: protocol::sanitize_system_name(&self.local_hello.system_name),
            app_version: protocol::app_version().into(),
        };
        // The request socket is the cancellation token; dropping this task closes it.
        self.pair_requests.outgoing_task = Some(tokio::spawn(async move {
            let operation = async {
                let mut stream = transport::pairing::dial_request(&identity, address, peer).await?;
                write_frame(
                    &mut stream,
                    &wire::PairingEnvelope {
                        major: protocol::PROTOCOL_MAJOR,
                        minor: protocol::PROTOCOL_MINOR,
                        payload: Some(wire::pairing_envelope::Payload::Request(hello)),
                        ..Default::default()
                    },
                )
                .await?;
                tracing::info!("pairing request: sent; waiting for decision");
                read_request_decision(&mut stream).await
            };
            let state = match tokio::time::timeout(WAIT, operation).await {
                Ok(Ok(state)) => state,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::Unsupported => {
                    "legacy".into()
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "pairing request failed");
                    "error".into()
                }
                Err(_) => "timeout".into(),
            };
            let _ = tx
                .send(Event::Outgoing {
                    generation,
                    state,
                    address: address.to_string(),
                })
                .await;
        }));
        result(true, "")
    }

    pub(super) async fn set_pair_requests(&mut self, enabled: bool) -> ipc::OpResult {
        let path = self.paths.config_file();
        match tokio::task::spawn_blocking(move || {
            config::lock::update_locked(&path, |config| config.pairing.accept_requests = enabled)
        })
        .await
        {
            Ok(Ok(())) => {
                self.pair_requests.enabled = enabled;
                if !enabled {
                    self.clear_pair_request("disabled");
                }
                result(true, "")
            }
            _ => result(false, "CONFIG_SAVE_FAILED"),
        }
    }
}

async fn read_request_decision(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<String> {
    loop {
        let frame = read_frame(stream).await?;
        if frame.major != protocol::PROTOCOL_MAJOR || !frame.attempt_id.is_empty() {
            return Err(std::io::Error::other(
                "invalid pairing request response header",
            ));
        }
        let Some(wire::pairing_envelope::Payload::RequestAck(ack)) = frame.payload else {
            return Err(std::io::Error::other("unexpected pairing request response"));
        };
        match ack.status.as_str() {
            "pending" => continue,
            "accepted" | "declined" | "busy" | "disabled" | "timeout" => return Ok(ack.status),
            _ => return Err(std::io::Error::other("invalid pairing request decision")),
        }
    }
}

pub(super) fn parse_node(text: &str) -> Option<NodeId> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(NodeId::new(bytes))
}

fn result(ok: bool, code: &str) -> ipc::OpResult {
    ipc::OpResult {
        ok,
        error_code: code.into(),
    }
}

async fn read_frame(
    stream: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<wire::PairingEnvelope> {
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > protocol::MAX_FRAME_PAIRING {
        return Err(std::io::Error::other("invalid pairing request frame size"));
    }
    let mut bytes = vec![0; length + 4];
    bytes[..4].copy_from_slice(&(length as u32).to_be_bytes());
    stream.read_exact(&mut bytes[4..]).await?;
    protocol::decode_message(protocol::Channel::Pairing, &bytes)
        .map_err(|_| std::io::Error::other("malformed pairing request"))?
        .ok_or_else(|| std::io::Error::other("incomplete pairing request"))
}

async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    frame: &wire::PairingEnvelope,
) -> std::io::Result<()> {
    let bytes = protocol::encode_message(protocol::Channel::Pairing, frame)
        .map_err(|_| std::io::Error::other("pairing request frame too large"))?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

async fn write_ack(stream: &mut (impl AsyncWrite + Unpin), status: &str) -> std::io::Result<()> {
    write_frame(
        stream,
        &wire::PairingEnvelope {
            major: protocol::PROTOCOL_MAJOR,
            minor: protocol::PROTOCOL_MINOR,
            payload: Some(wire::pairing_envelope::Payload::RequestAck(
                wire::PairRequestAck {
                    status: status.into(),
                },
            )),
            ..Default::default()
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn requester_wait_is_bounded_even_while_target_keeps_socket_open() {
        let (mut requester, mut target) = tokio::io::duplex(1024);
        write_ack(&mut target, "pending").await.unwrap();
        let decision = tokio::time::timeout(WAIT, read_request_decision(&mut requester));
        tokio::time::advance(WAIT).await;
        assert!(decision.await.is_err());
    }

    #[tokio::test]
    async fn pending_is_not_acceptance_and_every_final_decision_is_preserved() {
        for status in ["accepted", "declined", "busy", "disabled", "timeout"] {
            let (mut requester, mut target) = tokio::io::duplex(1024);
            write_ack(&mut target, "pending").await.unwrap();
            write_ack(&mut target, status).await.unwrap();
            assert_eq!(read_request_decision(&mut requester).await.unwrap(), status);
        }
        let (mut requester, mut target) = tokio::io::duplex(1024);
        write_ack(&mut target, "unexpected").await.unwrap();
        assert!(read_request_decision(&mut requester).await.is_err());
    }

    #[test]
    fn prompt_timeout_and_requester_disconnect_use_the_same_expiry_path() {
        let (decisions, receive) = mpsc::channel(2);
        let incoming = Incoming {
            prompt: Default::default(),
            deadline: 60_000,
            decisions,
        };
        assert!(!incoming.expired(59_999));
        assert!(incoming.expired(60_000));
        drop(receive);
        assert!(
            incoming.expired(1),
            "Cancel closes the request stream and decision receiver"
        );
    }

    #[test]
    fn hostile_distinct_identities_cannot_grow_policy_maps_unbounded() {
        let mut policy = Policy::default();
        for index in 0..300u32 {
            let mut bytes = [0; 32];
            bytes[..4].copy_from_slice(&index.to_be_bytes());
            let peer = NodeId::new(bytes);
            let ip = IpAddr::V4(std::net::Ipv4Addr::from(index));
            let status = policy.admit(peer, ip, 0, true, false, false);
            assert_eq!(
                status,
                if index < MAX_SOURCES as u32 {
                    "pending"
                } else {
                    "busy"
                }
            );
            policy.mute(peer, 0);
        }
        assert_eq!(policy.nodes.len(), MAX_SOURCES);
        assert_eq!(policy.ips.len(), MAX_SOURCES);
        assert_eq!(policy.muted.len(), MAX_SOURCES);
        assert!(
            policy.muted.contains_key(&NodeId::new([0; 32])),
            "flooding never evicts an unexpired decline"
        );
        assert!(!policy.mute(NodeId::new([255; 32]), 1));
    }

    #[test]
    fn policy_bounds_busy_rate_mute_disabled_and_trusted() {
        let a = NodeId::new([1; 32]);
        let b = NodeId::new([2; 32]);
        let ip = "192.168.1.2".parse().unwrap();
        let other = "192.168.1.3".parse().unwrap();
        let mut policy = Policy::default();
        assert_eq!(policy.admit(a, ip, 0, false, false, false), "disabled");
        assert_eq!(policy.admit(a, ip, 0, true, true, false), "declined");
        assert_eq!(policy.admit(a, ip, 0, true, false, true), "busy");
        assert_eq!(policy.admit(a, ip, 0, true, false, false), "pending");
        assert_eq!(policy.admit(a, other, 1, true, false, false), "busy");
        assert_eq!(policy.admit(b, ip, 1, true, false, false), "busy");
        assert_eq!(policy.admit(a, ip, RATE_MS, true, false, false), "pending");
        policy.muted.insert(a, MUTE_MS);
        assert_eq!(
            policy.admit(a, ip, RATE_MS * 2, true, false, false),
            "declined"
        );
        assert_eq!(policy.admit(a, ip, MUTE_MS, true, false, false), "pending");
    }
    #[tokio::test]
    async fn bounded_reader_refuses_before_body_allocation() {
        let mut prefix = &0xffff_ffffu32.to_be_bytes()[..];
        assert!(read_frame(&mut prefix).await.is_err());
        let mut prefix = &0u32.to_be_bytes()[..];
        assert!(read_frame(&mut prefix).await.is_err());
    }
    #[test]
    fn node_ids_are_canonical_bytes_not_names() {
        assert_eq!(parse_node(&"01".repeat(32)), Some(NodeId::new([1; 32])));
        assert!(parse_node(&"я".repeat(32)).is_none());
    }
}
