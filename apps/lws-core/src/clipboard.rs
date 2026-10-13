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
//! 2. Every member is offered it on **its own** clipboard channel. A channel does not exist yet,
//!    so the peer first gets a `ClipboardGrant` over the input link — that is the only clipboard
//!    message the input link carries, because it has to precede the channel it authorises —
//!    then dials back on `47902` with `ClipboardBind` and receives `ClipboardOffer`, the
//!    `ClipboardData` chunks and `ClipboardComplete`.
//! 3. A received payload is verified (chunk count, size, SHA-256, UTF-8) and only then handed to
//!    the agent as `SetClipboardText`. A payload that came from a peer is never offered onward
//!    ([`cb::Source::apply_from_peer`]), which is what keeps the fan-out from looping.
//!
//! # Why the transfer is its own task
//!
//! `KVM-042`: «объём clipboard не блокирует key-up». A 1 MiB payload at 256 KiB/s takes four
//! seconds if it is paced honestly, and the session loop drives the input link — so the paced
//! sender runs on a task of its own and the session only ever hands it a payload. Everything
//! this module does *inside* the session loop is bookkeeping.

use std::collections::BTreeMap;
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

/// Everything `[clipboard]` decides.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// `clipboard.enabled`: the capability bit and the whole channel hang off it.
    pub enabled: bool,
    /// The pure policy: `skip_secret_marked` and `max_bytes`.
    pub policy: cb::Policy,
    /// `clipboard.rate_bytes_per_s`, handed to the paced sender.
    pub rate_bytes_per_s: u64,
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
            },
            rate_bytes_per_s: clipboard.rate_bytes_per_s,
            eager: matches!(clipboard.transfer, config::ClipboardTransfer::OnOffer),
        }
    }
}

/// What the coordinator wants the session to do, in order.
#[derive(Debug)]
pub enum Out {
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
    /// Hand this frame to the agent.
    Agent(ipc::agent_frame::Msg),
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
    /// A peer's clipboard channel is accepted, bound and validated; its messages follow.
    Bound {
        /// Which peer.
        peer: NodeId,
        /// The channel the bind named.
        channel_id: [u8; 16],
    },
    /// One message on a bound channel.
    Message {
        /// Which peer.
        peer: NodeId,
        /// What it sent.
        message: ClipboardMessage,
    },
    /// That peer's channel is gone (reset, closed, or refused).
    Down {
        /// Which peer.
        peer: NodeId,
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
    /// The offer we are waiting to hand to a peer once it binds.
    waiting: BTreeMap<NodeId, (ClipboardMessage, Vec<u8>)>,
    /// What we have offered a peer and it has not asked for yet, keyed by peer.
    ///
    /// `docs/05-protocol.md` fixes Offer → Request → Data, so the payload waits for the
    /// `ClipboardRequest` and a peer that never asks is never flooded.
    offered: BTreeMap<NodeId, (ClipboardMessage, Vec<u8>)>,
    /// The offer a peer made and we held back (`transfer = "on_focus_enter"`).
    pending_request: Option<(NodeId, [u8; 16])>,
    /// One transfer per offer we asked for, keyed by `offer_id`.
    incoming: BTreeMap<[u8; 16], Incoming>,
    /// The agent's chunks for the copy it is currently reporting.
    local: Option<LocalAssembly>,
    /// The payload of the local clipboard, kept so a member that joins later still gets it.
    payload: Vec<u8>,
    /// The outbound queue capacity a channel runtime is given.
    queue_capacity: usize,
    /// `network.port_clipboard`: the port this node's own listener is bound to, which every
    /// grant it issues advertises.
    clipboard_port: u16,
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
    /// The IP the peer's input link comes from — where its clipboard listener is reachable.
    remote_ip: Option<std::net::IpAddr>,
}

/// A payload we asked a peer for.
struct Incoming {
    offer: cb::Offer,
    assembler: cb::Assembler,
}

/// The agent's `ClipboardChanged` chunks, assembled before anything is decided.
struct LocalAssembly {
    version: u64,
    count: u32,
    next: u32,
    size: u64,
    markers: Vec<cb::Marker>,
    payload: Vec<u8>,
}

/// What one `ClipboardChanged` chunk contributed: nothing yet, or the whole copy.
type AssembledCopy = Result<Option<(Vec<u8>, Vec<cb::Marker>)>, cb::ClipboardError>;

impl Clipboard {
    /// Builds the endpoint and its runtime: the listener on `47902` and the dial list.
    pub async fn start(
        node: NodeId,
        settings: Settings,
        identity: Arc<identity::NodeIdentity>,
        trust: identity::TrustStore,
        listen: SocketAddr,
        queue_capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<Inbound>), String> {
        let (runtime, events) = Handle::start(identity, trust, listen, queue_capacity)
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
            local: None,
            payload: Vec::new(),
            queue_capacity,
            clipboard_port: listen.port(),
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
            .send(
                peer,
                ClipboardMessage::Cancel {
                    offer_id,
                    reason: wire_error(reason),
                },
            )
            .await;
    }

