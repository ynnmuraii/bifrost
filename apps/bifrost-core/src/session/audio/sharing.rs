//! Explicit SYSTEM-only intent. Configuration, pending intent, and native activity are distinct.
use super::*;

#[derive(Clone, Default)]
pub(super) struct Intent {
    pub(super) off_epoch: u64,
    pub(super) request: Option<Arc<PendingIntent>>,
}

pub(super) struct PendingIntent {
    request: Request,
    claimed: std::sync::atomic::AtomicBool,
}

pub(super) struct Start {
    peer: NodeId,
    deadline: u64,
    request_id: Option<[u8; 16]>,
}

// A coalesced slot, independent of the ordinary bounded GUI queue. OFF fences an
// already admitted ON before the actor next awaits persistence/device work.
pub(super) fn submit(
    intents: &watch::Sender<Intent>,
    request: Request,
) -> Result<(), (u64, mpsc::Sender<Outgoing>)> {
    if intents.is_closed() {
        return Err((request.request_id, request.out));
    }
    let off =
        matches!(&request.message, gui::gui_frame::Msg::SetSystemAudioSharing(set) if !set.enabled);
    intents.send_modify(|intent| {
        if off {
            intent.off_epoch = intent.off_epoch.saturating_add(1);
        }
        let pending = PendingIntent {
            request,
            claimed: std::sync::atomic::AtomicBool::new(false),
        };
        if let Some(previous) = intent.request.replace(Arc::new(pending)) {
            if !previous.claimed.swap(true, Ordering::AcqRel) {
                let _ = previous.request.out.try_send((
                    previous.request.request_id,
                    gui::gui_frame::Msg::OpResult(gui::OpResult {
                        ok: false,
                        error_code: ipc::audio::codes::AUDIO_UNAVAILABLE.into(),
                    }),
                ));
            }
        }
    });
    Ok(())
}

impl Actor {
    pub(super) fn system_enabled(&self) -> bool {
        self.config.system_enabled()
            && self.applied_system_off == self.system_intents.borrow().off_epoch
            && self.applied_system_off != u64::MAX
            && (self.config.system_sharing != Some(true) || self.system_requested)
    }

    pub(super) fn worker_needed(&self) -> bool {
        self.config.enabled
            && (self.system_enabled()
                || self.config.microphone.control_enabled
                || self.config.microphone.ptt_enabled)
    }

    pub(super) async fn sync_system_off(&mut self) -> Result<(), Code> {
        let epoch = self.system_intents.borrow().off_epoch;
        if epoch == self.applied_system_off {
            return self.system_off_result;
        }
        self.applied_system_off = epoch;
        self.system_requested = false;
        self.system_start = None;
        self.system_session = None;
        self.system_error = None;
        self.config.system_sharing = Some(false);
        self.stop_all(fsm::Reason::UserStop);
        self.fail_pending(ipc::audio::codes::AUDIO_UNAVAILABLE);
        let _ = self.configure();
        self.drain().await;
        if !self.worker_needed() {
            let _ = self.stop_worker().await;
        }
        // Persistence failure never restores transmission or an older full draft.
        self.system_off_result = self.save(self.config.clone()).await;
        self.system_error = self.system_off_result.err();
        self.dirty = true;
        self.system_off_result
    }

    pub(super) async fn system_intent(&mut self) {
        let intent = self.system_intents.borrow_and_update().clone();
        let Some(pending) = intent.request else {
            return;
        };
        if pending.claimed.swap(true, Ordering::AcqRel) {
            return;
        }
        let request = &pending.request;
        let off_result = self.sync_system_off().await;
        let gui::gui_frame::Msg::SetSystemAudioSharing(set) = &request.message else {
            return;
        };
        if !set.enabled {
            self.publish_caps();
            self.publish("system_off");
            reply(request, off_result).await;
            return;
        }
        let result = self.enable_system(set, intent.off_epoch).await;
        if let Err(code) = result {
            // A rejected edit does not mutate unrelated, already accepted intent.
            if !self.system_requested && intent.off_epoch == self.system_intents.borrow().off_epoch
            {
                self.system_error = Some(code);
                self.dirty = true;
            }
        }
        self.publish_caps();
        self.publish("system_intent");
        reply(request, result).await;
    }

