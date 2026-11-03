//! Bounded pinned-parent rekey handoff; native streams and session identity never change.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Installing,
    AwaitReady,
    AwaitCommit,
    Activating,
}
#[derive(Clone, Copy)]
struct Pending {
    epoch: u32,
    nonce: [u8; 32],
    phase: Phase,
    deadline: u64,
}
pub(super) struct State {
    epoch: u32,
    pending: Option<Pending>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            epoch: 1,
            pending: None,
        }
    }
}
impl State {
    fn can_prepare(&self, epoch: u32) -> bool {
        self.pending.is_none() && self.epoch.checked_add(1) == Some(epoch)
    }
}
// Fixed, domain-separated context fits the existing exporter's 64..=256-byte budget.
fn key_context(original: &[u8; keys::CONTEXT_LEN], epoch: u32, nonce: &[u8; 32]) -> [u8; 90] {
    let mut context = [0; 90];
    context[..22].copy_from_slice(b"bifrost-audio-rekey-v1");
    context[22..54].copy_from_slice(ring::digest::digest(&ring::digest::SHA256, original).as_ref());
    context[54..58].copy_from_slice(&epoch.to_be_bytes());
    context[58..].copy_from_slice(nonce);
    context
}
impl Actor {
    fn rotation_view(&self, peer: NodeId) -> Option<fsm::SessionView> {
        let record = self.records.get(&peer)?;
        let view = record.view;
        (self.system_enabled()
            && self.allowed(peer)
            && self.current(view.parent)
            && matches!(view.phase, fsm::Phase::Active | fsm::Phase::Rebuffering)
            && self
                .peers
                .get(&peer)
                .is_some_and(|p| p.parent == view.parent && p.rekey_support)
            && self
                .links
                .borrow()
                .peers
                .get(&peer)
                .is_some_and(|p| p.parent == view.parent && p.rekey_support)
            && self.worker.as_ref().is_some_and(|w| {
                w.alive
                    && w.model_ready
                    && generation(w.generation, self.worker_epoch) == view.worker_generation
            }))
        .then_some(view)
    }
    fn rotation_failed(&mut self, peer: NodeId) {
        if let Some(record) = self.records.get_mut(&peer) {
            record.rotation.pending = None;
            let view = record.view;
            let _ = self.feed(fsm::Input::ChildFailed {
                view,
                reason: fsm::Reason::KeyExpired,
            });
        }
    }
    async fn rotation_send(
        &self,
        peer: NodeId,
        epoch: u32,
        stage: u32,
        nonce: Option<[u8; 32]>,
    ) -> bool {
        let Some(view) = self.rotation_view(peer) else {
            return false;
        };
        self.manager
            .send_audio_control(
                &peer,
                net::AudioControl {
                    control_version: 1,
                    body: Some(net::audio_control::Body::Rekey(net::AudioRekey {
                        session_id: view.session.to_vec(),
                        epoch,
                        stage,
                        nonce: nonce.map_or_else(Vec::new, |value| value.to_vec()),
                    })),
                },
            )
            .await
            .is_ok()
    }
    async fn rotation_prepare(&mut self, peer: NodeId, epoch: u32, fresh_nonce: [u8; 32]) {
        let Some(view) = self.rotation_view(peer) else {
            return;
        };
        let record = &self.records[&peer];
        if !record.rotation.can_prepare(epoch) {
            return;
        }
        let agreement = record.agreement;
        let context = key_context(&agreement.context().encode(), epoch, &fresh_nonce);

        let material = self.derive_context(view.parent, &context).await;
        if self.rotation_view(peer) != Some(view) {
            return;
        }
        let Some(material) = material else {
            self.rotation_failed(peer);
            return;
        };
        let (forward, reverse) = material.into_wire();
        let deadline = self.now().saturating_add(10_000);
        self.records
            .get_mut(&peer)
            .expect("live record")
            .rotation
            .pending = Some(Pending {
            epoch,
            nonce: fresh_nonce,
            phase: Phase::Installing,
            deadline,
        });
        let result = self
            .worker
            .as_ref()
            .expect("live worker")
            .handle
            .rotate_session(worker::RotateSession {
                session_id: view.session.to_vec(),
                epoch,
                activate: false,
                forward: Some(forward),
                reverse: Some(reverse),
            })
            .await;
        if result.is_err() {
            self.rotation_failed(peer);
        }
    }
    async fn rotation_begin(&mut self, peer: NodeId) {
        if self.rotation_view(peer).is_none() {
            return;
        }
        let state = &self.records[&peer].rotation;
        if state.pending.is_some() {
            return;
        }
        let Some(epoch) = state.epoch.checked_add(1) else {
            self.rotation_failed(peer);
            return;
        };
        let Some(nonce) = nonce() else {
            self.rotation_failed(peer);
            return;
        };
        self.rotation_prepare(peer, epoch, nonce).await;
    }
    async fn rotation_activate(&mut self, peer: NodeId, epoch: u32) {
        let Some(view) = self.rotation_view(peer) else {
            return;
        };
        self.records
            .get_mut(&peer)
            .expect("live record")
            .rotation
            .pending
            .as_mut()
            .expect("pending rotation")
            .phase = Phase::Activating;
        if self
            .worker
            .as_ref()
            .expect("live worker")
            .handle
            .rotate_session(worker::RotateSession {
                session_id: view.session.to_vec(),
                epoch,
                activate: true,
                forward: None,
                reverse: None,
            })
            .await
            .is_err()
        {
            self.rotation_failed(peer);
        }
    }
    pub(super) async fn rotation_on_network(
        &mut self,
        parent: fsm::Parent,
        message: net::AudioRekey,
    ) {
        let peer = NodeId::new(parent.peer);
        let Some(view) = self
            .rotation_view(peer)
            .filter(|view| view.parent == parent && view.session.as_slice() == message.session_id)
        else {
            return;
        };
        let _ = view;
        let listener = self.role() == model::AudioRole::Listener;
        let state = &self.records[&peer].rotation;
        let expected = state.epoch.checked_add(1);
        if expected != Some(message.epoch) {
            return;
        }
        let pending = state.pending;
        match (message.stage, listener, pending) {
            (4, true, None) => self.rotation_begin(peer).await,
            (1, false, None) => {
                let Ok(nonce) = message.nonce.as_slice().try_into() else {
                    return;
                };
                self.rotation_prepare(peer, message.epoch, nonce).await;
            }
            (2, true, Some(pending)) if pending.phase == Phase::AwaitReady => {
                self.rotation_activate(peer, message.epoch).await
            }
            (3, false, Some(pending)) if pending.phase == Phase::AwaitCommit => {
                self.rotation_activate(peer, message.epoch).await
            }
            _ => {}
        }
    }
    pub(super) async fn rotation_on_worker(
        &mut self,
        observed_generation: u64,
        event: worker::RotationEvent,
    ) {
        let Some(peer) = self
            .records
            .iter()
            .find(|(_, record)| record.view.session.as_slice() == event.session_id)
            .map(|(peer, _)| *peer)
        else {
            return;
        };
        let Some(view) = self.rotation_view(peer) else {
            return;
        };
        if self
            .worker
            .as_ref()
            .is_none_or(|w| w.generation != observed_generation)
            || generation(observed_generation, self.worker_epoch) != view.worker_generation
        {
            return;
        }
        let listener = self.role() == model::AudioRole::Listener;
        if event.kind == 1 {
            let state = &self.records[&peer].rotation;
            if event.epoch != state.epoch || state.pending.is_some() {
                return;
            }
            if listener {
                self.rotation_begin(peer).await;
            } else if let Some(next) = event.epoch.checked_add(1) {
                if !self.rotation_send(peer, next, 4, None).await {
                    self.rotation_failed(peer);
                }
            } else {
                self.rotation_failed(peer);
            }
            return;
        }
        let Some(pending) = self.records[&peer]
            .rotation
            .pending
            .filter(|p| p.epoch == event.epoch && self.now() < p.deadline)
        else {
            return;
        };
        match (event.kind, pending.phase) {
            (2, Phase::Installing) => {
                self.records
                    .get_mut(&peer)
                    .expect("live record")
                    .rotation
                    .pending
                    .as_mut()
                    .expect("pending")
                    .phase = if listener {
                    Phase::AwaitReady
                } else {
                    Phase::AwaitCommit
                };
                let sent = self
                    .rotation_send(
                        peer,
                        pending.epoch,
                        if listener { 1 } else { 2 },
                        listener.then_some(pending.nonce),
                    )
                    .await;
                if !sent {
                    self.rotation_failed(peer);
                }
            }
            (3, Phase::Activating) => {
                if listener && !self.rotation_send(peer, pending.epoch, 3, None).await {
                    self.rotation_failed(peer);
                    return;
                }
                let state = &mut self.records.get_mut(&peer).expect("live record").rotation;
                state.epoch = pending.epoch;
                state.pending = None;
                tracing::info!(epoch = pending.epoch, "audio media keys rotated in place");
            }
            _ => {}
        }
    }
    pub(super) fn rotation_tick(&mut self) {
        let now = self.now();
        let mut expired = [None; model::MAX_AUDIO_SESSIONS];
        for (slot, (peer, _)) in
            expired
                .iter_mut()
                .zip(self.records.iter().filter(|(_, record)| {
                    record
                        .rotation
                        .pending
                        .is_some_and(|pending| now >= pending.deadline)
                }))
        {
            *slot = Some(*peer);
        }
        for peer in expired.into_iter().flatten() {
            self.rotation_failed(peer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_and_activated_epochs_reject_replay_out_of_order_and_conflicting_material() {
        let mut state = State::default();
        assert!(state.can_prepare(2));
        assert!(!state.can_prepare(1));
        assert!(!state.can_prepare(3));
        state.pending = Some(Pending {
            epoch: 2,
            nonce: [8; 32],
            phase: Phase::AwaitCommit,
            deadline: 10_000,
        });
        // Admission rejects any replacement, including another nonce at the same pending epoch.
        assert!(!state.can_prepare(2));
        assert!(!state.can_prepare(3));
        assert_eq!(state.pending.unwrap().nonce, [8; 32]);
        state.pending = None;
        state.epoch = 2;
        assert!(!state.can_prepare(2));
        assert!(!state.can_prepare(1));
        assert!(state.can_prepare(3));
        state.epoch = u32::MAX;
        assert!(!state.can_prepare(0));
        assert!(!state.can_prepare(1));
    }
    #[test]
    fn rekey_context_fits_exporter_and_binds_original_epoch_and_fresh_nonce() {
        let original = [7; keys::CONTEXT_LEN];
        let context = key_context(&original, 2, &[8; 32]);
        assert!((64..=256).contains(&context.len()));
        assert_ne!(context, key_context(&original, 3, &[8; 32]));
        assert_ne!(context, key_context(&original, 2, &[9; 32]));
        assert_ne!(context, key_context(&[6; keys::CONTEXT_LEN], 2, &[8; 32]));
    }
}
