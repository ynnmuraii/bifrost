//! Listener-only physical microphone authority; Sources relay intent, never acquire their mic.
use super::*;
use audio_model::microphone::{self as policy, Authority, Control, Effect, Op};
use net::{AudioControlResult as ResultCode, MicControlOp};

struct Held {
    connection: [u8; 16],
    out: mpsc::Sender<Outgoing>,
    press: [u8; 16],
    parent: fsm::Parent,
    next: u64,
}
enum Reply {
    Gui(Request),
    Network(fsm::Parent, Vec<u8>),
}
struct PendingMic {
    reply: Reply,
    worker_generation: [u8; 16],
    request: u64,
    deadline: u64,
    muted: Option<bool>,
    previous: bool,
    endpoint_generation: u64,
}
struct Recovery {
    worker_generation: [u8; 16],
    request: u64,
    endpoint_generation: u64,
    muted: bool,
    deadline: u64,
}
impl Recovery {
    fn matches(&self, worker_generation: [u8; 16], request: u64) -> bool {
        self.worker_generation == worker_generation && self.request == request
    }
    fn result(&self, snapshot: &worker::MuteSnapshot) -> ResultCode {
        if snapshot.generation != self.endpoint_generation || !snapshot.guard_active {
            ResultCode::DeviceGone
        } else if snapshot.muted != self.muted {
            ResultCode::Conflict
        } else {
            ResultCode::Ok
        }
    }
}
struct Remote {
    parent: fsm::Parent,
    state: net::MicState,
}
impl Remote {
    fn acknowledge(&mut self, parent: fsm::Parent, revision: u64) {
        if self.parent == parent {
            // Completion advances CAS even when mute/PTT facts have not changed.
            self.state.revision = self.state.revision.max(revision);
        }
    }
}
#[derive(Default)]
pub(super) struct State {
    authority: Authority,
    endpoint: Option<worker::MuteSnapshot>,
    acquire: Option<(u64, u64)>,
    configured: Option<(u64, bool, String)>,
    pending: Option<PendingMic>,
    outbound: BTreeMap<[u8; 16], (fsm::Parent, Request, u64)>,
    remote: Option<Remote>,
    held: Option<Held>,
    native_expires: u64,
    native_press: Option<[u8; 16]>,
    sequence: u64,
    last_publish: u64,
    reserved: Option<Vec<worker::PttHotkey>>,
    actual_grant: u64,
    native_parent: Option<fsm::Parent>,
    recovery: Option<Recovery>,
    recovered: bool,
    recovery_error: Option<ResultCode>,
    config_muted: Option<bool>,
    privacy_effect: Option<Effect>,
    lock_paused: bool,
    warned_refusal: bool,
    warned_nonextending: bool,
    warned_policy_expiry: bool,
    warned_native_terminal: bool,
    warned_native_orphan: bool,
}
impl State {
    fn update_remote(
        &mut self,
        parent: fsm::Parent,
        state: &net::MicState,
        now_ms: u64,
        now_ns: u64,
    ) -> Option<[u8; 16]> {
        let fresh = self.remote.as_ref().is_none_or(|remote| {
            remote.parent != parent
                || state.revision > remote.state.revision
                || state.expires_ns > remote.state.expires_ns
        });
        self.remote = Some(Remote {
            parent,
            state: state.clone(),
        });
        if !fresh || !state.supported || !state.ptt_enabled {
            return None;
        }
        // Fresh Listener authority drives renewal; a timer may still hold an old challenge.
        if let Some(held) = self.held.as_mut().filter(|held| held.parent == parent) {
            held.next = now_ms + 100;
            return Some(held.press);
        }
        if self.native_parent == Some(parent) && self.native_expires > now_ns {
            self.last_publish = now_ms;
            return self.native_press;
        }
        None
    }
    fn observe_grant(&mut self, event: &worker::PttEvent, now: u64) {
        let active = native_live(event, now);
        let matches = self.authority.grant.is_some_and(|g| g.id == event.grant_id);
        let persistent_noop = event.kind == worker::PttEventKind::Noop as i32
            && event.reason == worker::PttReleaseReason::PersistentUnmute as i32;
        if !active && !persistent_noop && matches && !self.warned_native_terminal {
            self.warned_native_terminal = true;
            tracing::warn!(
                kind = event.kind,
                reason = event.reason,
                event_remaining_ms = event.expires_at_ns.saturating_sub(now) / 1_000_000,
                event_overdue_ms = if event.expires_at_ns == 0 {
                    0
                } else {
                    now.saturating_sub(event.expires_at_ns) / 1_000_000
                },
                policy_remaining_ms = self
                    .authority
                    .grant
                    .map_or(0, |g| g.expires_ns.saturating_sub(now) / 1_000_000),
                pending = self.pending.is_some(),
                "microphone first native terminal event for a logical grant"
            );
        }
        if active && !matches && !self.warned_native_orphan {
            self.warned_native_orphan = true;
            tracing::warn!(
                kind = event.kind,
                reason = event.reason,
                event_remaining_ms = event.expires_at_ns.saturating_sub(now) / 1_000_000,
                pending = self.pending.is_some(),
                "microphone first native active event without a logical grant"
            );
        }
        if active {
            self.actual_grant = event.grant_id;
            self.authority.recovery_mode = Some(policy::RecoveryMode::Ptt);
        } else if self.actual_grant == event.grant_id {
            self.actual_grant = 0;
        }
        // A successful no-op still owns its logical press until release/expiry.
        if !active && !persistent_noop && matches {
            self.authority.revoke();
        }
    }
    fn mute_on_lock(&self) -> bool {
        self.authority.ptt_enabled
            && self.authority.recovery_mode != Some(policy::RecoveryMode::Persistent(false))
    }
    fn sync_config_intent(&mut self, muted: bool) {
        if self.config_muted != Some(muted) {
            if self.config_muted.is_some() {
                self.authority.set_local_muted(muted);
            } else {
                self.authority.desired_muted = muted;
            }
            self.config_muted = Some(muted);
        }
    }
}
fn host_ns() -> Option<u64> {
    ipc::clock::host_monotonic_us()?.checked_mul(1000)
}
fn native_live(event: &worker::PttEvent, now: u64) -> bool {
    if event.expires_at_ns <= now || event.expires_at_ns.saturating_sub(now) > policy::LEASE_NS {
        return false;
    }
    match worker::PttEventKind::try_from(event.kind) {
        Ok(worker::PttEventKind::Pressed) => {
            event.observed_at_ns > 0
                && event.observed_at_ns <= now
                && event.expires_at_ns.saturating_sub(event.observed_at_ns) <= policy::LEASE_NS
        }
        Ok(worker::PttEventKind::Renewed) => event.observed_at_ns == 0,
        _ => false,
    }
}
fn ptt_result(event: &worker::PttEvent, now: u64) -> ResultCode {
    if event.kind == worker::PttEventKind::Refused as i32 {
        return if event.reason == worker::PttReleaseReason::BackendFailed as i32 {
            ResultCode::DeviceGone
        } else {
            ResultCode::Conflict
        };
    }
    let active = matches!(
        worker::PttEventKind::try_from(event.kind),
        Ok(worker::PttEventKind::Pressed | worker::PttEventKind::Renewed)
    );
    if event.reason == worker::PttReleaseReason::Expired as i32
        || (active && !native_live(event, now))
    {
        ResultCode::Expired
    } else {
        ResultCode::Ok
    }
}
fn id(sequence: u64, connection: [u8; 16]) -> [u8; 16] {
    let mut id = connection;
    id[8..].copy_from_slice(&sequence.to_be_bytes());
    id
}
fn result(value: policy::ResultCode) -> ResultCode {
    match value {
        policy::ResultCode::Ok => ResultCode::Ok,
        policy::ResultCode::Disabled => ResultCode::Disabled,
        policy::ResultCode::ConsentRequired => ResultCode::ConsentRequired,
        policy::ResultCode::StaleRevision => ResultCode::StaleRevision,
        policy::ResultCode::Conflict => ResultCode::Conflict,
        policy::ResultCode::Expired => ResultCode::Expired,
        policy::ResultCode::Busy => ResultCode::Busy,
        policy::ResultCode::Invalid => ResultCode::Invalid,
        policy::ResultCode::Unsupported => ResultCode::Unsupported,
        policy::ResultCode::DeviceGone => ResultCode::DeviceGone,
        policy::ResultCode::Timeout => ResultCode::Timeout,
        policy::ResultCode::Cancelled => ResultCode::Cancelled,
    }
}
fn policy_result(code: ResultCode) -> policy::ResultCode {
    match code {
        ResultCode::Ok => policy::ResultCode::Ok,
        ResultCode::Disabled => policy::ResultCode::Disabled,
        ResultCode::ConsentRequired => policy::ResultCode::ConsentRequired,
        ResultCode::StaleRevision => policy::ResultCode::StaleRevision,
        ResultCode::Unsupported => policy::ResultCode::Unsupported,
        ResultCode::DeviceGone => policy::ResultCode::DeviceGone,
        ResultCode::Conflict => policy::ResultCode::Conflict,
        ResultCode::Expired => policy::ResultCode::Expired,
        ResultCode::Busy => policy::ResultCode::Busy,
        ResultCode::Timeout => policy::ResultCode::Timeout,
        ResultCode::Cancelled => policy::ResultCode::Cancelled,
        ResultCode::Invalid => policy::ResultCode::Invalid,
    }
}
fn gui_result(result: ResultCode) -> Result<(), Code> {
    use ipc::audio::codes::*;
    match result {
        ResultCode::Ok => Ok(()),
        ResultCode::ConsentRequired => Err(AUDIO_CONSENT_REQUIRED),
        ResultCode::Conflict | ResultCode::StaleRevision => Err(AUDIO_REVISION_CONFLICT),
        ResultCode::Unsupported => Err(AUDIO_UNSUPPORTED_BACKEND),
        ResultCode::DeviceGone => Err(AUDIO_DEVICE_UNAVAILABLE),
        _ => Err(AUDIO_UNAVAILABLE),
    }
}
impl Actor {
    pub(super) async fn mic_stop(&mut self) {
        if let (Some(w), Some(grant)) = (&self.worker, self.mic.authority.grant) {
            if let Err(error) = w.handle.ptt_revoke(grant.id).await {
                if self.native_privacy_failure(&error) {
                    return;
                }
            }
        }
        if let Some(w) = &self.worker {
            let _ = w.handle.configure_ptt(None, Vec::new()).await;
            if self.config.role == config::AudioRole::Listener {
                if let Err(error) = w.handle.release_mic_mute().await {
                    if self.native_privacy_failure(&error) {
                        return;
                    }
                }
            }
        }
    }
    pub(super) fn mic_set_hotkeys(&mut self, bindings: &config::HotkeyBindings) {
        self.mic.reserved = reserved_hotkeys(bindings).ok();
        self.mic.configured = None;
    }
    pub(super) fn mic_validate_config(&self, cfg: &config::AudioConfig) -> Result<(), Code> {
        if cfg.microphone.ptt_enabled && !cfg.microphone.ptt_hotkey.is_empty() {
            let binding = hotkey(&cfg.microphone.ptt_hotkey)?;
            let reserved = self
                .mic
                .reserved
                .as_ref()
                .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE)?;
            if reserved.contains(&binding) {
                return Err(ipc::audio::codes::AUDIO_REVISION_CONFLICT);
            }
            if !available(self.ready.as_ref(), worker::Capability::GlobalPtt) {
                return Err(ipc::audio::codes::AUDIO_UNSUPPORTED_BACKEND);
            }
        }
        Ok(())
    }
    fn mic_configured(&self) -> bool {
        self.config.enabled
            && self.config.microphone.control_enabled
            && self.config.group_id.is_some()
            && self.configuration_error.is_none()
    }
    fn mic_enabled(&self) -> bool {
        self.mic_configured() && !self.session_locked
    }
    pub(super) fn mic_capable(&self) -> bool {
        self.mic_enabled()
            && (self.config.role == config::AudioRole::Source
                || available(self.ready.as_ref(), worker::Capability::EndpointMute))
    }
    fn mic_ready(&self) -> bool {
        self.mic.recovered
            && self.mic.recovery_error.is_none()
            && self.mic.endpoint.as_ref().is_some_and(|s| s.guard_active)
    }
    fn mic_state_reason(&self) -> ResultCode {
        if let Some(reason) = self.mic.recovery_error {
            return reason;
        }
        if self.mic_ready() {
            return ResultCode::Ok;
        }
        if !available(self.ready.as_ref(), worker::Capability::EndpointMute) {
            return ResultCode::Unsupported;
        }
        if self.mic.recovery.is_some() || self.mic.acquire.is_some() {
            ResultCode::Busy
        } else {
            ResultCode::DeviceGone
        }
    }
    async fn mic_begin_recovery(&mut self, raw_generation: u64, snapshot: &worker::MuteSnapshot) {
        self.mic.recovered = false;
        self.mic.recovery_error = None;
        self.mic.recovery = None;
        self.mic.authority.revoke();
        self.mic.held = None;
        self.mic.native_expires = 0;
        self.mic.native_press = None;
        self.mic.actual_grant = 0;
        self.mic.configured = None;
        self.mic.authority.ptt_enabled = self.config.microphone.ptt_enabled;
        self.mic.sync_config_intent(self.config.microphone.muted);
        let muted =
            (self.session_locked && self.mic.mute_on_lock()) || self.mic.authority.recovery_muted();
        if !snapshot.guard_active {
            self.mic.recovery_error = Some(ResultCode::DeviceGone);
            return;
        }
        let Some(w) = self
            .worker
            .as_ref()
            .filter(|w| w.alive && w.generation == raw_generation)
        else {
            self.mic.recovery_error = Some(ResultCode::DeviceGone);
            return;
        };
        if w.handle.configure_ptt(None, Vec::new()).await.is_err() {
            self.mic.recovery_error = Some(ResultCode::DeviceGone);
            return;
        }
        match w.handle.set_mic_mute(snapshot.generation, muted).await {
            Ok(request) => {
                self.mic.recovery = Some(Recovery {
                    worker_generation: super::generation(raw_generation, self.worker_epoch),
                    request,
                    endpoint_generation: snapshot.generation,
                    muted,
                    deadline: self.now() + REQUEST_TIMEOUT_MS,
                });
            }
            Err(error) => {
                if self.native_privacy_failure(&error) {
                    return;
                }
                self.mic.recovery_error = Some(
                    if matches!(error,
                        AudioSupervisorError::WorkerRejected(code) if code == worker::ErrorCode::Unsupported as i32
                    ) {
                        ResultCode::Unsupported
                    } else {
                        ResultCode::DeviceGone
                    },
                );
            }
        }
    }
    fn mic_parent_allowed(&self, parent: fsm::Parent) -> bool {
        self.links
            .borrow()
            .peers
            .get(&NodeId::new(parent.peer))
            .is_some_and(|p| {
                p.trusted
                    && p.parent == parent
                    && p.mic_support
                    && match self.config.role {
                        config::AudioRole::Listener => self
                            .config
                            .peers
                            .iter()
                            .any(|c| c.node_id.0 == parent.peer && c.allow_mic_control),
                        config::AudioRole::Source => self
                            .config
                            .listener_node_id
                            .is_some_and(|id| id.0 == parent.peer),
                        _ => false,
                    }
            })
    }
    /// Call after sync_links; native privacy effects are drained on the next tick.
    pub(super) fn mic_on_parents_changed(&mut self) {
        let local = self.mic_local_parent([0; 16]);
        let enabled = self.mic_enabled();
        let links = &self.links;
        self.mic.authority.prune_history(|p| {
            p == local
                || links
                    .borrow()
                    .peers
                    .get(&NodeId::new(p.peer))
                    .is_some_and(|peer| peer.parent == p)
        });
        let mut allowed = [None; 3];
        for (slot, peer) in allowed.iter_mut().zip(
            self.links
                .borrow()
                .peers
                .values()
                .filter(|p| enabled && self.mic_parent_allowed(p.parent)),
        ) {
            *slot = Some(peer.parent);
        }
        let effect = self
            .mic
            .authority
            .retain(|p| (enabled && p == local) || allowed.contains(&Some(p)));
        if effect != Effect::None {
            if self.mic.privacy_effect != Some(Effect::SetMute(true)) {
                self.mic.privacy_effect = Some(effect);
            }
            self.dirty = true;
        }
    }
    fn mic_local_parent(&self, _connection: [u8; 16]) -> fsm::Parent {
        fsm::Parent {
            peer: *self.local.as_bytes(),
            connection_id: [0; 16],
            generation: generation(0, self.worker_epoch),
        }
    }
    fn mic_next_id(&mut self, connection: [u8; 16]) -> [u8; 16] {
        self.mic.sequence = self.mic.sequence.saturating_add(1);
        id(self.mic.sequence, connection)
    }
    async fn mic_send(&self, parent: fsm::Parent, body: net::audio_control::Body) {
        if !self
            .links
            .borrow()
            .peers
            .get(&NodeId::new(parent.peer))
            .is_some_and(|p| p.trusted && p.parent == parent && p.mic_support)
        {
            return;
        }
        let _ = self
            .manager
            .send_audio_control(
                &NodeId::new(parent.peer),
                net::AudioControl {
                    control_version: 1,
                    body: Some(body),
                },
            )
            .await;
    }
    async fn mic_reply(&mut self, to: Reply, code: ResultCode) {
        match to {
            Reply::Gui(r) => reply(&r, gui_result(code)).await,
            Reply::Network(parent, request_id) => {
                if let Ok(id) = request_id.as_slice().try_into() {
                    self.mic.authority.complete(parent, id, policy_result(code));
                }
                self.mic_send(
                    parent,
                    net::audio_control::Body::MicAck(net::MicAck {
                        group_id: self
                            .config
                            .group_id
                            .map(|g| g.0.to_vec())
                            .unwrap_or_default(),
                        request_id,
                        revision: self.mic.authority.revision,
                        result: code as i32,
                    }),
                )
                .await;
            }
        }
    }
    pub(super) fn microphone_status(&self) -> gui::AudioMicrophoneState {
        if self.config.role == config::AudioRole::Source {
            if let Some(remote) = &self.mic.remote {
                if self.mic_enabled() && self.mic_parent_allowed(remote.parent) {
                    return gui::AudioMicrophoneState {
                        muted: remote.state.muted,
                        ptt_pressed: remote.state.ptt_active,
                        capture_active: false,
                        target_node_id: remote.parent.peer.to_vec(),
                        control_active: remote.state.supported,
                    };
                }
            }
            return gui::AudioMicrophoneState {
                muted: false,
                ..Default::default()
            };
        }
        gui::AudioMicrophoneState {
            muted: self.mic.endpoint.as_ref().is_some_and(|s| s.muted),
            ptt_pressed: host_ns().is_some_and(|now| {
                self.mic.native_expires > now
                    || self
                        .mic
                        .authority
                        .grant
                        .is_some_and(|g| g.id == self.mic.actual_grant && g.expires_ns > now)
            }),
            capture_active: false,
            target_node_id: if self.mic.endpoint.is_some() {
                self.local.as_bytes().to_vec()
            } else {
                Vec::new()
            },
            control_active: self.mic_enabled() && self.mic_ready(),
        }
    }
    pub(super) fn mic_reset(&mut self) {
        self.mic.authority.reset();
        self.mic.lock_paused = false;
        self.mic.endpoint = None;
        self.mic.acquire = None;
        self.mic.configured = None;
        self.mic.native_expires = 0;
        self.mic.native_press = None;
        self.mic.held = None;
        self.mic.remote = None;
        self.mic.actual_grant = 0;
        self.mic.native_parent = None;
        self.mic.recovery = None;
        self.mic.recovered = false;
        self.mic.privacy_effect = None;
        self.mic.recovery_error = None;
        // Synchronous reset cannot await a GUI writer. Queue bounded terminal replies directly.
        let error = gui::gui_frame::Msg::OpResult(gui::OpResult {
            ok: false,
            error_code: ipc::audio::codes::AUDIO_UNAVAILABLE.into(),
        });
        if let Some(p) = self.mic.pending.take() {
            if let Reply::Gui(r) = p.reply {
                let _ = r.out.try_send((r.request_id, error.clone()));
            }
        }
        for (_, (_, r, _)) in std::mem::take(&mut self.mic.outbound) {
            let _ = r.out.try_send((r.request_id, error.clone()));
        }
        self.dirty = true;
    }
    pub(super) async fn mic_on_gui(&mut self, request: Request) {
        let (op, revision) = match &request.message {
            gui::gui_frame::Msg::SetMicrophoneMute(v) => {
                (Op::SetMute(v.muted), v.expected_revision)
            }
            gui::gui_frame::Msg::SetPtt(v) => (
                if v.pressed { Op::Press } else { Op::Release },
                v.expected_revision,
            ),
            _ => return,
        };
        if !self.mic_enabled() {
            reply(&request, gui_result(ResultCode::Disabled)).await;
            return;
        }
        if op == Op::Press && !self.config.microphone.ptt_enabled {
            reply(&request, gui_result(ResultCode::Disabled)).await;
            return;
        }
        if matches!(op, Op::SetMute(_)) {
            self.mic.held = None;
            self.mic.native_press = None;
            self.mic.native_parent = None;
            self.mic.native_expires = 0;
        }
        if !matches!(op, Op::Release | Op::SetMute(true)) && revision != self.revision {
            reply(&request, gui_result(ResultCode::StaleRevision)).await;
            return;
        }
        if self.mic.pending.is_some() && !matches!(op, Op::Release | Op::SetMute(true)) {
            reply(&request, gui_result(ResultCode::Busy)).await;
            return;
        }
        if self.mic.outbound.len() >= 16 {
            if !matches!(op, Op::Release | Op::SetMute(true)) {
                reply(&request, gui_result(ResultCode::Busy)).await;
                return;
            }
            if let Some((_, (_, r, _))) = self.mic.outbound.pop_first() {
                reply(&r, gui_result(ResultCode::Cancelled)).await;
            }
        }
        if op == Op::Release && self.mic.held.is_none() {
            reply(&request, Ok(())).await;
            return;
        }
        let connection = request.connection_id;
        if op == Op::Release
            && self
                .mic
                .held
                .as_ref()
                .is_some_and(|h| h.connection != connection)
        {
            reply(&request, gui_result(ResultCode::Conflict)).await;
            return;
        }
        let press = if op == Op::Press {
            if self.mic.held.is_some() {
                reply(&request, gui_result(ResultCode::Conflict)).await;
                return;
            }
            self.mic_next_id(connection)
        } else {
            self.mic.held.as_ref().map(|h| h.press).unwrap_or([0; 16])
        };
        let request_id = self.mic_next_id(connection);
        let parent = if self.config.role == config::AudioRole::Listener {
            self.mic_local_parent(connection)
        } else {
            let Some(remote) = &self.mic.remote else {
                reply(&request, gui_result(ResultCode::DeviceGone)).await;
                return;
            };
            remote.parent
        };
        if op == Op::Press {
            self.mic.held = Some(Held {
                connection,
                out: request.out.clone(),
                press,
                parent,
                next: self.now() + 100,
            });
        } else if op == Op::Release {
            self.mic.held = None;
        }
        if self.config.role == config::AudioRole::Source {
            let Some(remote) = &self.mic.remote else {
                return;
            };
            if !self.mic_parent_allowed(parent) {
                self.mic.held = None;
                reply(&request, gui_result(ResultCode::ConsentRequired)).await;
                return;
            }
            let c = net::MicControl {
                group_id: remote.state.group_id.clone(),
                request_id: request_id.to_vec(),
                expected_revision: remote.state.revision,
                op: wire_op(op) as i32,
                muted: matches!(op, Op::SetMute(true)),
                press_id: if matches!(op, Op::SetMute(_)) {
                    Vec::new()
                } else {
                    press.to_vec()
                },
                challenge: if matches!(op, Op::Press | Op::Renew) {
                    remote.state.challenge
                } else {
                    0
                },
                expires_ns: if matches!(op, Op::Press | Op::Renew) {
                    remote.state.expires_ns
                } else {
                    0
                },
            };
            self.mic.outbound.insert(
                request_id,
                (parent, request, self.now() + REQUEST_TIMEOUT_MS),
            );
            self.mic_send(parent, net::audio_control::Body::MicControl(c))
                .await;
        } else {
            let Some(now) = host_ns() else {
                reply(&request, gui_result(ResultCode::Disabled)).await;
                return;
            };
            let (challenge, expires_ns) = self.mic.authority.challenge(now);
            let c = Control {
                request: request_id,
                revision: self.mic.authority.revision,
                op,
                press,
                challenge,
                expires_ns,
            };
            self.mic_apply(now, parent, c, Reply::Gui(request)).await;
        }
    }
    fn mic_warn_refusal(&mut self, code: ResultCode, revision_matches: bool) {
        if self.mic.warned_refusal {
            return;
        }
        self.mic.warned_refusal = true;
        let now = host_ns().unwrap_or(u64::MAX);
        tracing::warn!(
            listener = self.config.role == config::AudioRole::Listener,
            code = code as i32,
            revision_matches,
            pending = self.mic.pending.is_some(),
            outbound_pending = !self.mic.outbound.is_empty(),
            held = self.mic.held.is_some(),
            policy_remaining_ms = self
                .mic
                .authority
                .grant
                .map_or(0, |g| g.expires_ns.saturating_sub(now) / 1_000_000),
            "microphone first held control refusal"
        );
    }
    async fn mic_apply(&mut self, now: u64, parent: fsm::Parent, c: Control, to: Reply) {
        self.mic_on_parents_changed();
        if let Some(effect) = self.mic.privacy_effect.take() {
            if self.mic_effect(effect, now).await {
                return;
            }
        }
        let local = parent.peer == *self.local.as_bytes();
        if !self.mic_ready() && c.op != Op::Release {
            if c.op == Op::SetMute(true)
                && self.mic.endpoint.as_ref().is_some_and(|s| s.guard_active)
            {
                self.mic.recovery = None;
            } else {
                if matches!(c.op, Op::Press | Op::Renew) {
                    self.mic_warn_refusal(
                        self.mic_state_reason(),
                        c.revision == self.mic.authority.revision,
                    );
                }
                self.mic.held = None;
                self.mic_reply(to, self.mic_state_reason()).await;
                return;
            }
        }
        if self.mic.pending.is_some() {
            if matches!(c.op, Op::Release | Op::SetMute(true)) {
                let pending = self.mic.pending.take().unwrap();
                self.mic_reply(pending.reply, ResultCode::Cancelled).await;
            } else {
                if matches!(c.op, Op::Press | Op::Renew) {
                    self.mic_warn_refusal(
                        ResultCode::Busy,
                        c.revision == self.mic.authority.revision,
                    );
                }
                self.mic_reply(to, ResultCode::Busy).await;
                return;
            }
        }
        let enabled = self.mic_enabled();
        let consent = local || self.mic_parent_allowed(parent);
        let previous_expiry = self.mic.authority.grant.map(|g| g.expires_ns);
        let decision = self.mic.authority.decide(now, parent, enabled, consent, c);
        if decision.result != policy::ResultCode::Ok {
            if matches!(c.op, Op::Press | Op::Renew) {
                self.mic_warn_refusal(
                    result(decision.result),
                    c.revision == self.mic.authority.revision,
                );
            }
            if matches!(c.op, Op::Press) && local {
                self.mic.held = None;
            }
            self.mic_reply(to, result(decision.result)).await;
            return;
        }
        if c.op == Op::Renew {
            if let (Some(previous), Effect::Grant(grant)) = (previous_expiry, decision.effect) {
                if grant.expires_ns <= previous && !self.mic.warned_nonextending {
                    self.mic.warned_nonextending = true;
                    tracing::warn!(
                        remaining_ms = grant.expires_ns.saturating_sub(now) / 1_000_000,
                        pending = self.mic.pending.is_some(),
                        "microphone first renewal without a later deadline"
                    );
                }
            }
        }
        if local {
            if let Effect::SetMute(muted) = decision.effect {
                self.mic.authority.set_local_muted(muted);
            }
        }
        let previous = self.mic.endpoint.as_ref().is_none_or(|s| s.muted);
        let write = match decision.effect {
            Effect::SetMute(muted) => {
                let revoke = self.mic.authority.revoke();
                if self.mic_effect(revoke, now).await {
                    return;
                }
                self.mic.held = None;
                let Some(endpoint) = &self.mic.endpoint else {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                };
                let generation = endpoint.generation;
                let Some(w) = &self.worker else {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                };
                // Persistent state is a different native mode, never an artificial talk lease.
                if w.handle.configure_ptt(None, Vec::new()).await.is_err() {
                    self.mic_reply(to, ResultCode::Unsupported).await;
                    return;
                }
                self.mic.configured = None;
                w.handle.set_mic_mute(generation, muted).await
            }
            Effect::Grant(g) => {
                let Some(w) = &self.worker else {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                };
                let ttl = ((g.expires_ns.saturating_sub(host_ns().unwrap_or(u64::MAX))) / 1_000_000)
                    .min(250) as u32;
                if ttl == 0 {
                    self.mic_reply(to, ResultCode::Expired).await;
                    return;
                }
                w.handle.ptt_grant(g.id, ttl).await
            }
            Effect::Revoke(id) => {
                let Some(w) = &self.worker else {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                };
                w.handle.ptt_revoke(id).await
            }
            Effect::None => {
                self.mic_reply(to, ResultCode::Ok).await;
                return;
            }
        };
        match write {
            Ok(request) => {
                let worker_generation = super::generation(
                    self.worker.as_ref().map(|w| w.generation).unwrap_or(0),
                    self.worker_epoch,
                );
                self.mic.pending = Some(PendingMic {
                    reply: to,
                    worker_generation,
                    request,
                    deadline: self.now() + REQUEST_TIMEOUT_MS,
                    muted: if let Op::SetMute(m) = c.op {
                        Some(m)
                    } else {
                        None
                    },
                    previous,
                    endpoint_generation: self
                        .mic
                        .endpoint
                        .as_ref()
                        .map(|s| s.generation)
                        .unwrap_or(0),
                });
            }
            Err(error) => {
                if self.native_privacy_failure(&error) {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                }
                let effect = self.mic.authority.revoke();
                if self.mic_effect(effect, now).await {
                    return;
                }
                self.mic_reply(to, ResultCode::Unsupported).await;
            }
        }
        self.dirty = true;
    }
    async fn mic_effect(&mut self, effect: Effect, _now: u64) -> bool {
        let Some(w) = &self.worker else {
            return false;
        };
        let write = match effect {
            Effect::SetMute(muted) => {
                let Some(endpoint) = &self.mic.endpoint else {
                    return false;
                };
                w.handle.set_mic_mute(endpoint.generation, muted).await
            }
            Effect::Revoke(id) => w.handle.ptt_revoke(id).await,
            Effect::Grant(g) => {
                let ttl = (g.expires_ns.saturating_sub(host_ns().unwrap_or(u64::MAX)) / 1_000_000)
                    .min(250) as u32;
                if ttl == 0 {
                    return false;
                }
                w.handle.ptt_grant(g.id, ttl).await
            }
            Effect::None => return false,
        };
        match write {
            Err(error) => self.native_privacy_failure(&error),
            Ok(_) => false,
        }
    }
    pub(super) async fn mic_on_network(
        &mut self,
        parent: fsm::Parent,
        message: &net::AudioControl,
    ) {
        if !self
            .links
            .borrow()
            .peers
            .get(&NodeId::new(parent.peer))
            .is_some_and(|p| p.trusted && p.parent == parent && p.mic_support)
        {
            return;
        }
        match &message.body {
            Some(net::audio_control::Body::MicControl(c))
                if self.config.role == config::AudioRole::Listener =>
            {
                if self.config.group_id.is_none_or(|g| c.group_id != g.0) {
                    return;
                }
                let Ok(request) = c.request_id.as_slice().try_into() else {
                    return;
                };
                let op = match MicControlOp::try_from(c.op) {
                    Ok(MicControlOp::SetMute) => Op::SetMute(c.muted),
                    Ok(MicControlOp::Press) => Op::Press,
                    Ok(MicControlOp::Renew) => Op::Renew,
                    Ok(MicControlOp::Release) => Op::Release,
                    _ => return,
                };
                let press = if c.press_id.is_empty() {
                    [0; 16]
                } else {
                    let Ok(p) = c.press_id.as_slice().try_into() else {
                        return;
                    };
                    p
                };
                let Some(now) = host_ns() else {
                    return;
                };
                self.mic_apply(
                    now,
                    parent,
                    Control {
                        request,
                        revision: c.expected_revision,
                        op,
                        press,
                        challenge: c.challenge,
                        expires_ns: c.expires_ns,
                    },
                    Reply::Network(parent, c.request_id.clone()),
                )
                .await;
            }
            Some(net::audio_control::Body::MicState(s))
                if self.config.role == config::AudioRole::Source =>
            {
                if !self.mic_parent_allowed(parent)
                    || self.config.group_id.is_none_or(|g| s.group_id != g.0)
                {
                    return;
                }
                if self
                    .mic
                    .remote
                    .as_ref()
                    .is_some_and(|r| r.parent == parent && r.state.revision > s.revision)
                {
                    return;
                }
                let ms = self.now();
                if let Some(now) = host_ns() {
                    if let Some(press) = self.mic.update_remote(parent, s, ms, now) {
                        if self.mic_enabled() {
                            self.mic_renew(parent, press, now).await;
                        }
                    }
                } else {
                    self.mic.remote = Some(Remote {
                        parent,
                        state: s.clone(),
                    });
                }
                self.dirty = true;
            }
            Some(net::audio_control::Body::MicAck(a)) => {
                if !self.mic_parent_allowed(parent)
                    || self.config.group_id.is_none_or(|g| a.group_id != g.0)
                {
                    return;
                }
                let Ok(key) = <[u8; 16]>::try_from(a.request_id.as_slice()) else {
                    return;
                };
                let code = ResultCode::try_from(a.result).unwrap_or(ResultCode::Invalid);
                if code != ResultCode::Ok
                    && (self
                        .mic
                        .held
                        .as_ref()
                        .is_some_and(|held| held.parent == parent)
                        || self.mic.native_parent == Some(parent))
                {
                    let revision_matches = self.mic.remote.as_ref().is_some_and(|remote| {
                        remote.parent == parent && remote.state.revision == a.revision
                    });
                    self.mic_warn_refusal(code, revision_matches);
                }
                if self
                    .mic
                    .outbound
                    .get(&key)
                    .is_some_and(|(p, _, _)| *p == parent)
                {
                    if let Some((_, r, _)) = self.mic.outbound.remove(&key) {
                        if let Some(remote) = &mut self.mic.remote {
                            remote.acknowledge(parent, a.revision);
                        }
                        if code != ResultCode::Ok {
                            self.mic.held = None;
                        }
                        reply(&r, gui_result(code)).await;
                    }
                }
            }
            _ => {}
        }
    }
    pub(super) async fn mic_on_worker(&mut self, event: &AudioSupervisorEvent) {
        self.mic_on_parents_changed();
        if let Some(effect) = self.mic.privacy_effect.take() {
            if self.mic_effect(effect, host_ns().unwrap_or(u64::MAX)).await {
                return;
            }
        }
        match event {
            AudioSupervisorEvent::MuteSnapshot {
                generation,
                request_id,
                snapshot,
            } => {
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == *generation)
                {
                    return;
                }
                if self.config.role != config::AudioRole::Listener || !self.mic_configured() {
                    return;
                }
                let acquired = self.mic.acquire == Some((*generation, *request_id));
                if acquired {
                    self.mic.acquire = None;
                }
                if self
                    .mic
                    .endpoint
                    .as_ref()
                    .is_some_and(|s| s.generation > snapshot.generation)
                {
                    return;
                }
                let replaced = self
                    .mic
                    .endpoint
                    .as_ref()
                    .is_some_and(|s| s.generation != snapshot.generation);
                if self
                    .mic
                    .endpoint
                    .as_ref()
                    .is_none_or(|s| s.generation != snapshot.generation)
                {
                    self.mic.authority.changed();
                }
                self.mic.authority.observe(snapshot.muted);
                self.mic.endpoint = Some(snapshot.clone());
                self.dirty = true;
                if acquired || replaced {
                    self.mic_begin_recovery(*generation, snapshot).await;
                    return;
                }
                if self.mic.recovery.as_ref().is_some_and(|r| {
                    r.matches(
                        super::generation(*generation, self.worker_epoch),
                        *request_id,
                    )
                }) {
                    let recovery = self.mic.recovery.take().unwrap();
                    let result = recovery.result(snapshot);
                    self.mic.recovered = result == ResultCode::Ok;
                    self.mic.recovery_error = (result != ResultCode::Ok).then_some(result);
                    self.mic.authority.changed();
                }
                if self.mic.pending.as_ref().is_some_and(|p| {
                    p.worker_generation == super::generation(*generation, self.worker_epoch)
                        && p.request == *request_id
                        && p.muted.is_some()
                }) {
                    let p = self.mic.pending.take().unwrap();
                    let desired = p.muted.unwrap();
                    let allowed = match &p.reply {
                        Reply::Gui(_) => true,
                        Reply::Network(parent, _) => self.mic_parent_allowed(*parent),
                    };
                    let code = if snapshot.generation != p.endpoint_generation {
                        ResultCode::DeviceGone
                    } else if !allowed {
                        if let Some(w) = &self.worker {
                            if let Err(error) =
                                w.handle.set_mic_mute(snapshot.generation, true).await
                            {
                                if self.native_privacy_failure(&error) {
                                    self.mic_reply(p.reply, ResultCode::DeviceGone).await;
                                    return;
                                }
                            }
                        }
                        ResultCode::ConsentRequired
                    } else if desired != snapshot.muted {
                        ResultCode::Conflict
                    } else if matches!(&p.reply, Reply::Network(_, _)) {
                        self.mic.recovered = true;
                        self.mic.recovery_error = None;
                        ResultCode::Ok
                    } else {
                        let mut next = self.config.clone();
                        next.microphone.muted = desired;
                        match self.save(next.clone()).await {
                            Ok(()) => {
                                self.config = next;
                                self.mic.config_muted = Some(desired);
                                self.mic.authority.set_local_muted(desired);
                                self.mic.recovered = true;
                                self.mic.recovery_error = None;
                                ResultCode::Ok
                            }
                            Err(_) => {
                                if let Some(w) = &self.worker {
                                    if let Err(error) =
                                        w.handle.set_mic_mute(snapshot.generation, p.previous).await
                                    {
                                        if self.native_privacy_failure(&error) {
                                            self.mic_reply(p.reply, ResultCode::DeviceGone).await;
                                            return;
                                        }
                                    }
                                }
                                self.mic
                                    .authority
                                    .set_local_muted(self.config.microphone.muted);
                                ResultCode::Conflict
                            }
                        }
                    };
                    self.mic_reply(p.reply, code).await;
                }
            }
            AudioSupervisorEvent::PttEvent {
                generation,
                request_id,
                event,
            } => {
                if !self.mic_enabled() {
                    return;
                }
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == *generation)
                {
                    return;
                }
                if self.config.role == config::AudioRole::Listener && !self.mic_ready() {
                    return;
                }
                if event.grant_id != 0 {
                    self.mic.observe_grant(event, host_ns().unwrap_or(u64::MAX));
                }
                if event.grant_id == 0 {
                    let now = host_ns().unwrap_or(u64::MAX);
                    let valid = native_live(event, now);
                    if (self.mic.native_expires > now) != valid {
                        self.mic.authority.changed();
                    }
                    self.mic.native_expires = if valid { event.expires_at_ns } else { 0 };
                    if self.config.role == config::AudioRole::Listener {
                        if valid
                            && self.mic.authority.recovery_mode
                                != Some(policy::RecoveryMode::Persistent(false))
                        {
                            self.mic.authority.recovery_mode = Some(policy::RecoveryMode::Ptt);
                        }
                        if valid && self.mic.native_press.is_none() {
                            self.mic.native_press = Some(self.mic_next_id([0; 16]));
                        }
                        if !valid {
                            self.mic.native_press = None;
                        }
                    }
                    if self.config.role == config::AudioRole::Source && self.mic_enabled() {
                        if valid && event.kind == worker::PttEventKind::Pressed as i32 {
                            if let Some(parent) = self
                                .mic
                                .remote
                                .as_ref()
                                .map(|r| r.parent)
                                .filter(|p| self.mic_parent_allowed(*p))
                            {
                                let press = self.mic_next_id(parent.connection_id);
                                self.mic.native_press = Some(press);
                                self.mic.native_parent = Some(parent);
                                self.mic_source_control(parent, press, Op::Press).await;
                            }
                        } else if !valid {
                            if let (Some(press), Some(parent)) =
                                (self.mic.native_press.take(), self.mic.native_parent.take())
                            {
                                self.mic_release(parent, press, now).await;
                            }
                        }
                    }
                }
                if self.mic.pending.as_ref().is_some_and(|p| {
                    p.worker_generation == super::generation(*generation, self.worker_epoch)
                        && p.request == *request_id
                        && p.muted.is_none()
                }) {
                    let p = self.mic.pending.take().unwrap();
                    let code = ptt_result(event, host_ns().unwrap_or(u64::MAX));
                    if code != ResultCode::Ok {
                        let effect = self.mic.authority.revoke();
                        if self.mic_effect(effect, host_ns().unwrap_or(u64::MAX)).await {
                            return;
                        }
                        self.mic.held = None;
                    }
                    self.mic_reply(p.reply, code).await;
                }
                self.dirty = true;
            }
            _ => {}
        }
    }
    pub(super) async fn mic_tick(&mut self) {
        self.mic_on_parents_changed();
        if let Some(effect) = self.mic.privacy_effect.take() {
            if self.mic_effect(effect, host_ns().unwrap_or(u64::MAX)).await {
                return;
            }
        }
        let Some(now) = host_ns() else {
            return;
        };
        let ms = self.now();
        if self.mic.recovery.as_ref().is_some_and(|r| r.deadline <= ms) {
            self.mic.recovery = None;
            self.mic.recovered = false;
            self.mic.recovery_error = Some(ResultCode::Timeout);
            self.mic.authority.changed();
            self.dirty = true;
        }
        if let Some(p) = self.mic.pending.as_ref() {
            if p.deadline <= ms
                || matches!(&p.reply,Reply::Network(parent,_) if !self.mic_parent_allowed(*parent))
            {
                let p = self.mic.pending.take().unwrap();
                let effect = self.mic.authority.revoke();
                if self.mic_effect(effect, now).await {
                    return;
                }
                self.mic.held = None;
                self.mic_reply(p.reply, ResultCode::Timeout).await;
            }
        }
        let mut expired = [None; 16];
        for (slot, (key, _)) in expired.iter_mut().zip(self.mic.outbound.iter().filter(
            |(_, (parent, _, deadline))| *deadline <= ms || !self.mic_parent_allowed(*parent),
        )) {
            *slot = Some(*key);
        }
        for key in expired.into_iter().flatten() {
            if let Some((_, r, _)) = self.mic.outbound.remove(&key) {
                reply(&r, gui_result(ResultCode::Timeout)).await;
                self.mic.held = None;
            }
        }
        if !self.mic_enabled() {
            let effect = self.mic.authority.revoke();
            if self.mic_effect(effect, now).await {
                return;
            }
            if self.mic_configured() && self.session_locked {
                if let Some(pending) = self.mic.pending.take() {
                    self.mic_reply(pending.reply, ResultCode::Cancelled).await;
                }
                if let Some(held) = self.mic.held.take() {
                    self.mic_release(held.parent, held.press, now).await;
                }
                if let (Some(press), Some(parent)) =
                    (self.mic.native_press.take(), self.mic.native_parent.take())
                {
                    self.mic_release(parent, press, now).await;
                }
                self.mic.native_expires = 0;
                self.mic.remote = None;
                if !self.mic.lock_paused {
                    if let Some(w) = &self.worker {
                        if w.handle.configure_ptt(None, Vec::new()).await.is_err() {
                            // A live physical binding must not survive a failed lock transition.
                            self.privacy_restart = true;
                            self.worker_lost();
                            return;
                        }
                    }
                    self.mic.configured = None;
                    self.mic.authority.ptt_enabled = self.config.microphone.ptt_enabled;
                    if self.config.role == config::AudioRole::Listener && self.mic.mute_on_lock() {
                        if self.mic_effect(Effect::SetMute(true), now).await {
                            return;
                        }
                    }
                    self.mic.lock_paused = true;
                }
                if self.config.role == config::AudioRole::Listener
                    && self.mic.endpoint.is_none()
                    && self.mic.acquire.is_none()
                {
                    if let Some(w) = &self.worker {
                        if w.alive
                            && available(self.ready.as_ref(), worker::Capability::EndpointMute)
                        {
                            if let Ok(request) = w
                                .handle
                                .acquire_mic_mute(device(&self.config.microphone.capture_device))
                                .await
                            {
                                self.mic.acquire = Some((w.generation, request));
                            }
                        }
                    }
                }
                return;
            }
            self.mic.held = None;
            self.mic.native_expires = 0;
            self.mic.remote = None;
            if self.mic.endpoint.is_some()
                || self.mic.configured.is_some()
                || self.mic.acquire.is_some()
            {
                self.mic_stop().await;
            }
            self.mic.endpoint = None;
            self.mic.acquire = None;
            self.mic.native_press = None;
            self.mic.native_parent = None;
            self.mic.configured = None;
            return;
        }
        self.mic.lock_paused = false;
        let mut allowed = [None; 3];
        for (slot, peer) in allowed.iter_mut().zip(
            self.links
                .borrow()
                .peers
                .values()
                .filter(|p| self.mic_parent_allowed(p.parent)),
        ) {
            *slot = Some(peer.parent);
        }
        // Current-parent and consent pruning also runs synchronously after link changes.
        if self
            .mic
            .authority
            .grant
            .is_some_and(|g| g.expires_ns <= now)
            && !self.mic.warned_policy_expiry
        {
            self.mic.warned_policy_expiry = true;
            tracing::warn!(
                pending = self.mic.pending.is_some(),
                native_matches = self
                    .mic
                    .authority
                    .grant
                    .is_some_and(|g| g.id == self.mic.actual_grant),
                observed_muted = self.mic.endpoint.as_ref().map(|snapshot| snapshot.muted),
                overdue_ms = self
                    .mic
                    .authority
                    .grant
                    .map_or(0, |g| now.saturating_sub(g.expires_ns) / 1_000_000),
                "microphone first policy grant expiry"
            );
        }
        let effect = self.mic.authority.expire(now);
        if self.mic_effect(effect, now).await {
            return;
        }
        if self.config.role == config::AudioRole::Listener
            && self.mic.endpoint.is_none()
            && self.mic.acquire.is_none()
        {
            if let Some(w) = &self.worker {
                if w.alive && available(self.ready.as_ref(), worker::Capability::EndpointMute) {
                    if let Ok(request) = w
                        .handle
                        .acquire_mic_mute(device(&self.config.microphone.capture_device))
                        .await
                    {
                        self.mic.acquire = Some((w.generation, request));
                    }
                }
            }
        }
        self.mic.authority.ptt_enabled = self.config.microphone.ptt_enabled;
        self.mic.sync_config_intent(self.config.microphone.muted);
        if let Some(w) = self
            .worker
            .as_ref()
            .filter(|_| self.config.role == config::AudioRole::Source || self.mic_ready())
        {
            if !self.mic.configured.as_ref().is_some_and(|key| {
                key.0 == w.generation
                    && key.1 == self.config.microphone.ptt_enabled
                    && key.2 == self.config.microphone.ptt_hotkey
            }) {
                let key = (
                    w.generation,
                    self.config.microphone.ptt_enabled,
                    self.config.microphone.ptt_hotkey.clone(),
                );
                let reserved = self
                    .mic
                    .reserved
                    .clone()
                    .ok_or(ipc::audio::codes::AUDIO_UNAVAILABLE);
                let binding = if key.1 && !key.2.is_empty() {
                    hotkey(&key.2).map(Some)
                } else {
                    Ok(None)
                };
                match reserved.and_then(|reserved| binding.map(|binding| (binding, reserved))) {
                    Ok((binding, reserved))
                        if !binding.as_ref().is_some_and(|b| reserved.contains(b)) =>
                    {
                        if binding.is_none()
                            || available(self.ready.as_ref(), worker::Capability::GlobalPtt)
                        {
                            if w.handle.configure_ptt(binding, reserved).await.is_ok() {
                                self.mic.configured = Some(key);
                            }
                        } else if w.handle.configure_ptt(None, reserved).await.is_ok() {
                            self.mic.configured = Some(key);
                        }
                    }
                    _ => {
                        if w.handle.configure_ptt(None, Vec::new()).await.is_ok() {
                            self.mic.configured = Some(key);
                        }
                    }
                }
            }
        }
        if self.mic.held.as_ref().is_some_and(|h| {
            !self.gui_live(h.connection, &h.out)
                || (self.config.role == config::AudioRole::Source
                    && !self.mic_parent_allowed(h.parent))
        }) {
            if let Some(h) = self.mic.held.take() {
                self.mic_release(h.parent, h.press, now).await;
            }
        }
        if self.mic.held.as_ref().is_some_and(|h| h.next <= ms) {
            let (parent, press) = self.mic.held.as_ref().map(|h| (h.parent, h.press)).unwrap();
            if let Some(h) = &mut self.mic.held {
                h.next = ms + 100;
            }
            self.mic_renew(parent, press, now).await;
        }
        // Native observation is absolute host time, never a cached bool granted a new TTL.
        if self.config.role == config::AudioRole::Source {
            if self.mic.native_parent.is_some_and(|p| {
                !self.mic_parent_allowed(p)
                    || self.mic.remote.as_ref().is_none_or(|r| r.parent != p)
            }) {
                self.mic.native_press = None;
                self.mic.native_parent = None;
                self.mic.native_expires = 0;
            }
            if self.mic.native_expires <= now {
                if let (Some(press), Some(parent)) =
                    (self.mic.native_press.take(), self.mic.native_parent.take())
                {
                    self.mic_release(parent, press, now).await;
                }
            } else if ms.saturating_sub(self.mic.last_publish) >= 100 {
                if let (Some(press), Some(parent)) = (self.mic.native_press, self.mic.native_parent)
                {
                    self.mic_renew(parent, press, now).await;
                }
            }
        }
        if ms.saturating_sub(self.mic.last_publish) >= 100 {
            self.mic.last_publish = ms;
            if self.config.role == config::AudioRole::Listener {
                let (challenge, expires_ns) = self.mic.authority.challenge(now);
                for parent in allowed.into_iter().flatten() {
                    let grant = self
                        .mic
                        .authority
                        .grant
                        .filter(|g| g.id == self.mic.actual_grant && g.expires_ns > now);
                    let native = self
                        .mic
                        .native_press
                        .filter(|_| self.mic.native_expires > now);
                    let active = self.mic_ready()
                        && self.config.microphone.ptt_enabled
                        && (grant.is_some() || native.is_some())
                        && self.mic.endpoint.as_ref().is_some_and(|s| !s.muted);
                    self.mic_send(
                        parent,
                        net::audio_control::Body::MicState(net::MicState {
                            group_id: self.config.group_id.unwrap().0.to_vec(),
                            revision: self.mic.authority.revision,
                            muted: self.mic.endpoint.as_ref().is_some_and(|s| s.muted),
                            ptt_enabled: self.config.microphone.ptt_enabled,
                            ptt_active: active,
                            supported: self.mic_ready(),
                            reason: self.mic_state_reason() as i32,
                            controller_node_id: if active {
                                grant
                                    .map(|g| g.parent.peer.to_vec())
                                    .unwrap_or_else(|| self.local.as_bytes().to_vec())
                            } else {
                                Vec::new()
                            },
                            press_id: if active {
                                grant
                                    .map(|g| g.press)
                                    .or(native)
                                    .map(|p| p.to_vec())
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            },
                            challenge,
                            expires_ns,
                            desired_muted: self.mic.authority.desired_muted,
                        }),
                    )
                    .await;
                }
            }
        }
    }
    async fn mic_source_control(&mut self, parent: fsm::Parent, press: [u8; 16], op: Op) {
        if !self.mic_parent_allowed(parent) {
            return;
        }
        let request = self.mic_next_id(parent.connection_id);
        if let Some(r) = &self.mic.remote {
            if r.parent == parent {
                self.mic_send(
                    parent,
                    net::audio_control::Body::MicControl(net::MicControl {
                        group_id: r.state.group_id.clone(),
                        request_id: request.to_vec(),
                        expected_revision: r.state.revision,
                        op: wire_op(op) as i32,
                        muted: false,
                        press_id: press.to_vec(),
                        challenge: if op == Op::Release {
                            0
                        } else {
                            r.state.challenge
                        },
                        expires_ns: if op == Op::Release {
                            0
                        } else {
                            r.state.expires_ns
                        },
                    }),
                )
                .await;
            }
        }
    }
    async fn mic_release(&mut self, parent: fsm::Parent, press: [u8; 16], now: u64) {
        if self.config.role == config::AudioRole::Source {
            self.mic_source_control(parent, press, Op::Release).await;
        } else {
            if self
                .mic
                .authority
                .grant
                .is_some_and(|g| g.parent == parent && g.press == press)
            {
                let e = self.mic.authority.revoke();
                if self.mic_effect(e, now).await {
                    return;
                }
            }
        }
    }
    async fn mic_renew(&mut self, parent: fsm::Parent, press: [u8; 16], now: u64) {
        if self.config.role == config::AudioRole::Source {
            self.mic_source_control(parent, press, Op::Renew).await;
        } else {
            self.mic_on_parents_changed();
            if let Some(effect) = self.mic.privacy_effect.take() {
                if self.mic_effect(effect, now).await {
                    return;
                }
            }
            let (challenge, expires_ns) = self.mic.authority.challenge(now);
            let request = self.mic_next_id(parent.connection_id);
            let c = Control {
                request,
                revision: self.mic.authority.revision,
                op: Op::Renew,
                press,
                challenge,
                expires_ns,
            };
            let d = self.mic.authority.decide(now, parent, true, true, c);
            if self.mic_effect(d.effect, now).await {
                return;
            }
        }
    }
}
fn wire_op(op: Op) -> MicControlOp {
    match op {
        Op::SetMute(_) => MicControlOp::SetMute,
        Op::Press => MicControlOp::Press,
        Op::Renew => MicControlOp::Renew,
        Op::Release => MicControlOp::Release,
    }
}
fn hotkey(text: &str) -> Result<worker::PttHotkey, Code> {
    let h = config::Hotkey::parse(text).map_err(|_| ipc::audio::codes::AUDIO_UNAVAILABLE)?;
    let key = h.key();
    let bytes = key.as_bytes();
    let code = if bytes.len() == 1 && bytes[0].is_ascii_uppercase() {
        u32::from(bytes[0] - b'A') + 4
    } else if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        if bytes[0] == b'0' {
            39
        } else {
            u32::from(bytes[0] - b'1') + 30
        }
    } else if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if n <= 12 {
            57 + n
        } else {
            91 + n
        }
    } else {
        match key {
            "Enter" => 40,
            "Escape" => 41,
            "Backspace" => 42,
            "Tab" => 43,
            "Space" => 44,
            "Insert" => 73,
            "Home" => 74,
            "PageUp" => 75,
            "Delete" => 76,
            "End" => 77,
            "PageDown" => 78,
            "Right" => 79,
            "Left" => 80,
            "Down" => 81,
            "Up" => 82,
            _ => return Err(ipc::audio::codes::AUDIO_UNAVAILABLE),
        }
    };
    Ok(worker::PttHotkey {
        key_code: code,
        control: h.has(config::Modifier::Ctrl),
        alt: h.has(config::Modifier::Alt),
        shift: h.has(config::Modifier::Shift),
        meta: h.has(config::Modifier::Super),
    })
}
fn reserved_hotkeys(bindings: &config::HotkeyBindings) -> Result<Vec<worker::PttHotkey>, Code> {
    bindings
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(_, text)| hotkey(text))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hid_conversion_and_all_kvm_reservations() {
        assert_eq!(hotkey("Ctrl+Alt+Shift+F12").unwrap().key_code, 69);
        assert_eq!(hotkey("Ctrl+F24").unwrap().key_code, 115);
        assert_eq!(hotkey("Ctrl+0").unwrap().key_code, 39);
        assert_eq!(hotkey("Ctrl+Right").unwrap().key_code, 79);
        let bindings = config::HotkeyBindings::default();
        let reserved = reserved_hotkeys(&bindings).unwrap();
        assert_eq!(reserved.len(), 5);
        assert!(reserved.contains(&hotkey(&bindings.game_guard_toggle).unwrap()));
    }
    #[test]
    fn native_observation_expires_at_absolute_host_deadline() {
        let mut event = worker::PttEvent {
            kind: worker::PttEventKind::Pressed as i32,
            observed_at_ns: 100,
            expires_at_ns: 200,
            ..Default::default()
        };
        assert!(native_live(&event, 150));
        assert!(!native_live(&event, 200));
        assert!(!native_live(&event, 99));
        event.kind = worker::PttEventKind::Renewed as i32;
        event.observed_at_ns = 0;
        assert!(native_live(&event, 150));
        assert!(!native_live(&event, 201));
        event.expires_at_ns = policy::LEASE_NS + 151;
        assert!(!native_live(&event, 150));
    }
    #[test]
    fn persistent_unmute_and_stale_revoke_are_not_remute_requests() {
        let mut event = worker::PttEvent {
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::PersistentUnmute as i32,
            ..Default::default()
        };
        assert_eq!(ptt_result(&event, 100), ResultCode::Ok);
        event.kind = worker::PttEventKind::Noop as i32;
        event.reason = worker::PttReleaseReason::Revoked as i32;
        assert_eq!(ptt_result(&event, 100), ResultCode::Ok);
        event.kind = worker::PttEventKind::Refused as i32;
        event.reason = worker::PttReleaseReason::BackendFailed as i32;
        assert_eq!(ptt_result(&event, 100), ResultCode::DeviceGone);
        event.reason = worker::PttReleaseReason::PreMuted as i32;
        assert_eq!(ptt_result(&event, 100), ResultCode::Conflict);
    }
    #[test]
    fn recovery_requires_native_snapshot_matching_target_and_endpoint() {
        for muted in [false, true] {
            let recovery = Recovery {
                worker_generation: [1; 16],
                request: 2,
                endpoint_generation: 3,
                muted,
                deadline: 100,
            };
            assert!(recovery.matches([1; 16], 2));
            assert!(!recovery.matches([2; 16], 2));
            assert!(!recovery.matches([1; 16], 3));
            let mut snapshot = worker::MuteSnapshot {
                generation: 3,
                guard_active: true,
                muted,
                ..Default::default()
            };
            assert_eq!(recovery.result(&snapshot), ResultCode::Ok);
            snapshot.muted = !muted;
            assert_eq!(recovery.result(&snapshot), ResultCode::Conflict);
            snapshot.muted = muted;
            snapshot.guard_active = false;
            assert_eq!(recovery.result(&snapshot), ResultCode::DeviceGone);
            snapshot.guard_active = true;
            snapshot.generation = 4;
            assert_eq!(recovery.result(&snapshot), ResultCode::DeviceGone);
        }
    }
    #[test]
    fn unchanged_config_never_promotes_remote_intent_during_recovery() {
        let mut state = State::default();
        state.sync_config_intent(false);
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let control = Control {
            request: id(1, parent.connection_id),
            revision: state.authority.revision,
            op: Op::SetMute(false),
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        assert_eq!(
            state
                .authority
                .decide(0, parent, true, true, control)
                .effect,
            Effect::SetMute(false)
        );
        state.sync_config_intent(false);
        assert_eq!(state.authority.desired_muted_by, Some(parent));
        assert!(state.authority.recovery_muted());
        state.authority.reset();
        state.sync_config_intent(false);
        assert!(state.authority.recovery_muted());
        assert!(state.authority.desired_muted);
    }
    #[test]
    fn config_local_intent_and_initial_ptt_base_survive_recovery() {
        let mut state = State::default();
        state.sync_config_intent(false);
        state.authority.reset();
        state.sync_config_intent(false);
        assert!(!state.authority.recovery_muted());
        state.sync_config_intent(true);
        assert!(state.authority.recovery_muted());
        state.sync_config_intent(false);
        assert!(!state.authority.recovery_muted());
        let mut ptt = State::default();
        ptt.authority.ptt_enabled = true;
        ptt.sync_config_intent(false);
        assert!(ptt.authority.recovery_muted());
    }
    #[test]
    fn lock_closes_ptt_but_preserves_explicit_local_persistent_unmute() {
        let mut state = State::default();
        state.authority.ptt_enabled = true;
        assert!(state.mute_on_lock());
        state.authority.recovery_mode = Some(policy::RecoveryMode::Ptt);
        assert!(state.mute_on_lock());
        state.authority.set_local_muted(false);
        assert!(!state.mute_on_lock());
        state.authority.set_local_muted(true);
        assert!(state.mute_on_lock());
        state.authority.ptt_enabled = false;
        assert!(!state.mute_on_lock());
    }
    #[test]
    fn lock_withdrawal_remutes_remote_persistent_intent_even_without_ptt() {
        let mut state = State::default();
        state.authority.set_local_muted(false);
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let control = Control {
            request: id(1, parent.connection_id),
            revision: state.authority.revision,
            op: Op::SetMute(false),
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        state.authority.decide(0, parent, true, true, control);
        assert_eq!(state.authority.retain(|_| false), Effect::SetMute(true));
        assert!(state.authority.recovery_muted());
        assert!(state.authority.desired_muted);
    }
    #[test]
    fn unchanged_mute_ack_advances_revision_before_the_next_press() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut authority = Authority::default();
        authority.ptt_enabled = true;
        let mut remote = Remote {
            parent,
            state: net::MicState {
                revision: authority.revision,
                muted: true,
                ptt_enabled: true,
                ..Default::default()
            },
        };
        let mute = Control {
            request: id(1, parent.connection_id),
            revision: remote.state.revision,
            op: Op::SetMute(true),
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        assert_eq!(
            authority.decide(0, parent, true, true, mute).effect,
            Effect::SetMute(true)
        );
        authority.observe(true);
        assert_ne!(remote.state.revision, authority.revision);
        let observed = remote.state.clone();
        remote.acknowledge(parent, authority.revision);
        assert_eq!(remote.state.revision, authority.revision);
        assert_eq!(remote.state.muted, observed.muted);
        assert_eq!(remote.state.ptt_active, observed.ptt_active);
        assert_eq!(remote.state.expires_ns, observed.expires_ns);
        let (challenge, expires_ns) = authority.challenge(0);
        let press = Control {
            request: id(3, parent.connection_id),
            revision: remote.state.revision,
            op: Op::Press,
            press: id(2, parent.connection_id),
            challenge,
            expires_ns,
        };
        let decision = authority.decide(0, parent, true, true, press);
        assert_eq!(decision.result, policy::ResultCode::Ok);
        assert!(matches!(decision.effect, Effect::Grant(_)));
    }
    #[test]
    fn mic_ack_revision_is_monotonic_and_bound_to_its_parent() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut remote = Remote {
            parent,
            state: net::MicState {
                revision: 7,
                ..Default::default()
            },
        };
        remote.acknowledge(parent, 6);
        assert_eq!(remote.state.revision, 7);
        let replacement = fsm::Parent {
            generation: [4; 16],
            ..parent
        };
        remote.acknowledge(replacement, 9);
        assert_eq!(remote.state.revision, 7);
        remote.acknowledge(parent, 8);
        assert_eq!(remote.state.revision, 8);
    }
    #[test]
    fn persistent_unmute_noop_press_renews_and_releases_without_a_native_grant() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut state = State::default();
        state.authority.ptt_enabled = true;
        let mute = Control {
            request: id(1, parent.connection_id),
            revision: state.authority.revision,
            op: Op::SetMute(false),
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        state.authority.decide(0, parent, true, true, mute);
        state.authority.observe(false);
        let (challenge, expires_ns) = state.authority.challenge(0);
        let mut press = Control {
            request: id(3, parent.connection_id),
            revision: state.authority.revision,
            op: Op::Press,
            press: id(2, parent.connection_id),
            challenge,
            expires_ns,
        };
        let decision = state.authority.decide(0, parent, true, true, press);
        let Effect::Grant(grant) = decision.effect else {
            panic!("press must establish logical ownership");
        };
        let mut event = worker::PttEvent {
            grant_id: grant.id,
            kind: worker::PttEventKind::Noop as i32,
            reason: worker::PttReleaseReason::PersistentUnmute as i32,
            ..Default::default()
        };
        state.observe_grant(&event, 0);
        assert_eq!(ptt_result(&event, 0), ResultCode::Ok);
        for n in 1..=6 {
            let now = n * 100_000_000;
            let (challenge, expires_ns) = state.authority.challenge(now);
            press = Control {
                request: id(n + 3, parent.connection_id),
                revision: state.authority.revision,
                op: Op::Renew,
                challenge,
                expires_ns,
                ..press
            };
            let decision = state.authority.decide(now, parent, true, true, press);
            assert_eq!(decision.result, policy::ResultCode::Ok);
            assert!(matches!(decision.effect, Effect::Grant(g) if g.id == grant.id));
            state.observe_grant(&event, now);
            assert_eq!(state.actual_grant, 0);
            assert!(!state.authority.muted);
        }
        press.request = id(10, parent.connection_id);
        press.op = Op::Release;
        let decision = state
            .authority
            .decide(650_000_000, parent, true, true, press);
        assert_eq!(decision.result, policy::ResultCode::Ok);
        assert_eq!(decision.effect, Effect::Revoke(grant.id));
        event.reason = worker::PttReleaseReason::Revoked as i32;
        state.observe_grant(&event, 650_000_000);
        assert_eq!(ptt_result(&event, 650_000_000), ResultCode::Ok);
        assert!(!state.authority.desired_muted);
        assert!(!state.authority.muted);
        assert_eq!(state.actual_grant, 0);
        assert!(state.authority.grant.is_none());
        assert!(
            state.authority.recovery_muted(),
            "remote intent is not a recovery base"
        );
    }
    #[test]
    fn fresh_listener_state_renews_held_press_before_the_next_timer() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut authority = Authority::default();
        authority.ptt_enabled = true;
        let (challenge, expires_ns) = authority.challenge(0);
        let press = Control {
            request: id(2, parent.connection_id),
            revision: authority.revision,
            op: Op::Press,
            press: id(1, parent.connection_id),
            challenge,
            expires_ns,
        };
        authority.decide(50_000_000, parent, true, true, press);
        let mut source = State::default();
        source.remote = Some(Remote {
            parent,
            state: net::MicState {
                revision: authority.revision,
                challenge,
                expires_ns,
                supported: true,
                ptt_enabled: true,
                ..Default::default()
            },
        });
        let (out, _receive) = mpsc::channel(1);
        source.held = Some(Held {
            connection: [4; 16],
            out,
            press: press.press,
            parent,
            next: 250,
        });
        // The native observation advances authority after the initial admission ACK.
        authority.observe(false);
        let stale = Control {
            request: id(3, parent.connection_id),
            revision: source.remote.as_ref().unwrap().state.revision,
            op: Op::Renew,
            ..press
        };
        assert_eq!(
            authority
                .decide(150_000_000, parent, true, true, stale)
                .result,
            policy::ResultCode::StaleRevision
        );
        let (challenge, expires_ns) = authority.challenge(175_000_000);
        let state = net::MicState {
            revision: authority.revision,
            challenge,
            expires_ns,
            supported: true,
            ptt_enabled: true,
            ..Default::default()
        };
        assert_eq!(
            source.update_remote(parent, &state, 175, 175_000_000),
            Some(press.press)
        );
        assert_eq!(source.held.as_ref().unwrap().next, 275);
        let renewed = Control {
            request: id(4, parent.connection_id),
            revision: source.remote.as_ref().unwrap().state.revision,
            challenge,
            expires_ns,
            ..stale
        };
        let decision = authority.decide(175_000_000, parent, true, true, renewed);
        assert_eq!(decision.result, policy::ResultCode::Ok);
        assert!(matches!(decision.effect, Effect::Grant(g) if g.expires_ns == expires_ns));
        assert_eq!(source.update_remote(parent, &state, 176, 176_000_000), None);
        assert_eq!(source.held.as_ref().unwrap().next, 275);
        // An ACK may already have advanced revision; a newer challenge still renews.
        let refreshed = net::MicState {
            challenge: state.challenge + 1,
            expires_ns: state.expires_ns + policy::CHALLENGE_NS,
            ..state.clone()
        };
        assert_eq!(
            source.update_remote(parent, &refreshed, 200, 200_000_000),
            Some(press.press)
        );
        let unavailable = net::MicState {
            revision: refreshed.revision + 1,
            supported: false,
            ..refreshed.clone()
        };
        assert_eq!(
            source.update_remote(parent, &unavailable, 201, 201_000_000),
            None
        );
        let replacement = fsm::Parent {
            generation: [9; 16],
            ..parent
        };
        assert_eq!(
            source.update_remote(replacement, &refreshed, 202, 202_000_000),
            None
        );
    }
}
