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
    deadline: Option<u64>,
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
    pub(super) async fn reconcile_system(&mut self) {
        self.service_system_start().await;
        self.drain().await;
        if !self.worker_needed() && self.worker.is_some() {
            let _ = self.stop_worker().await;
            self.drain().await;
        }
        // Reconciliation may revoke intent after the ordinary event publication.
        // Advance its semantic barrier before another queued activation is selected.
        self.publish_caps();
        self.publish("system_reconciliation");
    }

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
            || self.native_stops_pending()
    }

    pub(super) async fn sync_system_off(&mut self) -> Result<(), Code> {
        let epoch = self.system_intents.borrow().off_epoch;
        if epoch == self.applied_system_off {
            return self.system_off_result;
        }
        self.applied_system_off = epoch;
        self.system_requested = false;
        self.source_sound_released = false;
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
        // Validation/CAS/save failures answer only this request. They must not
        // replace the asynchronous outcome of a newer accepted intent.
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
        if self.config.role == config::AudioRole::Source
            && (role != self.config.role || self.config.listener_node_id != listener)
        {
            // A deliberate SYSTEM scope change revokes only obsolete positive
            // send grants. Independent preferences and known denials survive.
            next.allow_send_to
                .retain(|peer| role == config::AudioRole::Source && Some(*peer) == listener);
            for peer in &mut next.peers {
                if peer.system_send_override == Some(true)
                    && (role != config::AudioRole::Source || Some(peer.node_id) != listener)
                {
                    peer.system_send_override = None;
                }
            }
        }
        if role == config::AudioRole::Source {
            if !pinned {
                next.group_id_override = Some(false);
            }
            if !pinned && (self.config.role != role || next.listener_node_id != listener) {
                next.group_id = None;
            }
        } else if self.config.role != role || next.group_id.is_none() {
            next.group_id = Some(config::audio::HexId(
                identity::random16().map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?,
            ));
        }
        next.listener_node_id = listener;
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
                deadline: None,
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
        // Offline/not-yet-enabled receivers are pending discovery. Local consent
        // and trust revocation are different: they cancel even without a Hello.
        if !self.allowed(peer) || !self.links.borrow().trusted.contains(&peer) {
            self.system_failed(ipc::audio::codes::AUDIO_CONSENT_REQUIRED);
            return;
        }
        if start
            .deadline
            .is_some_and(|deadline| self.now() >= deadline)
        {
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
        if self.peers.get(&peer).is_none_or(|peer| {
            !peer.support.control || self.model.remote_hello(peer.parent).is_none()
        }) {
            return;
        }
        // An automatic Source still awaits its selected Listener's authenticated
        // group; a cached Hello may describe the peer's previous role.
        if self.config.group_id.is_none() {
            return;
        }
        if let Some(code) = self.start_refusal(peer) {
            self.system_failed(code);
            return;
        }
        if !self.start_ready(peer) {
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
        let deadline = self.now().saturating_add(REQUEST_TIMEOUT_MS);
        if let Some(start) = self.system_start.as_mut() {
            start.request_id = Some(request_id);
            start.deadline = Some(deadline);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_off_retires_only_after_matching_native_stop() {
        controlled_system_off(false, NativeStart::Started).await;
    }

    #[tokio::test]
    async fn system_off_still_reports_real_worker_loss_and_rejects_stale_stops() {
        controlled_system_off(true, NativeStart::Started).await;
    }

    #[tokio::test]
    async fn system_off_retires_a_committed_session_without_a_native_start() {
        controlled_system_off(false, NativeStart::Fenced).await;
    }

    #[tokio::test]
    async fn system_off_waits_for_issued_start_without_a_started_ack() {
        controlled_system_off(false, NativeStart::Issued).await;
    }

    #[tokio::test]
    async fn system_off_issued_start_without_ack_still_fails_on_worker_loss() {
        controlled_system_off(true, NativeStart::Issued).await;
    }

    #[tokio::test]
    async fn system_off_actor_processes_trust_revoke_before_native_stop() {
        controlled_system_off(false, NativeStart::RevokePending).await;
    }

    #[tokio::test]
    async fn system_off_actor_processes_control_withdrawal_before_native_stop() {
        controlled_system_off(false, NativeStart::ControlPending).await;
    }

    #[tokio::test]
    async fn system_off_deadline_does_not_hide_worker_loss() {
        controlled_system_off(true, NativeStart::Deadline).await;
    }

    #[tokio::test]
    async fn system_off_deadline_late_native_stopped_preserves_positive_completion() {
        controlled_system_off(false, NativeStart::Deadline).await;
    }

    #[tokio::test]
    async fn system_off_with_mic_keeps_status_responsive_during_parent_updates() {
        const CHILD: &str = "BIFROST_MIC_PARENT_TEST_CHILD";
        const ROOT: &str = "BIFROST_MIC_PARENT_TEST_ROOT";
        if std::env::var_os(CHILD).is_some() {
            controlled_system_off(false, NativeStart::MicContention).await;
            return;
        }
        use tokio::io::AsyncReadExt;
        let root = std::env::temp_dir().join(format!(
            "bf-mic-parent-{}",
            u128::from_be_bytes(identity::random16().expect("owned fixture ID"))
        ));
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::audio::sharing::tests::system_off_with_mic_keeps_status_responsive_during_parent_updates",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(ROOT, &root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("owned Actor test child");
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let capture = tokio::spawn(async move {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err)).unwrap();
            (out, err)
        });
        let result = timeout(Duration::from_secs(20), child.wait()).await;
        if result.is_err() {
            child
                .kill()
                .await
                .expect("kill and reap only the owned stalled test child");
        }
        let (stdout, stderr) = capture.await.unwrap();
        if root.exists() {
            std::fs::remove_dir_all(root).unwrap();
        }
        assert!(
            matches!(result, Ok(Ok(status)) if status.success()),
            "owned Actor child failed or stalled: {result:?}\n{}\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        );
        print!("{}", String::from_utf8_lossy(&stdout));
    }

    enum NativeStart {
        Fenced,
        Issued,
        Started,
        RevokePending,
        ControlPending,
        MicContention,
        Deadline,
    }

    #[tokio::test]
    async fn malformed_optional_artwork_cannot_stop_an_established_audio_actor() {
        controlled_system_off(false, NativeStart::Started).await;
    }
    async fn controlled_system_off(worker_lost: bool, native: NativeStart) {
        let native_issued = !matches!(native, NativeStart::Fenced);
        let native_started = matches!(
            native,
            NativeStart::Started
                | NativeStart::RevokePending
                | NativeStart::ControlPending
                | NativeStart::MicContention
                | NativeStart::Deadline
        );
        let revoke_pending = matches!(native, NativeStart::RevokePending);
        let control_pending = matches!(native, NativeStart::ControlPending);
        let past_deadline = matches!(native, NativeStart::Deadline);
        let mic_contention = matches!(native, NativeStart::MicContention);
        let root = if mic_contention {
            std::path::PathBuf::from(std::env::var_os("BIFROST_MIC_PARENT_TEST_ROOT").unwrap())
        } else {
            std::env::temp_dir().join(format!(
                "bf-stop-{}",
                u128::from_be_bytes(identity::random16().expect("owned fixture ID"))
            ))
        };
        let paths = config::Paths::new(&root, &root, &root, Some(root.clone()));
        let identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(&root))
                .expect("owned identity")
                .0,
        );
        let local = identity.node_id();
        let peer = NodeId::new([7; 32]);
        let (manager, _links, _) = manager_for(identity, peer, None, Vec::new());
        let mut audio = config::AudioConfig {
            enabled: true,
            system_sharing: Some(true),
            role: config::AudioRole::Source,
            group_id: Some(config::audio::HexId([3; 16])),
            listener_node_id: Some(config::audio::HexId(*peer.as_bytes())),
            ..Default::default()
        };
        if mic_contention {
            audio.microphone.control_enabled = true;
            audio.microphone.muted = true;
        }
        let mut persisted = config::Config {
            audio: audio.clone(),
            ..Default::default()
        };
        persisted.network.listen_address = "127.0.0.1".into();
        persisted.validate().expect("valid owned configuration");
        persisted
            .save(paths.config_file())
            .expect("owned persisted configuration");
        let (events, mut consumer) = broadcast::channel(32);
        let (coordinator, mut actor) = Coordinator::prepare(
            audio,
            local,
            manager,
            paths,
            "127.0.0.1:0".parse().unwrap(),
            true,
            events,
        );
        actor.system_requested = true;
        actor.configure().unwrap();
        actor.effects.clear();
        let parent = fsm::Parent {
            peer: *peer.as_bytes(),
            connection_id: [4; 16],
            generation: [5; 16],
        };
        let support = fsm::Support {
            control: true,
            system_source: false,
            listener: true,
        };
        let hello = fsm::Hello {
            role: model::AudioRole::Listener,
            group: Some([3; 16]),
            listener: *peer.as_bytes(),
            available: true,
            allowed: true,
            consent_revision: 1,
        };
        actor.peers.insert(
            peer,
            Peer {
                parent,
                name: "receiver".into(),
                remote: "127.0.0.1:40002".parse().unwrap(),
                support,
                trusted: true,
                hello: Some(hello),
                focus_support: false,
                mic_support: mic_contention,
                media_support: false,
                media_seek_support: false,
                rekey_support: false,
                artwork_support: false,
                artwork_rejected: 0,
                focus_sent_revision: 0,
                focus_request_at: None,
                focus_reply_at: None,
                focus_reply: false,
            },
        );
        // Establish a session at the accepted model seam; only native IPC is controlled.
        let mut effects = Vec::new();
        for input in [
            fsm::Input::WorkerUp {
                generation: generation(1, 0),
                capture: true,
                render: false,
            },
            fsm::Input::PeerUp {
                parent,
                support,
                trusted: true,
                local_allowed: true,
                consent_revision: 1,
            },
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Hello(hello),
            },
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Offer(fsm::Offer {
                    request_id: None,
                    session: [6; 16],
                    listener_nonce: [8; 32],
                    parent_connection_id: parent.connection_id,
                    group: [3; 16],
                    listener: *peer.as_bytes(),
                    source: *local.as_bytes(),
                    format: format(Profile::Ethernet),
                    format_sha256: wire::format_sha256(format(Profile::Ethernet)),
                    listener_port: 40002,
                    consent_revision: 1,
                }),
            },
            fsm::Input::SourceNonce {
                parent,
                session: [6; 16],
                nonce: [9; 32],
            },
        ] {
            actor.model.handle(0, input, &mut effects).unwrap();
        }
        let agreement = effects
            .iter()
            .find_map(|effect| match effect {
                fsm::Effect::DeriveKeys { agreement, .. } => Some(*agreement),
                _ => None,
            })
            .expect("real model agreement");
        let view = actor.model.session(parent.peer).unwrap();
        for input in [
            fsm::Input::KeysReady {
                view,
                transcript_sha256: [10; 32],
            },
            fsm::Input::ChildPrepared(view),
            fsm::Input::Receive {
                parent,
                message: fsm::Control::Commit {
                    session: view.session,
                    transcript_sha256: [10; 32],
                },
            },
        ] {
            actor.model.handle(0, input, &mut effects).unwrap();
        }
        if native_issued {
            actor
                .model
                .handle(0, fsm::Input::ChildStartIssued(view), &mut effects)
                .unwrap();
        }
        if native_started {
            for input in [fsm::Input::ChildStarted(view), fsm::Input::PathReady(view)] {
                actor.model.handle(0, input, &mut effects).unwrap();
            }
        }
        let view = actor.model.session(parent.peer).unwrap();
        assert_eq!(
            view.phase,
            if native_started {
                fsm::Phase::Active
            } else {
                fsm::Phase::Committed
            }
        );
        actor.records.insert(
            peer,
            Record {
                view,
                agreement,
                role: model::AudioRole::Source,
                material: None,
                start_request_id: native_issued.then_some(1),
                health: Default::default(),
                health_state: model::StreamHealth::Ready,
                latency_estimate_ms: None,
                latency_stages: None,
                path_diagnostic: model::PathDiagnostic::None,
                remote_quality: None,
                capture_active: native_started.then_some(true),
                sending: native_started.then_some(true),
                applied_gain: None,
                rotation: rotation::State::default(),
            },
        );
        let (handle, mut stops, mut shutdown, native) = AudioHandle::controlled_stop(1);
        actor.worker = Some(Worker {
            handle,
            generation: 1,
            alive: true,
            model_ready: true,
            pid: 0,
            probe_ack: 0,
        });
        let (native_events, events) = mpsc::channel(8);
        actor.worker_events = Some(events);
        if native_started {
            coordinator.links.send_modify(|links| {
                links.peers = actor.peers.clone();
                links.trusted.insert(peer);
            });
            actor.admitted.insert(peer);
            let before = actor.model.session(parent.peer).unwrap();
            for (width, total_bytes) in [(193, 1024), (192, 49153)] {
                let message = net::AudioControl {
                    control_version: 1,
                    body: Some(net::audio_control::Body::Artwork(net::MediaArtworkChunk {
                        session_id: view.session.to_vec(),
                        token: 1,
                        revision: 1,
                        artwork_id: vec![1; 32],
                        total_bytes,
                        offset: 0,
                        data: vec![0; 1024],
                        width,
                        height: 1,
                    })),
                };
                let violation = protocol::validate::audio_control(&message).unwrap_err();
                assert_eq!(violation, protocol::validate::AudioViolation::Artwork);
                coordinator.rejected(peer, parent.connection_id, violation);
            }
            actor.sync_links();
            assert_eq!(actor.model.session(parent.peer).unwrap(), before);
            assert!(
                stops.try_recv().is_err(),
                "optional thumbnail sent native Stop"
            );
        }
        if revoke_pending || control_pending || mic_contention {
            let coordinator = Arc::new(coordinator);
            coordinator.links.send_modify(|links| {
                links.peers = actor.peers.clone();
                links.trusted.insert(peer);
            });
            actor.admitted.insert(peer);
            let runner = tokio::spawn(actor.run());
            timeout(Duration::from_secs(5), async {
                loop {
                    let (_, message) = consumer.recv().await.unwrap();
                    if matches!(message, gui::gui_frame::Msg::AudioStatus(status)
                        if status.sessions.iter().any(|session|
                            session.session_id == view.session
                                && session.state == gui::AudioSessionState::Running as i32))
                    {
                        break;
                    }
                }
            })
            .await
            .expect("actual actor publishes established session");
            let contention = mic_contention.then(|| {
                let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let active = Arc::clone(&running);
                let parent = Arc::clone(&coordinator);
                let (ready, started) = std::sync::mpsc::sync_channel(0);
                let writer = std::thread::spawn(move || {
                    let caps = [
                        net::Capability::AudioControlV1 as i32,
                        net::Capability::AudioListener as i32,
                        net::Capability::MicControlV1 as i32,
                    ];
                    parent.capabilities_changed(peer, &caps);
                    ready.send(()).unwrap();
                    let mut updates = 1usize;
                    while active.load(Ordering::Acquire) {
                        parent.capabilities_changed(peer, &caps);
                        updates += 1;
                        std::thread::yield_now();
                    }
                    updates
                });
                started
                    .recv()
                    .expect("actual Coordinator completed its first parent update");
                (running, writer)
            });
            let (out, mut replies) = mpsc::channel(4);
            coordinator
                .submit(Request {
                    request_id: 91,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::SetSystemAudioSharing(
                        gui::SetSystemAudioSharing {
                            enabled: false,
                            role: i32::MAX,
                            listener_node_id: Vec::new(),
                            expected_revision: 0,
                        },
                    ),
                    out: out.clone(),
                })
                .expect("real actor OFF admission");
            assert_eq!(stops.recv().await, Some(view.session));
            assert!(!*shutdown.borrow(), "native stop remains withheld");
            native_events
                .send(AudioSupervisorEvent::Health {
                    generation: 1,
                    health: worker::Health {
                        sessions: vec![worker::SessionHealth {
                            session_id: view.session.to_vec(),
                            capture_active: Some(false),
                            sending: Some(false),
                            ..Default::default()
                        }],
                    },
                })
                .await
                .unwrap();
            if mic_contention {
                native_events
                    .send(AudioSupervisorEvent::SessionEvent {
                        generation: 1,
                        event: worker::SessionEvent {
                            session_id: view.session.to_vec(),
                            kind: worker::EventKind::Stopped as i32,
                            ..Default::default()
                        },
                    })
                    .await
                    .unwrap();
            } else if revoke_pending {
                coordinator.trusted(&identity::TrustStore::new());
            } else {
                coordinator.capabilities_changed(peer, &[]);
            }
            coordinator
                .submit(Request {
                    request_id: 92,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                    out: out.clone(),
                })
                .expect("ordinary public status command during native stop");
            timeout(Duration::from_secs(5), async {
                loop {
                    let (_, message) = consumer.recv().await.unwrap();
                    if let gui::gui_frame::Msg::AudioStatus(status) = message {
                        if status.sessions.iter().any(|session| {
                            session.session_id == view.session
                                && session.state
                                    == if mic_contention {
                                        gui::AudioSessionState::Idle as i32
                                    } else {
                                        gui::AudioSessionState::Failed as i32
                                    }
                        }) {
                            assert_eq!(status.system_sharing_requested, Some(false));
                            break;
                        }
                    }
                }
                if mic_contention {
                    let mut off_replied = false;
                    let mut status_replied = false;
                    while !off_replied || !status_replied {
                        let (request_id, message) = replies.recv().await.unwrap();
                        match (request_id, message) {
                            (91, gui::gui_frame::Msg::OpResult(result)) if !off_replied => {
                                assert!(result.ok, "public OFF completes successfully: {result:?}");
                                off_replied = true;
                            }
                            (92, gui::gui_frame::Msg::AudioStatus(status)) if !status_replied => {
                                assert_eq!(status.system_sharing_requested, Some(false));
                                status_replied = true;
                            }
                            (request_id, _) => {
                                panic!("unexpected public reply for request {request_id}");
                            }
                        }
                    }
                } else {
                    loop {
                        let (request_id, message) = replies.recv().await.unwrap();
                        if request_id == 92 {
                            assert!(matches!(message, gui::gui_frame::Msg::AudioStatus(_)));
                            break;
                        }
                    }
                }
            })
            .await
            .expect("actual Actor processes native stop, parent changes, and public status");
            if let Some((running, writer)) = contention {
                assert!(
                    !*shutdown.borrow(),
                    "independent microphone policy retains the worker"
                );
                for request_id in 93..=155 {
                    coordinator
                        .submit(Request {
                            request_id,
                            connection_id: [11; 16],
                            message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                            out: out.clone(),
                        })
                        .expect("public status admission during parent updates");
                    let (id, message) = replies.recv().await.unwrap();
                    assert_eq!(id, request_id);
                    let gui::gui_frame::Msg::AudioStatus(status) = message else {
                        panic!("public audio status reply");
                    };
                    assert_eq!(status.system_sharing_requested, Some(false));
                    let mic = status.config.unwrap().microphone.unwrap();
                    assert!(mic.control_enabled && mic.muted);
                }
                running.store(false, Ordering::Release);
                let updates = writer.join().expect("owned Coordinator writer closes");
                println!("ACTOR_PARENT_PROGRESS status_replies=64 parent_updates={updates} native_stop=completed mic_policy=enabled");
            }
            coordinator.stopping.send_replace(true);
            runner.await.unwrap();
            native.await.unwrap();
            drop(coordinator);
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        let (out, mut replies) = mpsc::channel(4);
        coordinator
            .submit(Request {
                request_id: 91,
                connection_id: [11; 16],
                message: gui::gui_frame::Msg::SetSystemAudioSharing(gui::SetSystemAudioSharing {
                    enabled: false,
                    role: i32::MAX,
                    listener_node_id: Vec::new(),
                    expected_revision: 0,
                }),
                out,
            })
            .expect("real Coordinator OFF admission");
        let completion = async {
            if !native_issued {
                // An unrelated event is a deterministic receive-loop barrier: if
                // retirement waits here, the actor will fence that unknown start.
                native_events
                    .send(AudioSupervisorEvent::SessionEvent {
                        generation: 1,
                        event: worker::SessionEvent {
                            session_id: vec![12; 16],
                            kind: worker::EventKind::Started as i32,
                            ..Default::default()
                        },
                    })
                    .await
                    .unwrap();
                tokio::select! {
                    result = shutdown.changed() => result.unwrap(),
                    stop = stops.recv() => panic!(
                        "retirement waited for a nonexistent native session: {stop:?}"
                    ),
                }
                return;
            }
            assert_eq!(stops.recv().await, Some(view.session));
            assert!(
                !*shutdown.borrow(),
                "wire write is not native stop completion"
            );
            native_events
                .send(AudioSupervisorEvent::Health {
                    generation: 1,
                    health: worker::Health {
                        sessions: vec![worker::SessionHealth {
                            session_id: view.session.to_vec(),
                            capture_active: Some(false),
                            sending: Some(false),
                            ..Default::default()
                        }],
                    },
                })
                .await
                .unwrap();
            for (gen, session) in [(0, view.session), (1, [12; 16])] {
                native_events
                    .send(AudioSupervisorEvent::SessionEvent {
                        generation: gen,
                        event: worker::SessionEvent {
                            session_id: session.to_vec(),
                            kind: worker::EventKind::Stopped as i32,
                            ..Default::default()
                        },
                    })
                    .await
                    .unwrap();
            }
            let event = if worker_lost {
                AudioSupervisorEvent::Failed {
                    generation: 1,
                    session_id: None,
                    reason: AudioSupervisorError::Unavailable,
                }
            } else {
                AudioSupervisorEvent::SessionEvent {
                    generation: 1,
                    event: worker::SessionEvent {
                        session_id: view.session.to_vec(),
                        kind: worker::EventKind::Stopped as i32,
                        ..Default::default()
                    },
                }
            };
            native_events.send(event).await.unwrap();
        };
        let ((), ()) = tokio::join!(actor.system_intent(), completion);
        let (_, reply) = replies.recv().await.expect("actual OFF response");
        assert!(
            matches!(&reply, gui::gui_frame::Msg::OpResult(result) if result.ok),
            "actual OFF response: {reply:?}"
        );
        if native_issued {
            assert!(actor.native_stops_pending());
            assert_eq!(
                actor.model.session(parent.peer).unwrap().phase,
                fsm::Phase::Stopping
            );
        }
        if past_deadline {
            let stopped_at = actor.now();
            for advance in [250, 500, 750, 1000, 1250] {
                actor.epoch = Instant::now()
                    .checked_sub(Duration::from_millis(stopped_at + advance))
                    .expect("supplied stop-deadline clock");
                actor
                    .feed(fsm::Input::WorkerHeartbeat {
                        generation: view.worker_generation,
                    })
                    .unwrap();
                actor.feed(fsm::Input::Tick).unwrap();
            }
            assert_eq!(
                actor.model.session(parent.peer).unwrap().phase,
                fsm::Phase::Stopped
            );
            assert!(
                actor.native_stops_pending(),
                "logical expiry is not a native ACK"
            );
        }
        while actor.worker.is_some() {
            let event = actor.worker_events.as_mut().unwrap().recv().await.unwrap();
            let completion_fact = match &event {
                AudioSupervisorEvent::SessionEvent { generation, event } => {
                    *generation == 1
                        && event.session_id == view.session
                        && event.kind == worker::EventKind::Stopped as i32
                }
                AudioSupervisorEvent::Failed { generation: 1, .. } => true,
                _ => false,
            };
            actor.on_worker(event).await;
            actor.drain().await;
            actor.reconcile_system().await;
            if !completion_fact {
                assert!(
                    actor.worker.is_some(),
                    "Health/stale Stop cannot retire the worker"
                );
                assert!(actor.native_stops_pending());
                assert_eq!(
                    actor.model.session(parent.peer).unwrap().phase,
                    if past_deadline {
                        fsm::Phase::Stopped
                    } else {
                        fsm::Phase::Stopping
                    }
                );
            }
        }
        actor.drain().await;
        actor.publish("controlled_stop_completion");
        let mut status = None;
        while let Ok((_, gui::gui_frame::Msg::AudioStatus(value))) = consumer.try_recv() {
            status = Some(value);
        }
        let status = status.expect("consumer receives completed OFF");
        assert_eq!(status.system_sharing_requested, Some(false));
        let stopped = status
            .sessions
            .iter()
            .find(|session| session.session_id == view.session)
            .expect("consumer retains matching stop result");
        if worker_lost {
            assert_eq!(stopped.state, gui::AudioSessionState::Failed as i32);
            assert_eq!(stopped.capture_active, None);
            assert_eq!(stopped.sending, None);
            assert_eq!(
                actor.model.session(parent.peer).unwrap().reason,
                Some(fsm::Reason::WorkerLost)
            );
        } else {
            assert_eq!(stopped.state, gui::AudioSessionState::Idle as i32);
            let observation = native_issued.then_some(false);
            assert_eq!(stopped.capture_active, observation);
            assert_eq!(stopped.sending, observation);
        }
        assert!(actor.worker.is_none(), "completed generation is retired");
        native.await.unwrap();
        drop(actor);
        drop(coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn reconciliation_failure_is_published_before_queued_activation_cas() {
        let root = std::env::temp_dir().join(format!(
            "bifrost-sharing-cas-{}-{}",
            std::process::id(),
            u128::from_be_bytes(identity::random16().expect("owned fixture ID")),
        ));
        let paths = config::Paths::new(&root, &root, &root, Some(root.clone()));
        let identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(&root))
                .expect("owned identity")
                .0,
        );
        let local = identity.node_id();
        let peer = NodeId::new([7; 32]);
        let mut trust = identity::TrustStore::new();
        trust.trust(identity::TrustedPeer::paired(peer, "receiver", None, 0));
        let (manager, _link_events) = LinkManager::new(transport::ManagerConfig {
            identity,
            trust: trust.clone(),
            local_hello: net::Hello::default(),
            listen: "127.0.0.1:0".parse().expect("owned listen address"),
            peers: Vec::new(),
            max_members: 4,
            queue_capacity: 32,
            liveness: Default::default(),
            backoff: Default::default(),
            pairing: transport::PairingGate::closed(),
        })
        .expect("real manager");
        let audio = config::AudioConfig {
            enabled: true,
            system_sharing: Some(true),
            role: config::AudioRole::Source,
            group_id: Some(config::audio::HexId([3; 16])),
            listener_node_id: Some(config::audio::HexId(*peer.as_bytes())),
            ..Default::default()
        };
        let mut persisted = config::Config {
            audio: audio.clone(),
            ..Default::default()
        };
        persisted.network.listen_address = "127.0.0.1".into();
        persisted
            .validate()
            .expect("valid owned config before testing persistence");
        persisted
            .save(paths.config_file())
            .expect("real accepted consent persistence");
        let (events, mut consumer) = broadcast::channel(16);
        let (coordinator, mut actor) = Coordinator::prepare(
            audio,
            local,
            Arc::new(manager),
            paths.clone(),
            "127.0.0.1:0".parse().expect("owned address"),
            true,
            events,
        );
        coordinator.trusted(&trust);
        // Accepted pending intent with no native facts fabricated: worker is absent.
        actor.system_requested = true;
        actor.system_start = Some(Start {
            peer,
            deadline: None,
            request_id: None,
        });
        actor.configure().expect("pending model configuration");
        actor.drain().await;
        actor.epoch = actor
            .epoch
            .checked_sub(Duration::from_secs(11))
            .expect("clock anchor");
        Box::pin(actor.reconcile_system()).await;
        assert!(actor.system_requested, "no pre-Hello ten-second expiry");
        assert!(actor.system_error.is_none());
        let waiting = actor
            .system_start
            .as_ref()
            .expect("initial intent still waiting");
        assert!(
            waiting.deadline.is_none() && waiting.request_id.is_none(),
            "waiting cannot issue or retry a P1 request"
        );
        assert!(actor.worker.is_none());
        // Now exercise a genuine post-dispatch timeout phase, not initial discovery.
        actor.system_start = Some(Start {
            peer,
            deadline: Some(REQUEST_TIMEOUT_MS),
            request_id: Some([6; 16]),
        });
        actor.publish("pending_intent");
        let mut snapshot = None;
        while let Ok((_, gui::gui_frame::Msg::AudioStatus(status))) = consumer.try_recv() {
            snapshot = Some(status);
        }
        let snapshot = snapshot.expect("actual published pending state");
        assert_eq!(snapshot.system_sharing_requested, Some(true));
        assert!(snapshot.sessions.is_empty());
        let (out, mut replies) = mpsc::channel(4);
        coordinator
            .submit(Request {
                request_id: 41,
                connection_id: [8; 16],
                message: gui::gui_frame::Msg::SetSystemAudioSharing(gui::SetSystemAudioSharing {
                    enabled: true,
                    role: gui::AudioRole::Source as i32,
                    listener_node_id: peer.as_bytes().to_vec(),
                    expected_revision: snapshot.revision,
                }),
                out,
            })
            .expect("actual bounded intent admission");
        // Expiry reconciles the pending intent while the earlier GUI action is queued.
        Box::pin(actor.reconcile_system()).await;
        Box::pin(actor.system_intent()).await;
        let (request_id, message) = replies.recv().await.expect("actual correlated reply");
        assert_eq!(request_id, 41);
        assert!(
            matches!(&message, gui::gui_frame::Msg::OpResult(result)
            if !result.ok && result.error_code == ipc::audio::codes::AUDIO_REVISION_CONFLICT),
            "pre-failure queued activation must be refused: {message:?}"
        );
        assert!(!actor.system_requested);
        assert!(actor.worker.is_none());
        let mut stopped = None;
        while let Ok((_, gui::gui_frame::Msg::AudioStatus(status))) = consumer.try_recv() {
            stopped = Some(status);
        }
        let stopped = stopped.expect("consumer receives reconciliation before another command");
        assert_eq!(stopped.system_sharing_requested, Some(false));
        assert!(stopped.revision > snapshot.revision);
        assert_eq!(
            config::Config::load(paths.config_file())
                .expect("real config")
                .audio
                .system_sharing,
            Some(true),
            "runtime failure does not invent persisted OFF"
        );
        drop(actor);
        drop(coordinator);
        std::fs::remove_dir_all(root).expect("remove owned fixture");
    }

    async fn next_audio(
        events: &mut mpsc::Receiver<transport::LinkEvent>,
    ) -> (NodeId, [u8; 16], net::AudioControl) {
        timeout(Duration::from_secs(5), async {
            loop {
                match events.recv().await.expect("live authenticated link") {
                    transport::LinkEvent::Message {
                        peer,
                        message:
                            transport::convert::Inbound::Audio {
                                message,
                                connection_id,
                            },
                    } => return (peer, connection_id, message),
                    transport::LinkEvent::Message {
                        message: transport::convert::Inbound::AudioRejected { .. },
                        ..
                    } => panic!("fixture must deliver a valid authenticated control message"),
                    _ => {}
                }
            }
        })
        .await
        .expect("bounded authenticated AudioControl delivery")
    }

    async fn next_up(
        events: &mut mpsc::Receiver<transport::LinkEvent>,
    ) -> (
        NodeId,
        [u8; 16],
        [u8; 16],
        transport::convert::PeerInfo,
        SocketAddr,
    ) {
        timeout(Duration::from_secs(5), async {
            loop {
                if let transport::LinkEvent::Up {
                    peer,
                    generation,
                    connection_id,
                    hello,
                    remote,
                    ..
                } = events.recv().await.expect("live manager")
                {
                    return (peer, generation, connection_id, hello, remote);
                }
            }
        })
        .await
        .expect("real pinned TLS and Hello establishment")
    }

    fn manager_for(
        identity: Arc<identity::NodeIdentity>,
        peer: NodeId,
        address: Option<SocketAddr>,
        capabilities: Vec<i32>,
    ) -> (
        Arc<LinkManager>,
        mpsc::Receiver<transport::LinkEvent>,
        identity::TrustStore,
    ) {
        let mut trust = identity::TrustStore::new();
        trust.trust(identity::TrustedPeer::paired(
            peer,
            "fixture",
            address.map(|address| address.to_string()),
            0,
        ));
        let local = identity.node_id();
        let (manager, events) = LinkManager::new(transport::ManagerConfig {
            identity,
            trust: trust.clone(),
            local_hello: net::Hello {
                node_id: local.as_bytes().to_vec(),
                display_name: "fixture".into(),
                session_kind: net::SessionKind::X11 as i32,
                capabilities,
                ..Default::default()
            },
            listen: "127.0.0.1:0".parse().expect("owned listener"),
            peers: address.into_iter().collect(),
            max_members: 2,
            queue_capacity: 32,
            liveness: Default::default(),
            backoff: Default::default(),
            pairing: transport::PairingGate::closed(),
        })
        .expect("real TLS manager");
        (Arc::new(manager), events, trust)
    }

    #[tokio::test]
    async fn receiver_applies_published_source_support_before_start_request() {
        source_capability_ordering(CapabilityOrder::ReceiverWatch).await;
    }

    #[tokio::test]
    async fn source_waits_for_current_publication_across_capability_aba() {
        source_capability_ordering(CapabilityOrder::OldPositiveReceipt).await;
    }

    #[tokio::test]
    async fn source_old_withdrawal_completion_cannot_release_new_source_intent() {
        source_capability_ordering(CapabilityOrder::OldWithdrawalReceipt).await;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CapabilityOrder {
        ReceiverWatch,
        OldPositiveReceipt,
        OldWithdrawalReceipt,
    }

    async fn source_capability_ordering(order: CapabilityOrder) {
        let publication_aba = order != CapabilityOrder::ReceiverWatch;
        let root = std::env::temp_dir().join(format!(
            "bifrost-receiver-order-{}",
            u128::from_be_bytes(identity::random16().unwrap())
        ));
        let listener_root = root.join("listener");
        let source_root = root.join("source");
        let listener_identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                &listener_root,
            ))
            .unwrap()
            .0,
        );
        let source_identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                &source_root,
            ))
            .unwrap()
            .0,
        );
        let listener_id = listener_identity.node_id();
        let source_id = source_identity.node_id();
        let (listener_manager, mut listener_events, listener_trust) = manager_for(
            listener_identity,
            source_id,
            None,
            vec![
                net::Capability::AudioControlV1 as i32,
                net::Capability::AudioListener as i32,
            ],
        );
        let listener_task = tokio::spawn({
            let manager = Arc::clone(&listener_manager);
            async move { manager.run().await.unwrap() }
        });
        let listener_address = listener_manager
            .rebind_listener("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let (source_manager, mut source_events, source_trust) = manager_for(
            source_identity,
            listener_id,
            Some(listener_address),
            vec![net::Capability::AudioControlV1 as i32],
        );
        let source_task = tokio::spawn({
            let manager = Arc::clone(&source_manager);
            async move { manager.run().await.unwrap() }
        });
        let (peer_source, listener_generation, connection, source_hello, source_remote) =
            next_up(&mut listener_events).await;
        let (peer_listener, source_generation, source_connection, listener_hello, listener_remote) =
            next_up(&mut source_events).await;
        assert_eq!(peer_source, source_id);
        assert_eq!(peer_listener, listener_id);
        assert_eq!(connection, source_connection);
        assert!(!source_hello.system_audio_source);
        let [(listener_coordinator, mut listener_actor, listener_native), (mut source_coordinator, mut source_actor, source_native)] =
            std::array::from_fn::<_, 2, _>(|index| {
                let (directory, local, peer, generation, hello, remote, manager, trust) =
                    if index == 0 {
                        (
                            &listener_root,
                            listener_id,
                            source_id,
                            listener_generation,
                            &source_hello,
                            source_remote,
                            &listener_manager,
                            &listener_trust,
                        )
                    } else {
                        (
                            &source_root,
                            source_id,
                            listener_id,
                            source_generation,
                            &listener_hello,
                            listener_remote,
                            &source_manager,
                            &source_trust,
                        )
                    };
                let paths = config::Paths::new(
                    directory,
                    directory,
                    directory,
                    Some(directory.to_path_buf()),
                );
                let mut stored = config::Config {
                    audio: config::AudioConfig {
                        enabled: true,
                        system_sharing: Some(true),
                        role: config::AudioRole::Listener,
                        group_id: Some(config::audio::HexId(if index == 0 {
                            [3; 16]
                        } else {
                            [4; 16]
                        })),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                stored.network.listen_address = "127.0.0.1".into();
                stored.save(paths.config_file()).unwrap();
                let (events, _) = broadcast::channel(32);
                let (coordinator, mut actor) = Coordinator::prepare(
                    stored.audio,
                    local,
                    Arc::clone(manager),
                    paths,
                    if index == 0 {
                        listener_address
                    } else {
                        source_remote
                    },
                    true,
                    events,
                );
                coordinator.trusted(trust);
                coordinator.link_up(peer, generation, connection, hello, remote, true);
                actor.configure().unwrap();
                actor.sync_links();
                let (handle, _stops, _shutdown, native) = AudioHandle::controlled_stop(1);
                actor.worker = Some(Worker {
                    handle,
                    generation: 1,
                    alive: true,
                    model_ready: false,
                    pid: 0,
                    probe_ack: 0,
                });
                actor.probed(
                    1,
                    1,
                    worker::Ready {
                        backend: worker::BackendKind::Dry as i32,
                        probed: true,
                        capabilities: [
                            worker::Capability::SystemCapture,
                            worker::Capability::Render,
                        ]
                        .into_iter()
                        .map(|capability| worker::CapabilityAvailability {
                            capability: capability as i32,
                            availability: worker::Availability::Available as i32,
                            ..Default::default()
                        })
                        .collect(),
                    },
                );
                (coordinator, actor, native)
            });
        let mut source_header = net::Hello {
            node_id: source_id.as_bytes().to_vec(),
            display_name: "fixture".into(),
            session_kind: net::SessionKind::X11 as i32,
            ..Default::default()
        };
        let withdrawal = if publication_aba {
            source_coordinator
                .publish_capabilities(&source_manager, &mut source_header)
                .await;
            let ready = source_actor.ready.clone().unwrap();
            let mut unavailable = ready.clone();
            let capture = unavailable
                .capabilities
                .iter_mut()
                .find(|capability| {
                    capability.capability == worker::Capability::SystemCapture as i32
                })
                .unwrap();
            capture.availability = worker::Availability::Unavailable as i32;
            capture.reason = worker::UnavailableReason::DeviceGone as i32;
            source_actor.probed(1, 2, unavailable);
            let revision = prepare_capabilities(&mut source_coordinator.caps, &mut source_header);
            source_actor.probed(1, 3, ready);
            // The main publisher captured an old withdrawal before the newer
            // qualification; enqueue it but pause before completing its receipt.
            source_manager
                .update_local_hello(source_header.clone())
                .await;
            Some(revision)
        } else {
            None
        };
        listener_actor.drain().await;
        source_actor.drain().await;
        let mut source_status = source_actor.events.subscribe();
        let source_runner = tokio::spawn(source_actor.run());
        let (out, mut replies) = mpsc::channel(4);
        source_coordinator
            .submit(Request {
                request_id: 1,
                connection_id: [11; 16],
                message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                out: out.clone(),
            })
            .unwrap();
        let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, 1);
        let gui::gui_frame::Msg::AudioStatus(initial) = reply else {
            panic!("public initial Source status")
        };
        source_coordinator
            .submit(Request {
                request_id: 2,
                connection_id: [11; 16],
                message: gui::gui_frame::Msg::SetSystemAudioSharing(gui::SetSystemAudioSharing {
                    enabled: true,
                    role: gui::AudioRole::Source as i32,
                    listener_node_id: listener_id.as_bytes().to_vec(),
                    expected_revision: initial.revision,
                }),
                out: out.clone(),
            })
            .unwrap();
        let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, 2);
        assert!(matches!(reply, gui::gui_frame::Msg::OpResult(result) if result.ok));
        // This public barrier follows ON's normal publish/reconcile turn. The
        // original authenticated Listener Hello is still queued, not repaired.
        source_coordinator
            .submit(Request {
                request_id: 3,
                connection_id: [11; 16],
                message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                out: out.clone(),
            })
            .unwrap();
        let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(id, 3);
        assert!(
            matches!(reply, gui::gui_frame::Msg::AudioStatus(status) if status.system_sharing_requested == Some(true) && status.system_sharing_error.is_empty())
        );
        let capabilities = source_coordinator.capabilities();
        assert!(capabilities.contains(&(net::Capability::SystemAudioSource as i32)));
        if !publication_aba {
            source_coordinator
                .publish_capabilities(&source_manager, &mut source_header)
                .await;
            timeout(Duration::from_secs(5), async {
                loop {
                    match listener_events.recv().await.unwrap() {
                        transport::LinkEvent::Message {
                            peer,
                            message: transport::convert::Inbound::CapabilitiesChanged(capabilities),
                        } => {
                            assert_eq!(peer, source_id);
                            assert!(
                                capabilities.contains(&(net::Capability::SystemAudioSource as i32))
                            );
                            listener_coordinator.capabilities_changed(peer, &capabilities);
                            break;
                        }
                        transport::LinkEvent::Message {
                            peer,
                            message:
                                transport::convert::Inbound::Audio {
                                    message,
                                    connection_id,
                                },
                        } => {
                            listener_coordinator.receive(peer, connection_id, message);
                            let command = listener_actor.commands.recv().await.unwrap();
                            listener_actor.on_network(command.parent, command.message.unwrap());
                            listener_actor.drain().await;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("positive Source capability arrives over actual TLS first");
            {
                let links = listener_actor.links.borrow();
                let peer = links.peers.get(&source_id).unwrap();
                assert_eq!(peer.parent.connection_id, connection);
                assert_eq!(peer.parent.generation, listener_generation);
                assert!(peer.trusted && peer.support.control && peer.support.system_source);
            }
            assert!(listener_actor.links.has_changed().unwrap());
        }
        if let (CapabilityOrder::OldWithdrawalReceipt, Some(revision)) = (order, withdrawal) {
            capabilities_published(&source_coordinator.caps, revision);
        }
        // Release the same original readiness in the selected legal ordering.
        timeout(Duration::from_secs(5), async {
            loop {
                let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
                let ready = matches!(&message.body, Some(net::audio_control::Body::Hello(hello)) if hello.listener_available);
                source_coordinator.receive(peer, envelope_connection, message);
                if ready {
                    break;
                }
            }
        }).await.expect("original Listener readiness arrives within one deadline");
        if let Some(withdrawal) = withdrawal {
            timeout(Duration::from_secs(5), async {
                loop {
                    let (_, message) = source_status.recv().await.unwrap();
                    if let gui::gui_frame::Msg::AudioStatus(status) = message {
                        if status.role == gui::AudioRole::Source as i32
                            && status
                                .config
                                .as_ref()
                                .is_some_and(|config| config.group_id == [3; 16])
                        {
                            assert_eq!(status.system_sharing_requested, Some(true));
                            assert!(status.system_sharing_error.is_empty());
                            break;
                        }
                    }
                }
            })
            .await
            .expect("public Source scope resolves while current publication is pending");
            source_coordinator
                .submit(Request {
                    request_id: 4,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                    out: out.clone(),
                })
                .unwrap();
            let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(id, 4);
            let gui::gui_frame::Msg::AudioStatus(status) = reply else {
                panic!("public pending-publication status");
            };
            assert_eq!(status.system_sharing_requested, Some(true));
            assert!(status.system_sharing_error.is_empty());
            assert_eq!(status.config.unwrap().group_id, [3; 16]);
            if order == CapabilityOrder::OldPositiveReceipt {
                capabilities_published(&source_coordinator.caps, withdrawal);
            }
            source_coordinator
                .publish_capabilities(&source_manager, &mut source_header)
                .await;
        }
        let mut source_advertised_at_request = false;
        let request_id = timeout(Duration::from_secs(5), async {
            loop {
                let (peer, envelope_connection, message) =
                    match listener_events.recv().await.unwrap() {
                        transport::LinkEvent::Message {
                            peer,
                            message: transport::convert::Inbound::CapabilitiesChanged(capabilities),
                        } => {
                            listener_coordinator.capabilities_changed(peer, &capabilities);
                            continue;
                        }
                        transport::LinkEvent::Message {
                            peer,
                            message:
                                transport::convert::Inbound::Audio {
                                    message,
                                    connection_id,
                                },
                        } => (peer, connection_id, message),
                        transport::LinkEvent::Down { .. } => {
                            panic!("current authenticated parent closed before SYSTEM request")
                        }
                        _ => continue,
                    };
                let request = match &message.body {
                    Some(net::audio_control::Body::StartRequest(request)) => {
                        assert_eq!(request.source_node_id.as_slice(), source_id.as_bytes());
                        assert_eq!(request.group_id.as_slice(), &[3; 16]);
                        source_advertised_at_request = listener_actor
                            .links
                            .borrow()
                            .peers
                            .get(&source_id)
                            .unwrap()
                            .support
                            .system_source;
                        Some(<[u8; 16]>::try_from(request.request_id.as_slice()).unwrap())
                    }
                    _ => None,
                };
                listener_coordinator.receive(peer, envelope_connection, message);
                let command = listener_actor.commands.recv().await.unwrap();
                assert_eq!(command.parent.connection_id, connection);
                assert_eq!(command.parent.generation, listener_generation);
                // The legal command-first Actor dispatch intentionally precedes
                // consuming links.changed. No model support is assigned here.
                listener_actor.on_network(command.parent, command.message.unwrap());
                listener_actor.drain().await;
                if let Some(request_id) = request {
                    break request_id;
                }
            }
        })
        .await
        .expect("real public Source ON produces a SYSTEM request");
        timeout(Duration::from_secs(5), async {
            loop {
                let (_, _, response) = next_audio(&mut source_events).await;
                match response.body {
                    Some(net::audio_control::Body::Offer(offer)) => {
                        assert_eq!(offer.request_id.as_slice(), request_id);
                        assert_eq!(offer.group_id.as_slice(), &[3; 16]);
                        break;
                    }
                    Some(net::audio_control::Body::StartRefusal(refusal)) => {
                        assert_eq!(refusal.request_id.as_slice(), request_id);
                        panic!("published Source support must yield same-ID Offer, got StartRefusal reason={}", refusal.reason);
                    }
                    _ => {}
                }
            }
        }).await.expect("same-ID Offer arrives within one deadline");
        assert!(
            source_advertised_at_request,
            "authenticated Source capability must precede SYSTEM request"
        );
        listener_manager.kill_all().await;
        let mut offers = 1;
        timeout(Duration::from_secs(5), async {
            loop {
                match source_events.recv().await.unwrap() {
                    transport::LinkEvent::Message {
                        message: transport::convert::Inbound::Audio { message, .. },
                        ..
                    } => {
                        if let Some(net::audio_control::Body::Offer(offer)) = message.body {
                            assert_eq!(offer.request_id.as_slice(), request_id);
                            offers += 1;
                        }
                    }
                    transport::LinkEvent::Down { peer, .. } if peer == listener_id => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            offers, 1,
            "one real same-ID Offer, without fake native playback"
        );
        println!("RECEIVER_CAP_ORDER source_on=accepted source_capability=authenticated offers=1 same_request=true");
        source_coordinator.stopping.send_replace(true);
        source_runner.await.unwrap();
        source_native.await.unwrap();
        drop(listener_actor);
        listener_native.await.unwrap();
        source_manager.kill_all().await;
        listener_task.abort();
        source_task.abort();
        let _ = listener_task.await;
        let _ = source_task.await;
        drop(listener_coordinator);
        drop(source_coordinator);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SourceSetup {
        Restart,
        TerminalFailure,
        Legacy,
        WaitingLocalDeny,
        WaitingUnpair,
        RoleFlip,
        StaleScope,
        KnownWrongRole,
    }

    async fn run_published_actor(actor: Actor, mut caps: watch::Receiver<Capabilities>) {
        let manager = Arc::clone(&actor.manager);
        let mut hello = net::Hello {
            node_id: actor.local.as_bytes().to_vec(),
            display_name: "fixture".into(),
            session_kind: net::SessionKind::X11 as i32,
            ..Default::default()
        };
        tokio::select! {
            _ = actor.run() => {},
            _ = async {
                loop {
                    publish_capabilities(&mut caps, &manager, &mut hello).await;
                    if caps.changed().await.is_err() { break; }
                }
            } => {},
        }
    }

    async fn unsolicited_offer_with_stored_consent(setup: SourceSetup) {
        let terminal_failure = setup == SourceSetup::TerminalFailure;
        let legacy = matches!(setup, SourceSetup::Legacy | SourceSetup::KnownWrongRole);
        let unpaired = setup == SourceSetup::WaitingUnpair;
        let stale_scope = setup == SourceSetup::StaleScope;
        let role_flip = matches!(setup, SourceSetup::RoleFlip | SourceSetup::StaleScope);
        let known_wrong_role = setup == SourceSetup::KnownWrongRole;
        let root = std::env::temp_dir().join(format!(
            "bifrost-sharing-offer-{}",
            u128::from_be_bytes(identity::random16().expect("owned fixture ID"))
        ));
        let source_root = root.join("source");
        let listener_root = root.join("listener");
        let source_identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                &source_root,
            ))
            .expect("Source identity")
            .0,
        );
        let listener_identity = Arc::new(
            identity::NodeIdentity::load_or_create(&identity::IdentityPaths::rooted_at(
                &listener_root,
            ))
            .expect("Listener identity")
            .0,
        );
        let source_id = source_identity.node_id();
        let listener_id = listener_identity.node_id();
        let (source_manager, mut source_events, source_trust) = manager_for(
            source_identity,
            listener_id,
            None,
            vec![
                net::Capability::AudioControlV1 as i32,
                net::Capability::AudioFocusReportV1 as i32,
            ],
        );
        let source_runner = tokio::spawn({
            let manager = Arc::clone(&source_manager);
            async move {
                manager.run().await.expect("Source manager");
            }
        });
        // Rebind port 0 atomically: no free-port reservation/handoff race.
        let source_address = source_manager
            .rebind_listener("127.0.0.1:0".parse().expect("owned address"))
            .await
            .expect("bound Source listener");
        let (listener_manager, mut listener_events, _) = manager_for(
            listener_identity,
            source_id,
            Some(source_address),
            vec![
                net::Capability::AudioControlV1 as i32,
                net::Capability::AudioListener as i32,
            ],
        );
        let listener_runner = tokio::spawn({
            let manager = Arc::clone(&listener_manager);
            async move {
                manager.run().await.expect("Listener manager");
            }
        });
        let (peer, generation, connection, hello, remote) = next_up(&mut source_events).await;
        assert_eq!(peer, listener_id);
        let (_, _, listener_connection, _, _) = next_up(&mut listener_events).await;
        assert_eq!(connection, listener_connection);
        let paths = config::Paths::new(
            &source_root,
            &source_root,
            &source_root,
            Some(source_root.clone()),
        );
        let audio = config::AudioConfig {
            enabled: true,
            system_sharing: if legacy { None } else { Some(true) },
            role: if role_flip {
                config::AudioRole::Listener
            } else {
                config::AudioRole::Source
            },
            group_id: Some(config::audio::HexId([3; 16])),
            listener_node_id: (!role_flip).then_some(config::audio::HexId(*listener_id.as_bytes())),
            allow_send_to: if legacy {
                vec![config::audio::HexId(*listener_id.as_bytes())]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let mut persisted = config::Config {
            audio: audio.clone(),
            ..Default::default()
        };
        persisted.network.listen_address = "127.0.0.1".into();
        persisted
            .save(paths.config_file())
            .expect("stored consent, not current activation");
        let (events, _status) = broadcast::channel(32);
        let (coordinator, mut actor) = Coordinator::prepare(
            audio,
            source_id,
            Arc::clone(&source_manager),
            paths.clone(),
            source_address,
            true,
            events,
        );
        coordinator.trusted(&source_trust);
        coordinator.link_up(peer, generation, connection, &hello, remote, true);
        actor.configure().expect("startup configuration");
        actor.sync_links();
        actor.drain().await;
        assert_eq!(
            actor.system_requested, role_flip,
            "only the initial Listener owns active receive intent"
        );
        if role_flip || known_wrong_role {
            let stale = wire::encode_control(
                &fsm::Control::Hello(fsm::Hello {
                    role: model::AudioRole::Source,
                    group: Some([3; 16]),
                    listener: *source_id.as_bytes(),
                    available: true,
                    allowed: true,
                    consent_revision: 1,
                }),
                model::AudioRole::Source,
                44913,
                connection,
            )
            .unwrap();
            listener_manager
                .send_audio_control(&source_id, stale)
                .await
                .unwrap();
            let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
            coordinator.receive(peer, envelope_connection, message);
            let command = actor.commands.recv().await.unwrap();
            actor.on_network(command.parent, command.message.unwrap());
            actor.drain().await;
            if stale_scope {
                let parent = actor.peers.get(&listener_id).unwrap().parent;
                for hello in [
                    fsm::Hello {
                        role: model::AudioRole::Listener,
                        group: Some([4; 16]),
                        listener: *listener_id.as_bytes(),
                        available: true,
                        allowed: true,
                        consent_revision: 20,
                    },
                    fsm::Hello {
                        role: model::AudioRole::Source,
                        group: Some([3; 16]),
                        listener: *source_id.as_bytes(),
                        available: true,
                        allowed: true,
                        consent_revision: 19,
                    },
                ] {
                    let message = wire::encode_control(
                        &fsm::Control::Hello(hello),
                        hello.role,
                        44913,
                        connection,
                    )
                    .unwrap();
                    listener_manager
                        .send_audio_control(&source_id, message)
                        .await
                        .unwrap();
                    let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
                    coordinator.receive(peer, envelope_connection, message);
                    let command = actor.commands.recv().await.unwrap();
                    assert_eq!(command.parent, parent, "same authenticated Parent");
                    actor.on_network(command.parent, command.message.unwrap());
                    actor.drain().await;
                }
            }
            // Only the native IPC boundary is controlled; public intent, the
            // actual Actor and authenticated peer messages remain real.
            let (handle, _stops, shutdown, native) = AudioHandle::controlled_stop(1);
            actor.worker = Some(Worker {
                handle,
                generation: 1,
                alive: true,
                model_ready: false,
                pid: 0,
                probe_ack: 0,
            });
            actor.probed(
                1,
                1,
                worker::Ready {
                    backend: worker::BackendKind::Dry as i32,
                    probed: true,
                    capabilities: [
                        worker::Capability::SystemCapture,
                        worker::Capability::Render,
                    ]
                    .into_iter()
                    .map(|capability| worker::CapabilityAvailability {
                        capability: capability as i32,
                        availability: worker::Availability::Available as i32,
                        ..Default::default()
                    })
                    .collect(),
                },
            );
            actor.drain().await;
            let (out, mut replies) = mpsc::channel(4);
            let runner = tokio::spawn(run_published_actor(actor, coordinator.caps.clone()));
            coordinator
                .submit(Request {
                    request_id: 0,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                    out: out.clone(),
                })
                .unwrap();
            let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(id, 0);
            let gui::gui_frame::Msg::AudioStatus(initial) = reply else {
                panic!("initial public status reply")
            };
            coordinator
                .submit(Request {
                    request_id: 1,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::SetSystemAudioSharing(
                        gui::SetSystemAudioSharing {
                            enabled: true,
                            role: gui::AudioRole::Source as i32,
                            listener_node_id: listener_id.as_bytes().to_vec(),
                            expected_revision: initial.revision,
                        },
                    ),
                    out: out.clone(),
                })
                .unwrap();
            let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(id, 1);
            assert!(
                matches!(&reply, gui::gui_frame::Msg::OpResult(result) if result.ok),
                "public Source ON reply: {reply:?}"
            );
            coordinator
                .submit(Request {
                    request_id: 2,
                    connection_id: [11; 16],
                    message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                    out: out.clone(),
                })
                .unwrap();
            let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(id, 2);
            let gui::gui_frame::Msg::AudioStatus(status) = reply else {
                panic!("public status reply")
            };
            println!(
                "SOURCE_SCOPE_STATUS requested={:?} error={} group={:?}",
                status.system_sharing_requested,
                status.system_sharing_error,
                status.config.as_ref().unwrap().group_id
            );
            let mut requests = 0;
            if role_flip {
                assert_eq!(
                    status.system_sharing_requested,
                    Some(true),
                    "new intent survives unresolved scope or a stale peer role"
                );
                assert!(status.system_sharing_error.is_empty());
                if stale_scope {
                    assert_eq!(status.config.unwrap().group_id.as_slice(), &[4; 16]);
                } else {
                    assert!(status.config.unwrap().group_id.is_empty());
                }
                assert!(
                    !*shutdown.borrow(),
                    "pending Source keeps its controlled worker"
                );
                if !stale_scope {
                    let fresh = wire::encode_control(
                        &fsm::Control::Hello(fsm::Hello {
                            role: model::AudioRole::Listener,
                            group: Some([4; 16]),
                            listener: *listener_id.as_bytes(),
                            available: true,
                            allowed: true,
                            consent_revision: 2,
                        }),
                        model::AudioRole::Listener,
                        44913,
                        connection,
                    )
                    .unwrap();
                    listener_manager
                        .send_audio_control(&source_id, fresh)
                        .await
                        .unwrap();
                    let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
                    coordinator.receive(peer, envelope_connection, message);
                }
                timeout(Duration::from_secs(5), async {
                    loop {
                        let (_, _, message) = next_audio(&mut listener_events).await;
                        if let Some(net::audio_control::Body::StartRequest(request)) = message.body
                        {
                            assert_eq!(request.group_id.as_slice(), &[4; 16]);
                            assert_eq!(request.source_node_id.as_slice(), source_id.as_bytes());
                            assert_eq!(request.kind, net::AudioStreamKind::System as i32);
                            requests += 1;
                            break;
                        }
                    }
                })
                .await
                .expect("fresh authenticated Listener scope starts the existing intent");
                for request_id in 3..=10 {
                    coordinator
                        .submit(Request {
                            request_id,
                            connection_id: [11; 16],
                            message: gui::gui_frame::Msg::GetAudioStatus(gui::GetAudioStatus {}),
                            out: out.clone(),
                        })
                        .unwrap();
                    let (id, reply) = timeout(Duration::from_secs(5), replies.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(id, request_id);
                    let gui::gui_frame::Msg::AudioStatus(status) = reply else {
                        panic!("public status reply")
                    };
                    assert_eq!(status.system_sharing_requested, Some(true));
                    assert!(status.system_sharing_error.is_empty());
                    assert_eq!(status.config.unwrap().group_id.as_slice(), &[4; 16]);
                }
            } else {
                assert_eq!(status.system_sharing_requested, Some(false));
                assert_eq!(
                    status.system_sharing_error,
                    ipc::audio::codes::AUDIO_ROLE_CONFLICT
                );
            }
            coordinator.stopping.send_replace(true);
            runner.await.unwrap();
            native.await.unwrap();
            source_manager.kill_all().await;
            timeout(Duration::from_secs(5), async {
                loop {
                    match listener_events.recv().await.unwrap() {
                        transport::LinkEvent::Message {
                            message: transport::convert::Inbound::Audio { message, .. },
                            ..
                        } => {
                            if matches!(
                                message.body,
                                Some(net::audio_control::Body::StartRequest(_))
                            ) {
                                requests += 1;
                            }
                        }
                        transport::LinkEvent::Down { peer, .. } if peer == source_id => break,
                        _ => {}
                    }
                }
            })
            .await
            .expect("owned peer link closes after observed startup messages");
            assert_eq!(requests, usize::from(role_flip), "one accepted intent sends one peer System RequestStart, known-scope refusal sends none");
            listener_manager.kill_all().await;
            source_runner.abort();
            listener_runner.abort();
            let _ = source_runner.await;
            let _ = listener_runner.await;
            drop(coordinator);
            std::fs::remove_dir_all(root).unwrap();
            return;
        }

        if matches!(
            setup,
            SourceSetup::WaitingLocalDeny | SourceSetup::WaitingUnpair
        ) {
            // Current accepted intent, but no AudioHello and no first P1 attempt.
            actor.system_requested = true;
            actor.system_start = Some(Start {
                peer: listener_id,
                deadline: None,
                request_id: None,
            });
            actor.configure().expect("initial waiting intent");
            actor.drain().await;
            actor.epoch = actor
                .epoch
                .checked_sub(Duration::from_secs(11))
                .expect("clock anchor");
            Box::pin(actor.reconcile_system()).await;
            assert!(
                actor.system_requested,
                "walking to the receiver must not expire intent"
            );
            assert!(actor.system_error.is_none());
            let waiting = actor.system_start.as_ref().expect("still waiting");
            assert!(waiting.request_id.is_none() && waiting.deadline.is_none());
            assert!(actor.worker.is_none(), "waiting has no native capture");
            if unpaired {
                coordinator.trusted(&identity::TrustStore::new());
            } else {
                actor.config.peers.push(config::AudioPeerConfig {
                    node_id: config::audio::HexId(*listener_id.as_bytes()),
                    system_send_override: Some(false),
                    system_receive_override: None,
                    allow_media_control: false,
                    allow_mic_control: false,
                    gain_percent: 100,
                    muted: false,
                });
                actor
                    .save(actor.config.clone())
                    .await
                    .expect("real Advanced deny persistence");
                actor
                    .configure()
                    .expect("apply local permission withdrawal");
            }
            Box::pin(actor.reconcile_system()).await;
            assert!(
                !actor.system_requested,
                "local revocation precedes any late Hello"
            );
            assert!(actor.system_start.is_none());
            assert_eq!(
                actor.system_error,
                Some(ipc::audio::codes::AUDIO_CONSENT_REQUIRED)
            );
        }
        let listener_hello = wire::encode_control(
            &fsm::Control::Hello(fsm::Hello {
                role: model::AudioRole::Listener,
                group: Some([3; 16]),
                listener: *listener_id.as_bytes(),
                available: true,
                allowed: true,
                consent_revision: 1,
            }),
            model::AudioRole::Listener,
            44913,
            connection,
        )
        .expect("valid Listener Hello");
        listener_manager
            .send_audio_control(&source_id, listener_hello)
            .await
            .expect("real TLS send");
        let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
        coordinator.receive(peer, envelope_connection, message);
        if unpaired {
            assert!(
                matches!(
                    actor.commands.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ),
                "already authenticated but locally revoked parent cannot enqueue Hello"
            );
        } else {
            let command = actor
                .commands
                .recv()
                .await
                .expect("real coordinator ingress");
            actor.on_network(command.parent, command.message.expect("validated control"));
            actor.drain().await;
        }
        if terminal_failure {
            // Real accepted user persistence followed by real child startup failure:
            // this lib-test executable is not an audio-role executable. No native
            // Ready/Started/health facts or successful child ACK are fabricated.
            actor.publish("before_attempt");
            let (out, mut replies) = mpsc::channel(4);
            coordinator
                .submit(Request {
                    request_id: 57,
                    connection_id: [9; 16],
                    out,
                    message: gui::gui_frame::Msg::SetSystemAudioSharing(
                        gui::SetSystemAudioSharing {
                            enabled: true,
                            role: gui::AudioRole::Source as i32,
                            listener_node_id: listener_id.as_bytes().to_vec(),
                            expected_revision: actor.revision,
                        },
                    ),
                })
                .expect("explicit current user intent");
            Box::pin(actor.system_intent()).await;
            let (_, reply) = replies.recv().await.expect("actual persistence ACK");
            assert!(matches!(reply, gui::gui_frame::Msg::OpResult(result) if result.ok));
            assert!(!actor.system_requested);
            assert!(
                actor.system_error.is_some(),
                "failed native startup is authoritative"
            );
            Box::pin(actor.reconcile_system()).await;
        }
        let session = identity::random16().expect("fresh incoming session ID");
        let format = format(Profile::Ethernet);
        let offer = wire::encode_control(
            &fsm::Control::Offer(fsm::Offer {
                request_id: None,
                session,
                listener_nonce: [4; 32],
                parent_connection_id: connection,
                group: [3; 16],
                listener: *listener_id.as_bytes(),
                source: *source_id.as_bytes(),
                format,
                format_sha256: wire::format_sha256(format),
                listener_port: 44913,
                consent_revision: 1,
            }),
            model::AudioRole::Listener,
            44913,
            connection,
        )
        .expect("valid unsolicited Offer");
        listener_manager
            .send_audio_control(&source_id, offer)
            .await
            .expect("authenticated unsolicited Offer");
        let (peer, envelope_connection, message) = next_audio(&mut source_events).await;
        assert!(
            matches!(&message.body, Some(net::audio_control::Body::Offer(offer))
            if offer.session_id == session && offer.parent_connection_id == envelope_connection)
        );
        coordinator.receive(peer, envelope_connection, message);
        if unpaired {
            assert!(
                matches!(
                    actor.commands.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ),
                "late Offer is fenced at the revoked Source parent boundary"
            );
        } else {
            let command = actor
                .commands
                .recv()
                .await
                .expect("Offer reaches Source coordinator");
            assert_eq!(command.parent.connection_id, connection);
            actor.on_network(
                command.parent,
                command.message.expect("valid parent-bound Offer"),
            );
            actor.drain().await;
            let answer = timeout(Duration::from_secs(5), async {
                loop {
                    let (_, _, message) = next_audio(&mut listener_events).await;
                    if let Some(net::audio_control::Body::Answer(answer)) = message.body {
                        if answer.session_id == session {
                            break answer;
                        }
                    }
                }
            })
            .await
            .expect("Source sends a correlated refusal over authenticated link");
            assert!(!answer.accepted);
            assert!(
                answer.source_nonce.is_empty(),
                "first nonce/key/native Prepare was not authorized"
            );
            assert_eq!(
                answer.reason,
                if legacy {
                    net::AudioStopCode::DeviceUnavailable
                } else {
                    net::AudioStopCode::Disabled
                } as i32
            );
        }
        assert!(actor.model.session(*listener_id.as_bytes()).is_none());
        assert!(
            actor.worker.is_none(),
            "external Offer cannot spawn a media worker"
        );
        assert!(
            actor.records.is_empty(),
            "no native session/materials were prepared"
        );
        let saved = config::Config::load(paths.config_file()).expect("actual persisted config");
        assert_eq!(
            saved.audio.system_sharing,
            if legacy { None } else { Some(true) }
        );
        assert!(!saved.audio.resume_system_audio);
        source_manager.kill_all().await;
        listener_manager.kill_all().await;
        source_runner.abort();
        listener_runner.abort();
        let _ = source_runner.await;
        let _ = listener_runner.await;
        drop(actor);
        drop(coordinator);
        std::fs::remove_dir_all(root).expect("remove owned TLS fixture");
    }

    #[tokio::test]
    async fn source_scope_role_flip_waits_for_current_listener_before_start() {
        unsolicited_offer_with_stored_consent(SourceSetup::RoleFlip).await;
    }

    #[tokio::test]
    async fn source_scope_stale_peer_role_cannot_cancel_new_intent() {
        unsolicited_offer_with_stored_consent(SourceSetup::StaleScope).await;
    }

    #[tokio::test]
    async fn source_scope_known_group_wrong_role_remains_terminal() {
        unsolicited_offer_with_stored_consent(SourceSetup::KnownWrongRole).await;
    }

    #[tokio::test]
    async fn authenticated_unsolicited_offer_cannot_activate_restarted_modern_source() {
        unsolicited_offer_with_stored_consent(SourceSetup::Restart).await;
    }

    #[tokio::test]
    async fn authenticated_unsolicited_offer_cannot_rearm_terminally_failed_modern_source() {
        unsolicited_offer_with_stored_consent(SourceSetup::TerminalFailure).await;
    }

    #[tokio::test]
    async fn legacy_explicit_source_consent_remains_distinct_from_modern_runtime_intent() {
        unsolicited_offer_with_stored_consent(SourceSetup::Legacy).await;
    }

    #[tokio::test]
    async fn waiting_past_ten_seconds_local_deny_cancels_before_late_authenticated_hello_offer() {
        unsolicited_offer_with_stored_consent(SourceSetup::WaitingLocalDeny).await;
    }

    #[tokio::test]
    async fn waiting_past_ten_seconds_unpair_cancels_before_late_authenticated_hello_offer() {
        unsolicited_offer_with_stored_consent(SourceSetup::WaitingUnpair).await;
    }
}