    /// `SetClipboard` from the GUI: transfer clipboard content, or stop transferring it.
    ///
    /// This is the same gate `[clipboard].enabled` seeds ([`cb::Policy::enabled`]), flipped at
    /// runtime: content that arrives while it is off is not offered, and a transfer already in
    /// flight finishes rather than losing half a payload (`KVM-042`). The listener and the
    /// bound channels stay up, so turning it back on needs no reconnect.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.settings.enabled = enabled;
        self.settings.policy.enabled = enabled;
    }

    /// Whether clipboard transfers are on, for `Status.clipboard_enabled`.
    pub const fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    /// One `ClipboardChanged` chunk from the agent.
    ///
    /// Nothing is decided — and nothing is offered — until the last chunk arrived with a
    /// matching hash and valid UTF-8.
    pub async fn on_agent_frame(&mut self, message: &ipc::ClipboardChanged) -> Vec<Out> {
        let (payload, markers) = match self.take_chunk(message) {
            Ok(Some(assembled)) => assembled,
            // Not the last chunk yet: nothing is decided, and nothing is offered.
            Ok(None) => return Vec::new(),
            Err(error) => {
                tracing::warn!(?error, "clipboard chunks from the agent refused");
                return Vec::new();
            }
        };
        self.on_local_copy(payload, markers).await
    }

    /// Assembles one chunk.
    ///
    /// `Ok(None)` means more chunks are expected; `Ok(Some(..))` is the last one, with the
    /// payload and the markers the platform reported for the whole copy.
    fn take_chunk(&mut self, message: &ipc::ClipboardChanged) -> AssembledCopy {
        let count = message.chunk_count;
        if count == 0 || count as usize > cb::MAX_CHUNKS || message.chunk_index >= count {
            self.local = None;
            return Err(cb::ClipboardError::Invalid);
        }
        let fresh = match &self.local {
            Some(assembly) => assembly.version != message.origin_version || assembly.count != count,
            None => true,
        };
        if fresh {
            self.local = Some(LocalAssembly {
                version: message.origin_version,
                count,
                next: 0,
                size: message.size_bytes,
                markers: markers_of(&message.markers),
                payload: Vec::with_capacity(message.size_bytes.min(cb::MAX_BYTES) as usize),
            });
        }
        let assembly = self.local.as_mut().expect("inserted above");
        // The assembly is bounded by what the agent *declared*, not by the policy's limit: a
        // copy over the limit has to arrive whole so that the policy can name it
        // (`CLIPBOARD_TOO_LARGE`), and the declaration itself is bounded by `chunks_of`.
        if message.chunk_index != assembly.next
            || message.data.len() > cb::DECODED_FRAME_BYTES
            || (assembly.payload.len() + message.data.len()) as u64 > assembly.size
        {
            self.local = None;
            return Err(cb::ClipboardError::Invalid);
        }
        assembly.payload.extend_from_slice(&message.data);
        assembly.next += 1;
        if assembly.next < count {
            return Ok(None);
        }
        let assembly = self.local.take().expect("checked above");
        if assembly.payload.len() as u64 != assembly.size
            || cb::sha256(&assembly.payload) != message.sha256
        {
            return Err(cb::ClipboardError::HashMismatch);
        }
        Ok(Some((assembly.payload, assembly.markers)))
    }