    async fn enable_system(
        &mut self,
        set: &gui::SetSystemAudioSharing,
        epoch: u64,
    ) -> Result<(), Code> {
        if epoch != self.system_intents.borrow().off_epoch || epoch == u64::MAX {
            return Err(ipc::audio::codes::AUDIO_UNAVAILABLE);
        }
        if !wire::control_revision_matches(
            set.expected_revision,
            self.revision,
            self.control_revision,
        ) {
            return Err(ipc::audio::codes::AUDIO_REVISION_CONFLICT);
        }
        let role = match gui::AudioRole::try_from(set.role) {
            Ok(gui::AudioRole::Source) => config::AudioRole::Source,
            Ok(gui::AudioRole::Listener) => config::AudioRole::Listener,
            _ => return Err(ipc::audio::codes::AUDIO_ROLE_CONFLICT),
        };
        let listener = if role == config::AudioRole::Source {
            let id = <[u8; 32]>::try_from(set.listener_node_id.as_slice())
                .map_err(|_| ipc::audio::codes::AUDIO_ROLE_CONFLICT)?;
            let peer = NodeId::new(id);
            if peer == self.local || !self.links.borrow().trusted.contains(&peer) {
                return Err(ipc::audio::codes::AUDIO_CONSENT_REQUIRED);
            }
            if self
                .config
                .peers
                .iter()
                .any(|p| p.node_id.0 == id && p.system_send_override == Some(false))
            {
                return Err(ipc::audio::codes::AUDIO_CONSENT_REQUIRED);
            }
            Some(config::audio::HexId(id))
        } else {
            None
        };
        let pinned = self.config.effective_group_override();
        if pinned && role != self.config.role {
            return Err(ipc::audio::codes::AUDIO_ROLE_CONFLICT);
        }
        let mut next = self.config.clone();
        next.enabled = true;
        next.system_sharing = Some(true);
        next.role = role;
        if role == config::AudioRole::Source {
            if !pinned {
                next.group_id_override = Some(false);
            }
            if !pinned && (self.config.role != role || next.listener_node_id != listener) {
                next.group_id = None;
            }
            next.listener_node_id = listener;
        } else if self.config.role != role || next.group_id.is_none() {
            next.group_id = Some(config::audio::HexId(
                identity::random16().map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?,
            ));
        }
        next.validate()
            .map_err(|_| ipc::audio::codes::AUDIO_ROLE_CONFLICT)?;
        if self.listen.ip().is_unspecified() || self.listen.ip().is_multicast() {
            return Err(ipc::audio::codes::AUDIO_DEVICE_UNAVAILABLE);
        }
        self.save(next.clone()).await?;
        if epoch != self.system_intents.borrow().off_epoch {
            let _ = self.sync_system_off().await;
            return Err(ipc::audio::codes::AUDIO_UNAVAILABLE);
        }
        let changed_scope = self.config.role != next.role
            || self.config.listener_node_id != next.listener_node_id
            || self.config.group_id != next.group_id;
        self.system_start = None;
        self.system_session = None;
        if changed_scope {
            self.stop_all(fsm::Reason::UserStop);
        }
        self.config = next;
        self.system_requested = true;
        self.system_error = None;
        self.configuration_error = None;
        self.worker_failed = false;
        self.configure()?;
        if let Some(listener) = listener {
            self.system_start = Some(Start {
                peer: NodeId::new(listener.0),
                deadline: self.now().saturating_add(REQUEST_TIMEOUT_MS),
                request_id: None,
            });
        }
        self.dirty = true;
        self.drain().await;
        if let Err(code) = self.ensure_worker().await {
            self.system_failed(code);
        }
        let _ = self.sync_system_off().await;
        Ok(())
    }

