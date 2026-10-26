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

/// Everything `[clipboard]` decides.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// `clipboard.enabled`: the capability bit and the whole channel hang off it.
    pub enabled: bool,
    /// The pure policy: `skip_secret_marked` and `max_bytes`.
    pub policy: cb::Policy,
    /// Whether copies follow focus or are offered to every peer.
    pub routing_policy: config::ClipboardRoutingPolicy,
    /// Session-owned focus settling delay.
    pub settle_ms: u32,
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
            routing_policy: clipboard.policy,
            settle_ms: clipboard.settle_ms,
            rate_bytes_per_s: clipboard.rate_bytes_per_s,
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
    /// Hand this frame to the agent.
    Agent(ipc::agent_frame::Msg),
    /// A completed transfer, for the session-owned history ring.
    History {
        /// The transfer's remote peer.
        peer: NodeId,
        /// Which side sent the payload.
        direction: ipc::ClipboardHistoryDirection,
        /// Full verified UTF-8 text.
        text: String,
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
    /// A full outbound transfer successfully written to its channel.
    Sent {
        /// The receiving peer.
        peer: NodeId,
        /// Source version of the addressed offer.
        version: u64,
        /// Digest of the sent payload.
        sha256: [u8; 32],
        /// The original payload, recovered without cloning.
        text: String,
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
    /// Offers waiting for their authorized channel to bind.
    waiting: BTreeMap<NodeId, PendingOffer>,
    /// Offered payloads not started yet: broadcasts wait for Request; addressed deliveries
    /// start after the session has bound their application acknowledgement.
    offered: BTreeMap<NodeId, PendingOffer>,
    /// The offer a peer made and we held back (`transfer = "on_focus_enter"`).
    pending_request: Option<(NodeId, [u8; 16])>,
    /// One validated offer per incoming transfer, keyed by `offer_id`.
    incoming: BTreeMap<[u8; 16], Incoming>,
    /// Peers whose connection is closed (link down, revoke): frames still queued from them are
    /// dropped. Cleared when the peer's link comes up again.
    down: BTreeSet<NodeId>,
    /// The agent's chunks for the copy it is currently reporting.
    local: Option<LocalAssembly>,
    /// Validated local carry; history applies and explicit sends do not replace it.
    payload: Vec<u8>,
    carried: Option<(cb::Offer, [u8; 32])>,
    /// Latest outbound wire transfer per peer, retained through its Request/Cancel.
    deliveries: BTreeMap<NodeId, ([u8; 16], u64, [u8; 32])>,
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

struct PendingOffer {
    message: ClipboardMessage,
    payload: Vec<u8>,
    immediate: bool,
}

/// A validated payload a peer offered.
struct Incoming {
    /// Who is sending it: only that peer may complete it, and it ends with that peer's link.
    peer: NodeId,
    offer: cb::Offer,
    secret: bool,
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
            down: BTreeSet::new(),
            local: None,
            payload: Vec::new(),
            carried: None,
            deliveries: BTreeMap::new(),
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
        self.payload.clear();
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

    /// Whether clipboard transfers are on, for `Status.clipboard_enabled`.
    pub const fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Updates session-owned routing settings without restarting the clipboard runtime.
    pub fn set_routing_policy(&mut self, policy: config::ClipboardRoutingPolicy, settle_ms: u32) {
        if self.settings.routing_policy != policy {
            self.carried = None;
            self.payload.clear();
        }
        self.settings.routing_policy = policy;
        self.settings.settle_ms = settle_ms;
    }

    /// One `ClipboardChanged` chunk from the agent.
    ///
    /// Nothing is decided — and nothing is offered — until the last chunk arrived with a
    /// matching hash and valid UTF-8.
    pub async fn on_agent_frame(&mut self, message: &ipc::ClipboardChanged) -> Vec<Out> {
        // Secret chunks must not even enter the local assembly, regardless of legacy policy.
        let markers = markers_of(&message.markers);
        if !markers.is_empty() {
            if message.chunk_index == 0 {
                tracing::info!(
                    code = cb::ClipboardError::Secret.code(),
                    bytes = message.size_bytes,
                    "clipboard copy not offered"
                );
            }
            self.local = None;
            return self.clear_carry().await;
        }
        let fresh = self.local.as_ref().is_none_or(|assembly| {
            assembly.version != message.origin_version || assembly.count != message.chunk_count
        });
        let (payload, markers) = match self.take_chunk(message) {
            Ok(Some(assembled)) => assembled,
            Ok(None) => {
                return if fresh {
                    self.clear_carry().await
                } else {
                    Vec::new()
                };
            }
            Err(error) => {
                tracing::warn!(?error, "clipboard chunks from the agent refused");
                return self.clear_carry().await;
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

    /// A local copy supersedes incoming transfers and, when valid, becomes the carry.
    async fn on_local_copy(&mut self, payload: Vec<u8>, markers: Vec<cb::Marker>) -> Vec<Out> {
        let mut out = self.clear_carry().await;
        let refusal = if !self.settings.enabled {
            Some(cb::ClipboardError::Disabled)
        } else if !markers.is_empty() {
            Some(cb::ClipboardError::Secret)
        } else if std::str::from_utf8(&payload).is_err() {
            Some(cb::ClipboardError::Invalid)
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
        let sha256 = cb::sha256(&payload).try_into().expect("SHA-256 length");
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
        out.push(Out::History {
            peer: offer.origin.into(),
            direction: ipc::ClipboardHistoryDirection::Copied,
            text: std::str::from_utf8(&payload)
                .expect("validated above")
                .to_owned(),
            secret: false,
        });
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
        self.payload.clear();
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
        self.require_peer(peer)?;
        let (_, hash) = self.carried.as_ref().ok_or("no carried clipboard")?;
        if *hash != sha256 {
            return Err("carried clipboard changed".to_owned());
        }
        let offer = self.mint_delivery_offer(self.payload.len())?;
        self.offer_to(peer, offer, self.payload.clone(), sha256, true)
            .await
    }

    /// Sends a history item only to the requested peer, preserving the current carry.
    pub async fn send_text_to(&mut self, peer: NodeId, text: &str) -> Result<Vec<Out>, String> {
        self.require_peer(peer)?;
        let offer = self.mint_delivery_offer(text.len())?;
        let hash = cb::sha256(text.as_bytes())
            .try_into()
            .expect("SHA-256 length");
        self.offer_to(peer, offer, text.as_bytes().to_vec(), hash, true)
            .await
    }

    fn mint_delivery_offer(&mut self, bytes: usize) -> Result<cb::Offer, String> {
        let offer_id = identity::random16().map_err(|error| error.to_string())?;
        let from_peer = self.source.applied_from_peer();
        let offer = self
            .source
            .local_copy(
                &cb::Content::text(bytes as u64),
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

    fn require_peer(&self, peer: NodeId) -> Result<(), String> {
        if !self.settings.enabled {
            return Err("clipboard is disabled".to_owned());
        }
        if self
            .peers
            .get(&peer)
            .is_none_or(|state| state.parent.is_none() || !state.text_clipboard)
        {
            return Err("peer has no live text clipboard link".to_owned());
        }
        Ok(())
    }

    async fn offer_to(
        &mut self,
        peer: NodeId,
        offer: cb::Offer,
        payload: Vec<u8>,
        sha256: [u8; 32],
        immediate: bool,
    ) -> Result<Vec<Out>, String> {
        self.require_peer(peer)?;
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
            ..
        } = offer
        else {
            unreachable!("only offers are retained");
        };
        let sha256 = self
            .deliveries
            .get(&peer)
            .filter(|(id, _, _)| *id == offer_id)
            .map(|(_, _, hash)| *hash)
            .unwrap_or_else(|| cb::sha256(&payload).try_into().expect("SHA-256 length"));
        let result = self
            .runtime
            .send_transfer(
                peer,
                offer_id,
                (origin_version, sha256),
                payload,
                self.settings.rate_bytes_per_s,
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
            if self.require_peer(peer).is_ok() {
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

    /// One message from a peer on the clipboard channel.
    pub async fn on_peer_message(&mut self, peer: NodeId, message: ClipboardMessage) -> Vec<Out> {
        if self.down.contains(&peer) {
            // Frames of a connection that is already closed (a revoked peer's last, queued ones).
            return Vec::new();
        }
        match message {
            offer @ ClipboardMessage::Offer { .. } => self.on_offer(peer, offer).await,
            ClipboardMessage::Request { offer_id } => {
                self.send_offered(peer, offer_id).await;
                Vec::new()
            }
            ClipboardMessage::Data {
                offer_id,
                chunk_index,
                chunk_count,
                data,
            } => {
                let outcome = match self.incoming.get_mut(&offer_id).filter(|i| i.peer == peer) {
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
                if self.incoming.get(&offer_id).is_none_or(|i| i.peer != peer) {
                    return Vec::new();
                }
                if self.pending_request == Some((peer, offer_id)) {
                    self.pending_request = None;
                }
                let Some(incoming) = self.incoming.remove(&offer_id) else {
                    return Vec::new();
                };
                match incoming.assembler.finish(&sha256) {
                    Ok(text) => {
                        // The payload is now this node's, but it is *not* ours to offer: the
                        // fan-out stops here (ADR-008's echo rule).
                        self.source.apply_from_peer(&incoming.offer);
                        if self.settings.routing_policy == config::ClipboardRoutingPolicy::Broadcast
                        {
                            self.carried = None;
                            self.payload.clear();
                        }
                        tracing::info!(
                            peer = %peer,
                            bytes = text.len(),
                            version = incoming.offer.version,
                            "clipboard payload applied"
                        );
                        let mut out = vec![Out::ApplyPending {
                            peer,
                            version: incoming.offer.version,
                            sha256: sha256.try_into().expect("validated SHA-256 length"),
                        }];
                        out.extend(agent_text_chunks(&text, incoming.offer.version));
                        out.push(Out::History {
                            peer,
                            direction: ipc::ClipboardHistoryDirection::Received,
                            text,
                            secret: incoming.secret,
                        });
                        out
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
        if mime != cb::MIME {
            return vec![Out::Cancel {
                peer,
                offer_id,
                reason: cb::ClipboardError::Invalid,
            }];
        }
        let Ok(origin) = <[u8; 32]>::try_from(origin_node_id.as_slice()) else {
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
        self.incoming.insert(
            offer_id,
            Incoming {
                peer,
                offer,
                secret,
                assembler,
            },
        );
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
        let _ = self
            .runtime
            .send(peer, ClipboardMessage::Request { offer_id })
            .await;
    }

    /// Applies history locally without minting a network offer, even when sharing is disabled.
    pub(crate) fn history_to_agent(&mut self, text: &str) -> Vec<Out> {
        if self.settings.routing_policy == config::ClipboardRoutingPolicy::Broadcast {
            self.carried = None;
            self.payload.clear();
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
        out.extend(history_to_agent(&mut self.source, text));
        out
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
        if let Some(&(_, version, sha256)) = self.deliveries.get(&peer) {
            self.cancel_delivery(peer, version, sha256).await;
        }
        self.waiting.remove(&peer);
        self.offered.remove(&peer);
    }
}

/// Chunks a history item for the local agent without offering it to any peer.
pub(crate) fn history_to_agent(source: &mut cb::Source, text: &str) -> Vec<Out> {
    let offer = source
        .local_copy(
            &cb::Content::text(text.len() as u64),
            &[],
            &cb::Policy {
                enabled: true,
                skip_secret_marked: true,
                max_bytes: cb::MAX_BYTES,
            },
            [0; 16],
        )
        .expect("history text is bounded by the clipboard payload limit");
    source.apply_from_peer(&offer);
    agent_text_chunks(text, offer.version)
}

fn agent_text_chunks(text: &str, version: u64) -> Vec<Out> {
    let bytes = text.as_bytes();
    let count = chunks_of(bytes.len());
    let hash = cb::sha256(bytes);
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
    reader: tokio::task::AbortHandle,
    writer: tokio::task::AbortHandle,
    transfers: BTreeMap<[u8; 16], tokio::task::AbortHandle>,
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
        let channel = self
            .inner
            .lock()
            .await
            .conns
            .get(&peer)
            .map(|conn| conn.tx.clone());
        match channel {
            Some(tx) => tx
                .send(QueuedMessage {
                    message,
                    written: None,
                })
                .await
                .map_err(|_| ()),
            None => Err(()),
        }
    }

    /// Paces a payload onto the peer's channel from a task of its own (`KVM-042`).
    async fn send_transfer(
        &self,
        peer: NodeId,
        offer_id: [u8; 16],
        identity: (u64, [u8; 32]),
        payload: Vec<u8>,
        rate_bytes_per_s: u64,
        secret: bool,
    ) -> Result<(), ()> {
        let (version, sha256) = identity;
        let mut inner = self.inner.lock().await;
        let Some(conn) = inner.conns.get_mut(&peer) else {
            return Err(());
        };
        let tx = conn.tx.clone();
        let Some(tasks) = self.tasks.upgrade() else {
            return Err(());
        };
        let mut state = tasks.state.lock().expect("task registry");
        if state.closed {
            return Err(());
        }
        state.prune();
        conn.transfers.retain(|_, task| !task.is_finished());
        let mut outgoing =
            Outgoing::new(offer_id, payload, rate_bytes_per_s, cb::MAX_BYTES, 0).map_err(|_| ())?;
        let size = outgoing.size_bytes();
        let started = Instant::now();
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            while let Some((message, delay_ms)) =
                outgoing.next(started.elapsed().as_millis() as u64)
            {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                let (written, completion) = tokio::sync::oneshot::channel();
                if tx
                    .send(QueuedMessage {
                        message,
                        written: Some(written),
                    })
                    .await
                    .is_err()
                {
                    let _ = events
                        .send(Inbound::Failed {
                            peer,
                            version,
                            sha256,
                            detail: "clipboard channel closed".to_owned(),
                        })
                        .await;
                    return;
                }
                if !matches!(completion.await, Ok(true)) {
                    let _ = events
                        .send(Inbound::Failed {
                            peer,
                            version,
                            sha256,
                            detail: "clipboard channel write failed".to_owned(),
                        })
                        .await;
                    return;
                }
            }
            tracing::info!(peer = %peer, bytes = size, "clipboard transfer sent");
            if let Ok(text) = String::from_utf8(outgoing.into_payload()) {
                let _ = events
                    .send(Inbound::Sent {
                        peer,
                        version,
                        sha256,
                        text,
                        secret,
                    })
                    .await;
            }
        });
        if let Some(old) = conn.transfers.insert(offer_id, task.abort_handle()) {
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
        let (reader, writer) = connection.into_split();
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
                handle.read_task(peer, reader, reader_events.clone()).await;
                let mut inner = handle.inner.lock().await;
                // Only the connection that is still the peer's current one unregisters itself and
                // reports the channel down: an older one that was replaced is not the peer's.
                let current = inner
                    .conns
                    .get(&peer)
                    .is_some_and(|conn| conn.generation == generation);
                if current {
                    inner.conns.remove(&peer);
                }
                drop(inner);
                if current {
                    let _ = reader_events.send(Inbound::Down { peer }).await;
                }
            });
            let reader_abort = reader_task.abort_handle();
            task_state.tasks.push(reader_task);
            let writer_task = tokio::spawn(async move { write_task(writer, rx).await });
            let writer_abort = writer_task.abort_handle();
            task_state.tasks.push(writer_task);
            drop(task_state);
            inner.conns.insert(
                peer,
                Conn {
                    generation,
                    tx,
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
        let _ = events.send(Inbound::Bound { peer, channel_id }).await;
        // Both stream halves are already registered before publishing Bound.
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

/// Writes one channel's queue; the reader is a separate task so a blocked read never stalls a
/// send (`KVM-042`).
struct QueuedMessage {
    message: ClipboardMessage,
    written: Option<tokio::sync::oneshot::Sender<bool>>,
}

async fn write_task(mut writer: ClipboardWriter, mut outbound: mpsc::Receiver<QueuedMessage>) {
    while let Some(queued) = outbound.recv().await {
        let success = writer.send(queued.message).await.is_ok();
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
                    (1, hash),
                    payload,
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
        assert_eq!(endpoint.waiting[&peer].payload, b"new destination text");
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
                    receiver.on_focus().await;
                    sender
                        .on_peer_message(peer, ClipboardMessage::Request { offer_id })
                        .await;
                }
                loop {
                    let queued = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                        .await
                        .expect("wire data starts after delivery intent")
                        .expect("channel");
                    let complete = matches!(&queued.message, ClipboardMessage::Complete { .. });
                    let applied = receiver.on_peer_message(origin, queued.message).await;
                    if let Some(written) = queued.written {
                        written.send(true).unwrap();
                    }
                    if complete {
                        assert!(applied.iter().any(|event| matches!(
                            event,
                            Out::Agent(ipc::agent_frame::Msg::SetClipboardText(_))
                        )));
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
        assert_eq!(endpoint.waiting[&target].payload, b"current");
        assert!(!endpoint.waiting.contains_key(&other));
        endpoint.history_to_agent("local history");
        assert_eq!(endpoint.payload, b"current");
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
        assert_eq!(endpoint.waiting[&target].payload, b"old history");
        assert_eq!(endpoint.payload, b"current");
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
        assert_eq!(endpoint.waiting[&target].payload, b"current");
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
        for mode in 0..4 {
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
                    (1, cb::sha256(b"text").try_into().unwrap()),
                    b"text".to_vec(),
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
                    matches!(event, Ok(Some(Inbound::Sent { peer: sent_peer, text, secret: marked, .. }))
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
        for version in 1..=2 {
            let out = history_to_agent(&mut source, &text);
            assert_eq!(out.len(), 32);
            let mut received = Vec::new();
            for event in out {
                let Out::Agent(ipc::agent_frame::Msg::SetClipboardText(chunk)) = event else {
                    panic!("history copy must not offer to a peer");
                };
                assert_eq!(chunk.origin_version, version);
                received.extend(chunk.data);
                if chunk.chunk_index + 1 == chunk.chunk_count {
                    assert_eq!(chunk.sha256, cb::sha256(&received));
                }
            }
            assert_eq!(received, text.as_bytes());
            assert!(
                source.applied_from_peer(),
                "history is never a network source"
            );
        }
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
                        origin_node_id: [0x31; 32],
                        origin_version: 1,
                        mime: cb::MIME.to_owned(),
                        size_bytes: 4,
                        secret_marked: secret,
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
            assert!(matches!(out.first(), Some(Out::ApplyPending {
                peer: sender, version: 1, sha256,
            }) if *sender == peer && sha256.as_slice() == cb::sha256(b"text").as_slice()));
            assert!(out.iter().any(|event| matches!(event, Out::History {
                peer: sender, direction: ipc::ClipboardHistoryDirection::Received,
                text, secret: marked,
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
                reader: reader.abort_handle(),
                writer: writer.abort_handle(),
                transfers: BTreeMap::new(),
            },
        );
        let offer = ClipboardMessage::Offer {
            offer_id,
            origin_node_id: [0x31; 32],
            origin_version: 1,
            mime: cb::MIME.to_owned(),
            size_bytes: (cb::CHUNK_BYTES * 3) as u64,
            secret_marked: false,
        };
        endpoint.offered.insert(
            peer,
            PendingOffer {
                message: offer.clone(),
                payload: Vec::new(),
                immediate: false,
            },
        );
        endpoint.waiting.insert(
            peer,
            PendingOffer {
                message: offer,
                payload: Vec::new(),
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
                ),
                vec![b'x'; cb::CHUNK_BYTES * 3],
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
                ),
                vec![b'x'; cb::CHUNK_BYTES * 3],
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
                        origin_node_id: [0x31; 32],
                        origin_version: 10,
                        mime: cb::MIME.to_owned(),
                        size_bytes: 5,
                        secret_marked: false,
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
            assert!(!out.iter().any(|event| matches!(event, Out::Agent(_))));
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
            },
            routing_policy: config::ClipboardRoutingPolicy::Follow,
            settle_ms: 700,
            rate_bytes_per_s: 0,
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
        let origin = [0x31u8; 32];
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
            out.iter().all(|out| !matches!(out, Out::Agent(_))),
            "a superseded incoming transfer must not reach the agent"
        );
        assert_eq!(
            clipboard.payload, local,
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
            },
            routing_policy: config::ClipboardRoutingPolicy::Follow,
            settle_ms: 700,
            rate_bytes_per_s: 0,
            eager: false,
        };
        let (clipboard, _events) = Clipboard::start(
            identity.node_id(),
            settings,
            identity,
            Arc::new(RwLock::new(BTreeSet::new())),
            "127.0.0.1:0".parse().expect("an address"),
            4,
        )
        .await
        .expect("the clipboard endpoint starts");
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
                    origin_node_id: [0x31u8; 32],
                    origin_version: 9,
                    mime: cb::MIME.to_owned(),
                    size_bytes: text.len() as u64,
                    secret_marked: false,
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
        assert!(out.iter().all(|out| !matches!(out, Out::Agent(_))));
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
            out.iter().all(|out| !matches!(out, Out::Agent(_))),
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
}