    /// A local copy: the only path that mints a version, and the only one that offers.
    async fn on_local_copy(&mut self, payload: Vec<u8>, markers: Vec<cb::Marker>) -> Vec<Out> {
        if !self.settings.enabled {
            // `CLIPBOARD_DISABLED`: a disabled node never offers anything.
            return Vec::new();
        }
        let content = cb::Content {
            bytes: payload.len() as u64,
            markers: markers.clone(),
        };
        let Ok(offer_id) = identity::random16() else {
            return Vec::new();
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
                    return Vec::new();
                }
            };
        tracing::info!(
            bytes = offer.size,
            version = offer.version,
            "clipboard offer"
        );
        self.payload = payload;
        // «Локальное копирование побеждает»: a fresh local copy cancels what an incoming
        // transfer was going to deliver, so a newer offer must supersede the one on the wire.
        self.offered.clear();
        self.fan_out(offer).await
    }

    /// Offers one copy to every member, opening a channel where there is none.
    async fn fan_out(&mut self, offer: cb::Offer) -> Vec<Out> {
        let mut out = Vec::new();
        for (peer, state) in self.peers.iter_mut() {
            let peer = *peer;
            if !state.text_clipboard {
                // ADR-008 makes the clipboard opt-in **on both sides**: a peer that did not
                // claim the capability is never offered anything, so the refusal code never has
                // to travel.
                tracing::info!(peer = %peer, code = "CLIPBOARD_DISABLED", "clipboard offer skipped");
                continue;
            }
            let message = ClipboardMessage::Offer {
                offer_id: offer.offer_id,
                origin_node_id: offer.origin,
                origin_version: offer.version,
                mime: cb::MIME.to_owned(),
                size_bytes: offer.size,
            };
            if state.channel.is_some() {
                let _ = self.runtime.send(peer, message).await;
                continue;
            }
            let Some(parent) = state.parent else {
                continue;
            };
            // The grant travels on the input link: the peer cannot dial a channel it has not
            // been told about yet.
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
            self.waiting.insert(peer, (message, self.payload.clone()));
        }
        out
    }

    /// A peer's channel is bound: deliver what was waiting for it, and start the transfer.
    pub async fn on_bound(&mut self, peer: NodeId, channel_id: [u8; 16]) -> Vec<Out> {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.channel = Some(channel_id);
        }
        if let Some((offer, payload)) = self.waiting.remove(&peer) {
            let _ = self.runtime.send(peer, offer.clone()).await;
            // The payload waits for the peer's `ClipboardRequest`: Offer → Request → Data is
            // the order `docs/05-protocol.md` fixes, and a receiver under the
            // `on_focus_enter` policy has not asked yet.
            self.offered.insert(peer, (offer, payload));
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

    /// One message from a peer on the clipboard channel.
    pub async fn on_peer_message(&mut self, peer: NodeId, message: ClipboardMessage) -> Vec<Out> {
        match message {
            ClipboardMessage::Offer {
                offer_id,
                origin_node_id,
                origin_version,
                mime,
                size_bytes,
            } => {
                self.on_offer(
                    peer,
                    offer_id,
                    &origin_node_id,
                    origin_version,
                    mime,
                    size_bytes,
                )
                .await
            }
            ClipboardMessage::Request { offer_id } => {
                let asked = matches!(
                    self.offered.get(&peer),
                    Some((offer, _)) if offer_id_of(offer) == Some(offer_id)
                );
                if asked {
                    if let Some((_, payload)) = self.offered.remove(&peer) {
                        let _ = self
                            .runtime
                            .send_transfer(peer, offer_id, payload, self.settings.rate_bytes_per_s)
                            .await;
                    }
                }
                Vec::new()
            }
            ClipboardMessage::Data {
                offer_id,
                chunk_index,
                chunk_count,
                data,
            } => {
                let outcome = match self.incoming.get_mut(&offer_id) {
                    Some(incoming) => {
                        incoming
                            .assembler
                            .push(&offer_id, chunk_index, chunk_count, &data)
                    }
                    None => return Vec::new(),
                };
                match outcome {
                    Ok(cb::AssemblyOutcome::NeedMore) => Vec::new(),
                    Ok(cb::AssemblyOutcome::Ready) => Vec::new(),
                    Err(error) => {
                        tracing::warn!(peer = %peer, ?error, "clipboard transfer cancelled");
                        self.incoming.remove(&offer_id);
                        vec![Out::Cancel {
                            peer,
                            offer_id,
                            reason: error.error(),
                        }]
                    }
                }
            }
            ClipboardMessage::Complete { offer_id, sha256 } => {
                let Some(incoming) = self.incoming.remove(&offer_id) else {
                    return Vec::new();
                };
                match incoming.assembler.finish(&sha256) {
                    Ok(text) => {
                        // The payload is now this node's, but it is *not* ours to offer: the
                        // fan-out stops here (ADR-008's echo rule).
                        self.source.apply_from_peer(&incoming.offer);
                        tracing::info!(
                            peer = %peer,
                            bytes = text.len(),
                            version = incoming.offer.version,
                            "clipboard payload applied"
                        );
                        self.deliver_to_agent(text, incoming.offer.version)
                    }
                    Err(error) => {
                        tracing::warn!(peer = %peer, ?error, "clipboard payload refused");
                        vec![Out::Cancel {
                            peer,
                            offer_id,
                            reason: error.error(),
                        }]
                    }
                }
            }
            ClipboardMessage::Cancel { offer_id, reason } => {
                tracing::info!(peer = %peer, reason = ?reason, "clipboard transfer cancelled by the peer");
                self.incoming.remove(&offer_id);
                Vec::new()
            }
            ClipboardMessage::Grant { .. } | ClipboardMessage::Bind { .. } => Vec::new(),
        }
    }

    /// An offer arrived: judge it against the policy and the echo set, then ask for it.
    async fn on_offer(
        &mut self,
        peer: NodeId,
        offer_id: [u8; 16],
        origin_node_id: &[u8],
        origin_version: u64,
        mime: String,
        size_bytes: u64,
    ) -> Vec<Out> {
        if !self.settings.enabled {
            // `CLIPBOARD_DISABLED`: the receiver is off, so nothing is asked for or applied.
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Disabled,
            }];
        }
        if mime != cb::MIME {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        }
        let Ok(origin) = <[u8; 32]>::try_from(origin_node_id) else {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        };
        let offer = cb::Offer {
            offer_id,
            origin,
            version: origin_version,
            mime,
            size: size_bytes,
        };
        match self.source.incoming(&offer, &self.settings.policy) {
            Ok(cb::source::Incoming::Request) => {}
            Ok(cb::source::Incoming::Ignore(decision)) => {
                // A duplicate or stale offer is dropped rather than answered.
                tracing::debug!(?decision, "clipboard offer ignored");
                return Vec::new();
            }
            Err(error) => {
                return vec![Out::Cancel {
                    peer,
                    offer_id,
                    reason: error,
                }]
            }
        }
        self.source.accept(&offer);
        let Ok(assembler) = cb::Assembler::new(offer_id, size_bytes) else {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::TooLarge,
            }];
        };
        self.incoming
            .insert(offer_id, Incoming { offer, assembler });
        if self.settings.eager {
            let _ = self
                .runtime
                .send(peer, ClipboardMessage::Request { offer_id })
                .await;
        } else {
            // `transfer = "on_focus_enter"`: hold it until this node is the one being typed
            // into, which is what `on_focus` is for.
            self.pending_request = Some((peer, offer_id));
        }
        Vec::new()
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
        let _ = self
            .runtime
            .send(peer, ClipboardMessage::Request { offer_id })
            .await;
    }

    /// Hands verified text to the agent, in chunks that fit an IPC frame.
    fn deliver_to_agent(&mut self, text: String, version: u64) -> Vec<Out> {
        self.payload = Vec::new();
        let bytes = text.into_bytes();
        let count = chunks_of(bytes.len());
        let hash = cb::sha256(&bytes);
        (0..count)
            .map(|index| {
                let start = index as usize * IPC_CHUNK_BYTES;
                let end = (start + IPC_CHUNK_BYTES).min(bytes.len());
                Out::Agent(ipc::agent_frame::Msg::SetClipboardText(
                    ipc::SetClipboardText {
                        origin_version: version,
                        chunk_index: index,
                        chunk_count: count,
                        data: bytes[start..end].to_vec(),
                        sha256: if index + 1 == count {
                            hash.clone()
                        } else {
                            Vec::new()
                        },
                    },
                ))
            })
            .collect()
    }

    /// A member's input link came up: remember its connection id and clipboard capability,
    /// and, if this node holds a payload, offer it so a late joiner converges.
    pub async fn on_link_up(
        &mut self,
        peer: NodeId,
        connection_id: [u8; 16],
        text_clipboard: bool,
        remote_ip: std::net::IpAddr,
    ) -> Vec<Out> {
        let state = self.peers.entry(peer).or_default();
        state.parent = Some(connection_id);
        state.text_clipboard = text_clipboard;
        state.remote_ip = Some(remote_ip);
        if !self.settings.enabled || self.payload.is_empty() {
            return Vec::new();
        }
        let Ok(offer_id) = identity::random16() else {
            return Vec::new();
        };
        let content = cb::Content {
            bytes: self.payload.len() as u64,
            markers: Vec::new(),
        };
        match self
            .source
            .local_copy(&content, &[], &self.settings.policy, offer_id)
        {
            Ok(offer) => self.fan_out(offer).await,
            Err(_) => Vec::new(),
        }
    }

    /// A member's input link went down: its clipboard channel goes with it.
    pub async fn on_link_down(&mut self, peer: NodeId) {
        self.peers.remove(&peer);
        self.waiting.remove(&peer);
        self.offered.remove(&peer);
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
    }
}

