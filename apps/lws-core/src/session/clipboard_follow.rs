//! Session-owned metadata routing and bounded ordering; payloads stay in the endpoint.
use super::*;
use crate::clipboard_routing::{Delivery, Item, Routing};

type Key = (NodeId, u64, [u8; 32]);
#[derive(Default)]
pub(super) struct State {
    pub peers: BTreeMap<NodeId, bool>,
    pub routing: Routing,
    pub hold: Option<Hold>,
    applying: BTreeMap<(u64, [u8; 32]), (NodeId, u64)>,
    apply_sequence: u64,
    apply_group: Option<(u64, u64, bool)>,
    forwards: BTreeMap<Key, NodeId>,
    deliveries: BTreeMap<Key, Delivery>,
    requested: BTreeMap<(NodeId, NodeId), Item>,
    sends: BTreeMap<Key, (u64, oneshot::Sender<ctl::Response>)>,
}
impl State {
    fn next_apply_sequence(&mut self) -> u64 {
        self.apply_sequence = self
            .apply_sequence
            .checked_add(1)
            .expect("local clipboard apply sequence exhausted");
        self.apply_sequence
    }
    fn begin_apply(&mut self, peer: NodeId, wire_version: u64, hash: [u8; 32]) {
        let sequence = self.next_apply_sequence();
        self.applying.retain(|_, (source, _)| *source != peer);
        self.applying.insert((sequence, hash), (peer, wire_version));
        self.apply_group = Some((wire_version, sequence, true));
    }
    fn rewrite_apply_chunk(&mut self, chunk: &mut protocol::ipc::SetClipboardText) {
        let sequence = match self.apply_group {
            Some((wire_version, sequence, fresh))
                if wire_version == chunk.origin_version && (fresh || chunk.chunk_index != 0) =>
            {
                sequence
            }
            _ => self.next_apply_sequence(),
        };
        self.apply_group = Some((chunk.origin_version, sequence, false));
        chunk.origin_version = sequence;
        if chunk.chunk_count == 0 || chunk.chunk_index.saturating_add(1) >= chunk.chunk_count {
            self.apply_group = None;
        }
    }
    fn bind_pending(
        &mut self,
        origin: NodeId,
        wire_version: u64,
        hash: [u8; 32],
        target: NodeId,
    ) -> bool {
        let Some(item) = self
            .routing
            .carried()
            .filter(|item| item.origin == origin && item.hash == hash)
        else {
            return false;
        };
        if wire_version <= item.version {
            return false;
        }
        if self.requested.get(&(origin, target)) != Some(&item) {
            return false;
        }
        self.requested.remove(&(origin, target));
        self.deliveries
            .retain(|_, delivery| delivery.target != target);
        self.deliveries
            .insert((origin, wire_version, hash), Delivery { item, target });
        if let Some(hold) = self
            .hold
            .as_mut()
            .filter(|hold| hold.peer == target && hold.item == item)
        {
            hold.version = Some(wire_version);
        }
        true
    }
    fn fail_request(&mut self, item: Item, target: NodeId) -> bool {
        let key = (item.origin, target);
        if self.routing.carried() != Some(item) || self.requested.get(&key) != Some(&item) {
            return false;
        }
        self.requested.remove(&key);
        self.routing.failed(Delivery { item, target });
        true
    }
}
pub(super) struct Hold {
    peer: NodeId,
    ack_peer: NodeId,
    lease: fr::LeaseId,
    item: Item,
    version: Option<u64>,
    pub deadline: u64,
    events: Vec<InputEvent>,
}
impl Hold {
    fn new(
        peer: NodeId,
        lease: fr::LeaseId,
        item: Item,
        now: u64,
        supported: bool,
    ) -> Option<Self> {
        supported.then_some(Self {
            peer,
            ack_peer: peer,
            lease,
            item,
            version: Some(item.version),
            deadline: now.saturating_add(150),
            events: Vec::new(),
        })
    }
    fn ready(&self, now: u64, ack: Option<(NodeId, u64, [u8; 32], bool)>) -> bool {
        now >= self.deadline
            || ack.is_some_and(|(peer, version, hash, ok)| {
                ok && peer == self.ack_peer
                    && Some(version) == self.version
                    && hash == self.item.hash
            })
    }
    fn into_events(self, current: Option<(NodeId, fr::LeaseId)>) -> Option<Vec<InputEvent>> {
        (current == Some((self.peer, self.lease))).then_some(self.events)
    }
}
fn valid_carry(peer: NodeId, item: Item) -> bool {
    item.origin == peer
}
fn valid_clear(current: Option<Item>, item: Item) -> bool {
    current == Some(item)
}
fn can_send(trusted: bool, live: bool) -> bool {
    trusted && live
}
pub(super) fn activation(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key {
            state: input_model::KeyState::Down,
            ..
        } | InputEvent::Button {
            state: input_model::KeyState::Down,
            ..
        }
    )
}
fn route_item(message: &protocol::wire::ClipboardRoute) -> Option<Item> {
    Some(Item {
        origin: NodeId::new(message.origin_node_id.as_slice().try_into().ok()?),
        version: message.origin_version,
        hash: message.sha256.as_slice().try_into().ok()?,
    })
}
fn response(detail: String) -> ctl::Response {
    ctl::Response::Error {
        code: "CLIPBOARD_SEND_FAILED".into(),
        detail,
    }
}
impl Session {
    pub(super) fn clipboard_destination(&self) -> NodeId {
        self.router
            .lease()
            .source()
            .filter(|lease| lease.state == fr::SourceState::Remote)
            .map_or(self.node_id, |lease| lease.peer)
    }
    pub(super) async fn send_clipboard_route(
        &self,
        peer: NodeId,
        action: &str,
        item: Item,
        target: NodeId,
    ) -> Result<(), String> {
        if !self
            .clipboard_follow
            .peers
            .get(&peer)
            .copied()
            .unwrap_or(false)
        {
            return Err("peer does not support clipboard routing".into());
        }
        let handle = self.manager.handle(&peer).await.ok_or("peer is offline")?;
        handle
            .send_clipboard_route(protocol::wire::ClipboardRoute {
                action: action.into(),
                origin_node_id: item.origin.as_bytes().to_vec(),
                origin_version: item.version,
                sha256: item.hash.to_vec(),
                target_node_id: if matches!(action, "deliver" | "pending" | "failed") {
                    target.as_bytes().to_vec()
                } else {
                    Vec::new()
                },
            })
            .await
            .map_err(|error| error.to_string())
    }
    async fn announce_clipboard(&self, action: &str, item: Item) {
        for (&peer, &supported) in &self.clipboard_follow.peers {
            if supported {
                let _ = self
                    .send_clipboard_route(peer, action, item, self.node_id)
                    .await;
            }
        }
    }
    pub(super) async fn clipboard_carried(&mut self, item: Item) {
        self.clipboard_follow.routing.copy(item);
        self.clipboard_follow.deliveries.clear();
        self.clipboard_follow.requested.clear();
        self.announce_clipboard("carry", item).await;
    }
    pub(super) async fn clipboard_cleared(&mut self) {
        if let Some(item) = self.clipboard_follow.routing.carried() {
            self.announce_clipboard("clear", item).await;
        }
        self.clipboard_follow.routing.clear();
        self.clipboard_follow.deliveries.clear();
        self.clipboard_follow.requested.clear();
    }
    pub(super) async fn on_clipboard_route(
        &mut self,
        peer: NodeId,
        message: protocol::wire::ClipboardRoute,
    ) {
        if !self.trust.is_trusted(&peer)
            || !self
                .clipboard_follow
                .peers
                .get(&peer)
                .copied()
                .unwrap_or(false)
        {
            return;
        }
        let Some(item) = route_item(&message) else {
            return;
        };
        match message.action.as_str() {
            "carry" if valid_carry(peer, item) => {
                self.clipboard_follow.routing.copy(item);
                self.clipboard_follow.deliveries.clear();
                self.clipboard_follow.requested.clear();
            }
            "clear" if valid_clear(self.clipboard_follow.routing.carried(), item) => {
                self.clipboard_follow.routing.clear();
                self.clipboard_follow.deliveries.clear();
                self.clipboard_follow.requested.clear();
            }
            "addressed" => {
                if self.clipboard_follow.routing.addressed(item.hash) {
                    self.clipboard_follow.deliveries.clear();
                    self.clipboard_follow.requested.clear();
                    if let Some(endpoint) = self.clipboard.as_mut() {
                        endpoint.address_carried(item.hash);
                    }
                }
            }
            "pending" if self.router.is_owner() && item.origin == peer => {
                let Ok(bytes) = <[u8; 32]>::try_from(message.target_node_id.as_slice()) else {
                    return;
                };
                let target = NodeId::new(bytes);
                self.clipboard_follow
                    .bind_pending(peer, item.version, item.hash, target);
            }
            "failed" if self.router.is_owner() && item.origin == peer => {
                let Ok(bytes) = <[u8; 32]>::try_from(message.target_node_id.as_slice()) else {
                    return;
                };
                let target = NodeId::new(bytes);
                if self.clipboard_follow.fail_request(item, target)
                    && self
                        .clipboard_follow
                        .hold
                        .as_ref()
                        .is_some_and(|hold| hold.peer == target && hold.item == item)
                {
                    self.release_clipboard_hold().await;
                }
            }
            "deliver"
                if item.origin == peer
                    && message.target_node_id.as_slice() == self.node_id.as_bytes() =>
            {
                if let Some(endpoint) = self.clipboard.as_mut() {
                    endpoint.request_delivery(peer, item.version).await;
                }
            }
            "deliver"
                if self.router.owner().copied() == Some(peer) && item.origin == self.node_id =>
            {
                let Ok(bytes) = <[u8; 32]>::try_from(message.target_node_id.as_slice()) else {
                    return;
                };
                let target = NodeId::new(bytes);
                if !self.trust.is_trusted(&target)
                    || !self.clipboard_links.contains_key(&target)
                    || self.manager.handle(&target).await.is_none()
                {
                    let _ = self
                        .send_clipboard_route(peer, "failed", item, target)
                        .await;
                    return;
                }
                let result = match self.clipboard.as_mut() {
                    Some(endpoint) => endpoint.deliver_carried(target, item.hash).await,
                    None => Err("clipboard is disabled".into()),
                };
                match result {
                    Ok(out) => {
                        if let Some(version) = out.iter().find_map(|action| match action {
                            crate::clipboard::Out::Delivery { version, .. } => Some(*version),
                            _ => None,
                        }) {
                            let pending = Item { version, ..item };
                            if self
                                .send_clipboard_route(peer, "pending", pending, target)
                                .await
                                .is_err()
                            {
                                let _ = self
                                    .send_clipboard_route(peer, "failed", item, target)
                                    .await;
                                return;
                            }
                            self.clipboard_follow
                                .forwards
                                .retain(|(previous, _, _), _| *previous != target);
                            self.clipboard_follow
                                .forwards
                                .insert((target, version, item.hash), peer);
                        }
                        self.apply_clipboard(out).await;
                    }
                    Err(_) => {
                        let _ = self
                            .send_clipboard_route(peer, "failed", item, target)
                            .await;
                    }
                }
            }
            _ => {}
        }
    }
    async fn send_apply_ack(
        &self,
        peer: NodeId,
        version: u64,
        hash: [u8; 32],
        ok: bool,
        detail: String,
    ) {
        if self
            .clipboard_follow
            .peers
            .get(&peer)
            .copied()
            .unwrap_or(false)
        {
            if let Some(handle) = self.manager.handle(&peer).await {
                let _ = handle
                    .send_clipboard_applied(protocol::wire::ClipboardApplied {
                        origin_version: version,
                        sha256: hash.to_vec(),
                        ok,
                        detail,
                    })
                    .await;
            }
        }
    }
    pub(super) async fn on_agent_clipboard_applied(
        &mut self,
        ack: &protocol::ipc::ClipboardApplied,
    ) {
        let Ok(hash) = <[u8; 32]>::try_from(ack.sha256.as_slice()) else {
            return;
        };
        if let Some((peer, wire_version)) = self
            .clipboard_follow
            .applying
            .remove(&(ack.origin_version, hash))
        {
            self.send_apply_ack(peer, wire_version, hash, ack.ok, ack.detail.clone())
                .await;
        }
    }
    pub(super) async fn on_clipboard_applied(
        &mut self,
        peer: NodeId,
        ack: protocol::wire::ClipboardApplied,
    ) {
        let Ok(hash) = <[u8; 32]>::try_from(ack.sha256.as_slice()) else {
            return;
        };
        if !self.trust.is_trusted(&peer)
            || !self
                .clipboard_follow
                .peers
                .get(&peer)
                .copied()
                .unwrap_or(false)
        {
            return;
        }
        if let Some(owner) =
            self.clipboard_follow
                .forwards
                .remove(&(peer, ack.origin_version, hash))
        {
            self.send_apply_ack(owner, ack.origin_version, hash, ack.ok, ack.detail.clone())
                .await;
        }
        if ack.ok {
            self.clipboard_follow
                .deliveries
                .remove(&(peer, ack.origin_version, hash));
        } else {
            self.clipboard_delivery_failed(peer, ack.origin_version, hash, ack.detail.clone())
                .await;
        }
        self.finish_clipboard_send(peer, ack.origin_version, hash, ack.ok, ack.detail);
        if self.clipboard_follow.hold.as_ref().is_some_and(|hold| {
            hold.ready(
                self.now_ms(),
                Some((peer, ack.origin_version, hash, ack.ok)),
            )
        }) {
            self.release_clipboard_hold().await;
        }
    }
    pub(super) fn clipboard_delivery_started(
        &mut self,
        peer: NodeId,
        version: u64,
        hash: [u8; 32],
    ) {
        if let Some(hold) = self.clipboard_follow.hold.as_mut().filter(|hold| {
            hold.peer == peer && hold.item.origin == self.node_id && hold.item.hash == hash
        }) {
            hold.version = Some(version);
        }
        if let Some(item) = self
            .clipboard_follow
            .routing
            .carried()
            .filter(|item| item.origin == self.node_id && item.hash == hash)
        {
            self.clipboard_follow
                .deliveries
                .retain(|_, delivery| delivery.target != peer);
            self.clipboard_follow
                .deliveries
                .insert((peer, version, hash), Delivery { item, target: peer });
        }
    }
    pub(super) async fn clipboard_legacy_sent(
        &mut self,
        peer: NodeId,
        version: u64,
        hash: [u8; 32],
    ) {
        self.finish_clipboard_send(peer, version, hash, true, String::new());
        self.clipboard_follow
            .deliveries
            .remove(&(peer, version, hash));
        if let Some(owner) = self
            .clipboard_follow
            .forwards
            .remove(&(peer, version, hash))
        {
            self.send_apply_ack(owner, version, hash, true, String::new())
                .await;
        }
    }
    pub(super) async fn service_clipboard_follow(&mut self, activation: bool) {
        let now = self.now_ms();
        if self
            .clipboard_follow
            .hold
            .as_ref()
            .is_some_and(|hold| hold.ready(now, None))
        {
            self.release_clipboard_hold().await;
        }
        let expired: Vec<_> = self
            .clipboard_follow
            .sends
            .iter()
            .filter(|(_, (due, _))| *due <= now)
            .map(|(key, _)| *key)
            .collect();
        for (peer, version, hash) in expired {
            self.finish_clipboard_send(
                peer,
                version,
                hash,
                false,
                "clipboard transfer timed out".into(),
            );
        }
        if !self.router.is_owner() || self.pending_target.is_some() {
            return;
        }
        let target = self.clipboard_destination();
        self.clipboard_follow.routing.enter(target, now);
        let Some(delivery) = self.clipboard_follow.routing.settle(
            now,
            self.clipboard_settings.settle_ms,
            activation,
        ) else {
            return;
        };
        if target != self.node_id
            && (!self.clipboard_links.contains_key(&target)
                || self.manager.handle(&target).await.is_none())
        {
            self.clipboard_follow.routing.failed(delivery);
            return;
        }
        let supported = target == self.node_id
            || self
                .clipboard_follow
                .peers
                .get(&target)
                .copied()
                .unwrap_or(false);
        if target != self.node_id {
            if let Some(lease) = self.router.lease().source() {
                self.clipboard_follow.hold =
                    Hold::new(target, lease.lease_id, delivery.item, now, supported);
                if delivery.item.origin != self.node_id {
                    if let Some(hold) = self.clipboard_follow.hold.as_mut() {
                        hold.ack_peer = delivery.item.origin;
                        hold.version = None;
                    }
                }
            }
        }
        let result = if delivery.item.origin == self.node_id {
            match self.clipboard.as_mut() {
                Some(endpoint) => endpoint.deliver_carried(target, delivery.item.hash).await,
                None => Err("clipboard is disabled".into()),
            }
            .map(Some)
        } else {
            self.send_clipboard_route(delivery.item.origin, "deliver", delivery.item, target)
                .await
                .map(|()| None)
        };
        match result {
            Ok(Some(out)) => self.apply_clipboard(out).await,
            Ok(None) => {
                self.clipboard_follow
                    .requested
                    .insert((delivery.item.origin, target), delivery.item);
            }
            Err(_) => {
                self.clipboard_follow.routing.failed(delivery);
                self.release_clipboard_hold().await;
            }
        }
    }
    pub(super) async fn hold_clipboard_input(&mut self, events: &mut Vec<InputEvent>) -> bool {
        let current = self
            .router
            .lease()
            .source()
            .map(|lease| (lease.peer, lease.lease_id));
        if self
            .clipboard_follow
            .hold
            .as_ref()
            .is_some_and(|hold| current != Some((hold.peer, hold.lease)))
        {
            self.clipboard_follow.hold = None;
        }
        if let Some(hold) = self.clipboard_follow.hold.as_mut() {
            hold.events.append(events);
            if hold.events.len() >= 4096 {
                self.release_clipboard_hold().await;
            }
            true
        } else {
            false
        }
    }
    async fn release_clipboard_hold(&mut self) {
        if let Some(hold) = self.clipboard_follow.hold.take() {
            let current = self
                .router
                .lease()
                .source()
                .filter(|lease| lease.state == fr::SourceState::Remote)
                .map(|lease| (lease.peer, lease.lease_id));
            if let Some(events) = hold.into_events(current) {
                Box::pin(self.forward_captured(events)).await;
            }
        }
    }
    pub(super) fn finish_clipboard_send(
        &mut self,
        peer: NodeId,
        version: u64,
        hash: [u8; 32],
        ok: bool,
        detail: String,
    ) {
        if let Some((_, tx)) = self.clipboard_follow.sends.remove(&(peer, version, hash)) {
            let _ = tx.send(if ok {
                ctl::Response::Status("clipboard sent".into())
            } else {
                response(detail)
            });
        }
    }
    pub(super) fn fail_clipboard_sends(&mut self, peer: NodeId, detail: &str) {
        let keys: Vec<_> = self
            .clipboard_follow
            .sends
            .keys()
            .filter(|(target, _, _)| *target == peer)
            .copied()
            .collect();
        for (peer, version, hash) in keys {
            self.finish_clipboard_send(peer, version, hash, false, detail.into());
        }
    }
    pub(super) async fn clipboard_peer_down(&mut self, peer: NodeId, detail: &str) {
        self.fail_clipboard_sends(peer, detail);
        self.clipboard_follow
            .applying
            .retain(|_, (source, _)| *source != peer);
        let requests: Vec<_> = self
            .clipboard_follow
            .requested
            .iter()
            .filter(|((origin, target), _)| *origin == peer || *target == peer)
            .map(|(key, item)| (*key, *item))
            .collect();
        for ((origin, target), item) in requests {
            self.clipboard_follow.requested.remove(&(origin, target));
            self.clipboard_follow
                .routing
                .failed(Delivery { item, target });
        }
        let failed: Vec<_> = self
            .clipboard_follow
            .deliveries
            .iter()
            .filter(|((source, _, _), delivery)| *source == peer || delivery.target == peer)
            .map(|(key, delivery)| (*key, *delivery))
            .collect();
        for (key, delivery) in failed {
            self.clipboard_follow.deliveries.remove(&key);
            self.clipboard_follow.routing.failed(delivery);
        }
        let pending: Vec<_> = self
            .clipboard_follow
            .forwards
            .iter()
            .filter(|((target, _, _), owner)| *target == peer || **owner == peer)
            .map(|(key, value)| (*key, *value))
            .collect();
        for ((target, version, hash), owner) in pending {
            self.clipboard_follow
                .forwards
                .remove(&(target, version, hash));
            if owner != peer {
                self.send_apply_ack(owner, version, hash, false, detail.into())
                    .await;
            }
        }
        if self
            .clipboard_follow
            .hold
            .as_ref()
            .is_some_and(|hold| hold.peer == peer || hold.item.origin == peer)
        {
            self.release_clipboard_hold().await;
        }
    }
    pub(super) async fn send_clipboard_history(&mut self, id: u64, peer: NodeId) -> Handled {
        if !can_send(
            self.trust.is_trusted(&peer),
            self.clipboard_links.contains_key(&peer) && self.manager.handle(&peer).await.is_some(),
        ) {
            return response("peer is offline or untrusted".into()).into();
        }
        let Some(text) = self.clipboard_history.text(id) else {
            return response("history item not found".into()).into();
        };
        let out = match self.clipboard.as_mut() {
            Some(endpoint) => endpoint.send_text_to(peer, text).await,
            None => Err("clipboard is disabled".into()),
        };
        let out = match out {
            Ok(out) => out,
            Err(detail) => return response(detail).into(),
        };
        let Some((version, hash)) = out.iter().find_map(|out| match out {
            crate::clipboard::Out::Delivery {
                version, sha256, ..
            } => Some((*version, *sha256)),
            _ => None,
        }) else {
            return response("clipboard delivery was not started".into()).into();
        };
        let (tx, rx) = oneshot::channel();
        self.fail_clipboard_sends(peer, "superseded by a newer addressed send");
        self.clipboard_follow.sends.insert(
            (peer, version, hash),
            (self.now_ms().saturating_add(30_000), tx),
        );
        if self.clipboard_follow.routing.addressed(hash) {
            self.clipboard_follow.deliveries.clear();
            self.clipboard_follow.requested.clear();
            if let Some(endpoint) = self.clipboard.as_mut() {
                endpoint.address_carried(hash);
            }
        }
        self.announce_clipboard(
            "addressed",
            Item {
                origin: self.node_id,
                version,
                hash,
            },
        )
        .await;
        self.apply_clipboard(out).await;
        Handled::Deferred(rx)
    }
    pub(super) fn clipboard_apply_pending(&mut self, peer: NodeId, version: u64, hash: [u8; 32]) {
        self.clipboard_follow.begin_apply(peer, version, hash);
    }
    pub(super) fn prepare_clipboard_agent_frame(&mut self, frame: &mut Msg) {
        if let Msg::SetClipboardText(chunk) = frame {
            self.clipboard_follow.rewrite_apply_chunk(chunk);
        }
    }
    pub(super) async fn clipboard_delivery_failed(
        &mut self,
        peer: NodeId,
        version: u64,
        hash: [u8; 32],
        detail: String,
    ) {
        if let Some(owner) = self
            .clipboard_follow
            .forwards
            .remove(&(peer, version, hash))
        {
            self.send_apply_ack(owner, version, hash, false, detail)
                .await;
        }
        if let Some(delivery) = self
            .clipboard_follow
            .deliveries
            .remove(&(peer, version, hash))
        {
            self.clipboard_follow.routing.failed(delivery);
        }
        if self
            .clipboard_follow
            .hold
            .as_ref()
            .is_some_and(|hold| hold.ready(0, Some((peer, version, hash, true))))
        {
            self.release_clipboard_hold().await;
        }
    }
    pub(super) fn discard_clipboard_input(&mut self) {
        self.clipboard_follow.hold = None;
    }
    pub(super) async fn fail_clipboard_applies(&mut self) {
        for ((_, hash), (peer, version)) in std::mem::take(&mut self.clipboard_follow.applying) {
            self.send_apply_ack(
                peer,
                version,
                hash,
                false,
                "clipboard agent unavailable".into(),
            )
            .await;
        }
    }
    pub(super) async fn clipboard_disabled(&mut self) {
        self.release_clipboard_hold().await;
        self.clipboard_cleared().await;
        let peers: Vec<_> = self
            .clipboard_follow
            .sends
            .keys()
            .map(|(peer, _, _)| *peer)
            .collect();
        for peer in peers {
            self.fail_clipboard_sends(peer, "clipboard disabled");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_model::{HidKey, KeyState};

    fn node(n: u8) -> NodeId {
        NodeId::new([n; 32])
    }
    fn item() -> Item {
        Item {
            origin: node(1),
            version: 7,
            hash: [9; 32],
        }
    }
    fn key(usage: u16, state: KeyState) -> InputEvent {
        InputEvent::Key {
            key: HidKey::keyboard(usage),
            state,
        }
    }

    fn apply_chunk(version: u64, index: u32, count: u32) -> protocol::ipc::SetClipboardText {
        protocol::ipc::SetClipboardText {
            origin_version: version,
            chunk_index: index,
            chunk_count: count,
            data: Vec::new(),
            sha256: if index + 1 == count {
                vec![9; 32]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn same_wire_version_and_hash_from_two_origins_have_distinct_local_ack_keys() {
        let mut state = State::default();
        state.begin_apply(node(1), 1, [9; 32]);
        let mut first = apply_chunk(1, 0, 2);
        let mut last = apply_chunk(1, 1, 2);
        state.rewrite_apply_chunk(&mut first);
        state.rewrite_apply_chunk(&mut last);
        assert_eq!(first.origin_version, last.origin_version);
        state.begin_apply(node(2), 1, [9; 32]);
        let mut other = apply_chunk(1, 0, 1);
        state.rewrite_apply_chunk(&mut other);
        assert_ne!(first.origin_version, other.origin_version);
        assert_eq!(
            state.applying.remove(&(other.origin_version, [9; 32])),
            Some((node(2), 1))
        );
        assert_eq!(
            state.applying.remove(&(first.origin_version, [9; 32])),
            Some((node(1), 1))
        );
    }

    #[test]
    fn superseded_apply_ack_is_ignored_and_history_chunks_also_get_unique_sequence() {
        let mut state = State::default();
        let mut previous = None;
        for _ in 0..100 {
            state.begin_apply(node(1), 1, [9; 32]);
            let mut chunk = apply_chunk(1, 0, 1);
            state.rewrite_apply_chunk(&mut chunk);
            if let Some(old) = previous {
                assert!(!state.applying.contains_key(&(old, [9; 32])));
                assert!(chunk.origin_version > old);
            }
            assert_eq!(state.applying.len(), 1);
            previous = Some(chunk.origin_version);
        }
        let mut history_first = apply_chunk(1, 0, 2);
        let mut history_last = apply_chunk(1, 1, 2);
        state.rewrite_apply_chunk(&mut history_first);
        state.rewrite_apply_chunk(&mut history_last);
        assert_eq!(history_first.origin_version, history_last.origin_version);
        assert!(history_first.origin_version > previous.unwrap());
        assert!(!state
            .applying
            .contains_key(&(history_first.origin_version, [9; 32])));
        let mut abandoned = apply_chunk(1, 0, 2);
        state.rewrite_apply_chunk(&mut abandoned);
        let mut replacement = apply_chunk(1, 0, 1);
        state.rewrite_apply_chunk(&mut replacement);
        assert!(replacement.origin_version > abandoned.origin_version);
    }

    #[test]
    fn ctrl_and_following_v_wait_in_order_for_matching_success() {
        let mut hold = Hold::new(node(2), [7; 16], item(), 100, true).unwrap();
        let ctrl = key(0xe0, KeyState::Down);
        let v = key(0x19, KeyState::Down);
        assert!(activation(&ctrl));
        hold.events.push(ctrl);
        hold.events.push(v);
        assert!(!hold.ready(101, None));
        assert!(!hold.ready(102, Some((node(3), 7, [9; 32], true))));
        assert!(!hold.ready(103, Some((node(2), 7, [8; 32], true))));
        assert!(!hold.ready(104, Some((node(2), 7, [9; 32], false))));
        assert!(!hold.ready(104, Some((node(2), 6, [9; 32], true))));
        assert!(hold.ready(105, Some((node(2), 7, [9; 32], true))));
        assert_eq!(
            hold.into_events(Some((node(2), [7; 16]))).unwrap(),
            vec![ctrl, v]
        );
    }

    #[test]
    fn timeout_is_150ms_and_never_leaks_into_a_new_lease() {
        let hold = Hold::new(node(2), [7; 16], item(), 100, true).unwrap();
        assert!(!hold.ready(249, None));
        assert!(hold.ready(250, None));
        assert!(hold.into_events(Some((node(2), [8; 16]))).is_none());
        assert!(Hold::new(node(2), [7; 16], item(), 100, false).is_none());
    }

    #[test]
    fn motion_scroll_and_key_up_do_not_settle() {
        assert!(!activation(&key(0xe0, KeyState::Up)));
        assert!(!activation(&InputEvent::Scroll { h120: 0, v120: 120 }));
        assert!(!activation(&InputEvent::PointerRelative { dx: 1, dy: 2 }));
    }

    #[test]
    fn metadata_cannot_claim_another_origin() {
        let message = protocol::wire::ClipboardRoute {
            action: "carry".into(),
            origin_node_id: node(1).as_bytes().to_vec(),
            origin_version: 7,
            sha256: vec![9; 32],
            target_node_id: Vec::new(),
        };
        let carried = route_item(&message).unwrap();
        assert!(valid_carry(node(1), carried));
        assert!(!valid_carry(node(2), carried));
        let malformed = protocol::wire::ClipboardRoute {
            sha256: vec![9; 31],
            ..message
        };
        assert!(route_item(&malformed).is_none());
    }

    #[test]
    fn trusted_member_clear_requires_exact_current_tuple_not_origin_ownership() {
        assert!(valid_clear(Some(item()), item()));
        assert!(!valid_clear(None, item()));
        assert!(!valid_clear(
            Some(item()),
            Item {
                version: 8,
                ..item()
            }
        ));
        assert!(!valid_clear(
            Some(item()),
            Item {
                origin: node(2),
                ..item()
            }
        ));
        assert!(!valid_clear(
            Some(item()),
            Item {
                hash: [8; 32],
                ..item()
            }
        ));
    }

    #[test]
    fn explicit_current_hash_stops_follow_but_old_history_does_not() {
        let mut routing = Routing::default();
        routing.copy(item());
        assert!(!routing.addressed([8; 32]));
        assert_eq!(routing.carried(), Some(item()));
        assert!(routing.addressed([9; 32]));
        assert_eq!(routing.carried(), None);
        routing.enter(node(3), 0);
        assert_eq!(routing.settle(700, 700, true), None);
    }

    #[test]
    fn explicit_send_requires_both_trust_and_live_link() {
        assert!(!can_send(true, false));
        assert!(!can_send(false, true));
        assert!(!can_send(false, false));
        assert!(can_send(true, true));
    }

    #[test]
    fn accepted_addressed_failure_stays_stopped_but_offline_refusal_preserves_carry() {
        let mut routing = Routing::default();
        routing.copy(item());
        if can_send(true, false) {
            routing.addressed(item().hash);
        }
        assert_eq!(routing.carried(), Some(item()));
        assert!(can_send(true, true));
        assert!(routing.addressed(item().hash));
        routing.failed(Delivery {
            item: item(),
            target: node(2),
        });
        assert_eq!(routing.carried(), None);
        routing.enter(node(3), 100);
        assert_eq!(routing.settle(100, 700, true), None);
    }

    #[test]
    fn relayed_apply_is_bound_to_destination_and_wire_version() {
        let mut state = State::default();
        state.routing.copy(item());
        state.requested.insert((node(1), node(2)), item());
        state.hold = Hold::new(node(2), [7; 16], item(), 100, true);
        state.hold.as_mut().unwrap().ack_peer = node(1);
        assert!(!state.bind_pending(node(3), 8, [9; 32], node(2)));
        assert!(!state.bind_pending(node(1), 8, [8; 32], node(2)));
        assert!(!state.bind_pending(node(1), 8, [9; 32], node(3)));
        assert!(!state.bind_pending(node(1), 7, [9; 32], node(2)));
        assert!(state.bind_pending(node(1), 8, [9; 32], node(2)));
        let hold = state.hold.as_ref().unwrap();
        assert!(!hold.ready(101, Some((node(1), 7, [9; 32], true))));
        assert!(!hold.ready(101, Some((node(2), 8, [9; 32], true))));
        assert!(hold.ready(101, Some((node(1), 8, [9; 32], true))));
        assert!(!state.deliveries.contains_key(&(node(1), 7, [9; 32])));
        state
            .routing
            .failed(state.deliveries.remove(&(node(1), 8, [9; 32])).unwrap());
        state.routing.enter(node(3), 102);
        state.routing.enter(node(2), 103);
        assert!(state.routing.settle(103, 700, true).is_some());
    }

    #[test]
    fn previous_destination_pending_and_apply_never_release_current_hold() {
        let mut state = State::default();
        state.routing.copy(item());
        state.requested.insert((node(1), node(2)), item());
        state.requested.insert((node(1), node(3)), item());
        state.hold = Hold::new(node(3), [7; 16], item(), 100, true);
        state.hold.as_mut().unwrap().ack_peer = node(1);
        state.hold.as_mut().unwrap().version = None;
        assert!(state.bind_pending(node(1), 8, [9; 32], node(2)));
        assert!(!state
            .hold
            .as_ref()
            .unwrap()
            .ready(101, Some((node(1), 8, [9; 32], true))));
        assert!(state.bind_pending(node(1), 9, [9; 32], node(3)));
        assert!(!state
            .hold
            .as_ref()
            .unwrap()
            .ready(102, Some((node(1), 8, [9; 32], true))));
        assert!(state
            .hold
            .as_ref()
            .unwrap()
            .ready(103, Some((node(1), 9, [9; 32], true))));
        assert_eq!(
            state
                .deliveries
                .remove(&(node(1), 8, [9; 32]))
                .unwrap()
                .target,
            node(2)
        );
        assert_eq!(
            state
                .deliveries
                .remove(&(node(1), 9, [9; 32]))
                .unwrap()
                .target,
            node(3)
        );
    }

    #[test]
    fn pretransfer_failure_cannot_cancel_another_destination_or_copy() {
        let mut state = State::default();
        state.routing.copy(item());
        state.routing.enter(node(2), 100);
        let delivery = state.routing.settle(100, 700, true).unwrap();
        state.requested.insert((node(1), node(2)), delivery.item);
        assert!(!state.fail_request(item(), node(3)));
        assert!(!state.fail_request(
            Item {
                version: 6,
                ..item()
            },
            node(2)
        ));
        assert!(state.fail_request(item(), node(2)));
        assert_eq!(state.routing.settle(800, 700, true), None);
        state.routing.enter(node(3), 801);
        state.routing.enter(node(2), 802);
        assert!(state.routing.settle(802, 700, true).is_some());
    }

    #[test]
    fn automatic_owner_local_transit_does_not_settle_before_dwell() {
        let mut routing = Routing::default();
        routing.copy(item());
        routing.enter(node(2), 100);
        assert_eq!(routing.settle(799, 700, false), None);
        routing.enter(node(3), 799);
        assert_eq!(routing.settle(800, 700, true).unwrap().target, node(3));
        routing.enter(node(2), 900);
        assert_eq!(routing.settle(1599, 700, false), None);
        assert_eq!(routing.settle(1600, 700, false).unwrap().target, node(2));
    }

    #[test]
    fn local_observed_activation_or_explicit_selection_settles_without_hold() {
        for activation in [
            key(0xe0, KeyState::Down),
            InputEvent::Button {
                button: input_model::MouseButton::Left,
                state: KeyState::Down,
            },
        ] {
            let mut state = State::default();
            state.routing.copy(item());
            state.routing.enter(node(2), 100);
            assert!(state
                .routing
                .settle(100, 700, super::activation(&activation))
                .is_some());
            assert!(state.hold.is_none());
        }
        let mut state = State::default();
        state.routing.copy(item());
        state.routing.enter(node(2), 100);
        assert!(state.routing.settle(100, 700, true).is_some());
        assert!(state.hold.is_none());
    }

    #[test]
    fn missing_apply_ack_keeps_only_newest_delivery_per_destination() {
        let mut state = State::default();
        state.routing.copy(item());
        for wire_version in 8..100 {
            state.requested.insert((node(1), node(2)), item());
            assert!(state.bind_pending(node(1), wire_version, [9; 32], node(2)));
            assert!(state.requested.is_empty());
            assert_eq!(state.deliveries.len(), 1);
            assert!(state
                .deliveries
                .contains_key(&(node(1), wire_version, [9; 32])));
        }
    }
}