    pub(super) fn system_failed(&mut self, code: Code) {
        self.system_start = None;
        self.system_session = None;
        self.system_error = Some(code);
        if self.config.role == config::AudioRole::Source {
            self.system_requested = false;
            self.stop_all(fsm::Reason::UserStop);
            let _ = self.configure();
        }
        self.dirty = true;
    }

    pub(super) fn system_start_result(
        &mut self,
        id: [u8; 16],
        result: Result<[u8; 16], fsm::Reason>,
    ) {
        if self
            .system_start
            .as_ref()
            .is_some_and(|start| start.request_id == Some(id))
        {
            self.system_start = None;
            match result {
                Ok(session) => self.system_session = Some(session),
                Err(reason) => self.system_failed(reason_code(reason)),
            }
        }
    }

    pub(super) async fn service_system_start(&mut self) {
        let _ = self.sync_system_off().await;
        if let (Some(session), Some(peer)) = (self.system_session, self.config.listener_node_id) {
            let view = self.model.session(peer.0);
            if view.is_none()
                || view.is_some_and(|view| {
                    view.session == session
                        && matches!(view.phase, fsm::Phase::Stopped | fsm::Phase::Failed)
                })
            {
                let reason = view.and_then(|view| view.reason);
                if reason == Some(fsm::Reason::UserStop) {
                    self.system_requested = false;
                    self.system_session = None;
                    self.system_error = None;
                    let _ = self.configure();
                    self.dirty = true;
                } else {
                    self.system_failed(
                        reason.map_or(ipc::audio::codes::AUDIO_UNAVAILABLE, reason_code),
                    );
                }
            }
        }
        let Some(start) = self.system_start.as_ref() else {
            return;
        };
        if !self.system_enabled() {
            self.system_start = None;
            return;
        }
        let peer = start.peer;
        if self.now() >= start.deadline {
            self.system_failed(ipc::audio::codes::AUDIO_UNAVAILABLE);
            return;
        }
        if self.ready.as_ref().is_some_and(|ready| ready.probed)
            && !available(self.ready.as_ref(), worker::Capability::SystemCapture)
        {
            self.system_failed(ipc::audio::codes::AUDIO_DEVICE_UNAVAILABLE);
            return;
        }
        if start.request_id.is_some() {
            return;
        }
        // Disabled audio intentionally does not advertise control. This is pending
        // discovery, not a remote consent refusal; no StartRequest has been sent.
        if self
            .peers
            .get(&peer)
            .is_none_or(|peer| !peer.support.control || peer.hello.is_none())
        {
            return;
        }
        if let Some(code) = self.start_refusal(peer) {
            self.system_failed(code);
            return;
        }
        if !self.start_ready(peer) {
            return;
        }
        if !self.allowed(peer) || !self.peers.get(&peer).is_some_and(|p| p.trusted) {
            self.system_failed(ipc::audio::codes::AUDIO_CONSENT_REQUIRED);
            return;
        }
        if self
            .records
            .get(&peer)
            .is_some_and(|r| matches!(r.view.phase, fsm::Phase::Active))
        {
            self.system_start = None;
            self.system_session = self.records.get(&peer).map(|record| record.view.session);
            return;
        }
        if !self.start_budgets.available(peer, self.now()) {
            self.system_failed(ipc::audio::codes::AUDIO_START_RATE_LIMITED);
            return;
        }
        let Some(parent) = self.peers.get(&peer).map(|p| p.parent) else {
            return;
        };
        let Ok(request_id) = identity::random16() else {
            self.system_failed(ipc::audio::codes::AUDIO_UNAVAILABLE);
            return;
        };
        let profile = match self.config.profile {
            config::AudioProfile::Wifi => Profile::Wifi,
            _ => Profile::Ethernet,
        };
        if let Some(start) = self.system_start.as_mut() {
            start.request_id = Some(request_id);
        }
        if let Err(code) = self.feed(fsm::Input::RequestStart {
            parent,
            request_id,
            kind: keys::StreamKind::System,
            profile,
        }) {
            self.system_failed(code);
        }
        self.drain().await;
    }
}