/// `chunk_count` for an IPC payload: `1..=64` (`MAX_FRAME_IPC` is 64 KiB, so 64 frames reach
/// 2 MiB — more than the 1 MiB the policy refuses, which is what lets a payload over the limit
/// be answered with `CLIPBOARD_TOO_LARGE` instead of silently truncated).
fn chunks_of(size: usize) -> u32 {
    size.div_ceil(IPC_CHUNK_BYTES).clamp(1, 64) as u32
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
#[derive(Clone)]
struct Handle {
    inner: Arc<Mutex<Inner>>,
    identity: Arc<identity::NodeIdentity>,
    trust: identity::TrustStore,
    events: mpsc::Sender<Inbound>,
    epoch: Instant,
}

struct Inner {
    /// The one-use binding tokens.
    grants: Grants,
    /// Bound channels, by peer.
    channels: BTreeMap<NodeId, mpsc::Sender<ClipboardMessage>>,
}

impl Handle {
    async fn start(
        identity: Arc<identity::NodeIdentity>,
        trust: identity::TrustStore,
        listen: SocketAddr,
        queue_capacity: usize,
    ) -> Result<(Self, mpsc::Receiver<Inbound>), String> {
        let (events, receiver) = mpsc::channel(256);
        let handle = Handle {
            inner: Arc::new(Mutex::new(Inner {
                grants: Grants::default(),
                channels: BTreeMap::new(),
            })),
            identity,
            trust,
            events,
            epoch: Instant::now(),
        };
        let listener =
            ClipboardListener::bind(Arc::clone(&handle.identity), handle.trust.clone(), listen)
                .await
                .map_err(|error| error.to_string())?;
        tracing::info!(address = %listener.local_addr(), "clipboard channel listening");
        let acceptor = handle.clone();
        tokio::spawn(async move { acceptor.accept_loop(listener, queue_capacity).await });
        Ok((handle, receiver))
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
        let channel = self.inner.lock().await.channels.get(&peer).cloned();
        match channel {
            Some(tx) => tx.send(message).await.map_err(|_| ()),
            None => Err(()),
        }
    }

    /// Paces a payload onto the peer's channel from a task of its own (`KVM-042`).
    async fn send_transfer(
        &self,
        peer: NodeId,
        offer_id: [u8; 16],
        payload: Vec<u8>,
        rate_bytes_per_s: u64,
    ) -> Result<(), ()> {
        let channel = self.inner.lock().await.channels.get(&peer).cloned();
        let Some(tx) = channel else {
            return Err(());
        };
        let mut outgoing =
            Outgoing::new(offer_id, payload, rate_bytes_per_s, cb::MAX_BYTES, 0).map_err(|_| ())?;
        let size = outgoing.size_bytes();
        let started = Instant::now();
        tokio::spawn(async move {
            while let Some((message, delay_ms)) =
                outgoing.next(started.elapsed().as_millis() as u64)
            {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                if tx.send(message).await.is_err() {
                    return;
                }
            }
            tracing::info!(peer = %peer, bytes = size, "clipboard transfer sent");
        });
        Ok(())
    }

    /// Drops the channel to `peer`.
    async fn close(&self, peer: NodeId) {
        self.inner.lock().await.channels.remove(&peer);
    }

    /// The accept loop: bind, validate, then read.
    async fn accept_loop(&self, mut listener: ClipboardListener, queue_capacity: usize) {
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
        let (reader, writer) = connection.into_split();
        let (tx, rx) = mpsc::channel(queue_capacity.max(1));
        self.inner.lock().await.channels.insert(peer, tx);
        let events = self.events.clone();
        let _ = events.send(Inbound::Bound { peer, channel_id }).await;
        let handle = self.clone();
        tokio::spawn(async move {
            handle.read_task(peer, reader, events.clone()).await;
            let _ = events.send(Inbound::Down { peer }).await;
            handle.inner.lock().await.channels.remove(&peer);
        });
        tokio::spawn(async move { write_task(writer, rx).await });
    }

    /// The reader half of one channel.
    async fn read_task(
        &self,
        peer: NodeId,
        mut reader: ClipboardReader,
        events: mpsc::Sender<Inbound>,
    ) {
        loop {
            match reader.recv().await {
                Ok(message) => {
                    if events
                        .send(Inbound::Message { peer, message })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => return,
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
    async fn dial(
        &self,
        peer: NodeId,
        address: SocketAddr,
        parent: [u8; 16],
        channel_id: [u8; 16],
        token: [u8; 32],
        queue_capacity: usize,
    ) {
        let result = dial_clipboard(
            &self.identity,
            &self.trust,
            peer,
            address,
            parent,
            channel_id,
            token,
        )
        .await;
        match result {
            Ok(connection) => {
                self.serve(peer, channel_id, connection, queue_capacity)
                    .await
            }
            // A refused bind shows up on the peer as a closed connection; the next grant from
            // that peer opens a fresh one.
            Err(error) => {
                tracing::warn!(peer = %peer, %address, %error, "the clipboard bind failed")
            }
        }
    }
}

/// Writes one channel's queue; the reader is a separate task so a blocked read never stalls a
/// send (`KVM-042`).
async fn write_task(mut writer: ClipboardWriter, mut outbound: mpsc::Receiver<ClipboardMessage>) {
    while let Some(message) = outbound.recv().await {
        if writer.send(message).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            identity::TrustStore::new(),
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
    fn an_ipc_payload_of_one_mebibyte_is_thirty_two_frames() {
        assert_eq!(chunks_of(0), 1, "an empty selection is one empty chunk");
        assert_eq!(chunks_of(IPC_CHUNK_BYTES), 1);
        assert_eq!(chunks_of(IPC_CHUNK_BYTES + 1), 2);
        assert_eq!(chunks_of(cb::MAX_BYTES as usize), 32);
        assert_eq!(chunks_of(cb::MAX_BYTES as usize + 1), 33);
        assert_eq!(
            chunks_of(cb::MAX_BYTES as usize * 2),
            64,
            "the count is capped: a payload over the limit is refused, never split further"
        );
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
        for reason in [
            cb::ClipboardError::HashMismatch,
            cb::ClipboardError::Invalid,
            cb::ClipboardError::Secret,
        ] {
            assert_eq!(wire_error(reason), ErrorCode::InvalidFrame);
        }
    }
}
