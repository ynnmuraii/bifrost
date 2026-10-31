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
    Gui(Box<Request>),
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
enum PttOutcome {
    ReleaseComplete(PendingMic),
    Observe,
    Ignore,
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
const SAFETY_MUTE_RETRY_MS: u64 = 100;
struct SafetyMute {
    parent: fsm::Parent,
    group: [u8; 16],
    request: [u8; 16],
    revision: u64,
    next: u64,
    replace_request: bool,
}
impl SafetyMute {
    fn begin_retry(&mut self, now: u64) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now.saturating_add(SAFETY_MUTE_RETRY_MS);
        true
    }
    fn control(&self) -> net::MicControl {
        net::MicControl {
            group_id: self.group.to_vec(),
            request_id: self.request.to_vec(),
            expected_revision: self.revision,
            op: MicControlOp::SetMute as i32,
            muted: true,
            press_id: Vec::new(),
            challenge: 0,
            expires_ns: 0,
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
    pending_release: Option<PendingMic>,
    native_fence: u64,
    native_suppressed: bool,
    native_epoch: Option<u64>,
    native_epoch_floor: Option<u64>,
    native_epoch_exhausted: bool,
    native_status: Option<worker::PttStatus>,
    native_worker_generation: Option<[u8; 16]>,
    outbound: BTreeMap<[u8; 16], (fsm::Parent, Request, u64)>,
    remote: Option<Remote>,
    safety_mute: Option<SafetyMute>,
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
    fn pending_mute(&self, parent: fsm::Parent, request: [u8; 16]) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.muted == Some(true)
                && matches!(&pending.reply, Reply::Network(owner, id)
                    if *owner == parent && id.as_slice() == request.as_slice())
        })
    }
    fn acknowledge_safety_mute(
        &mut self,
        parent: fsm::Parent,
        request: [u8; 16],
        code: ResultCode,
    ) {
        if let Some(pending) = self
            .safety_mute
            .as_mut()
            .filter(|pending| pending.parent == parent && pending.request == request)
        {
            if matches!(code, ResultCode::Ok | ResultCode::ConsentRequired) {
                self.safety_mute = None;
            } else {
                pending.replace_request = true;
            }
        }
    }
    fn ptt_outcome(
        &mut self,
        worker_generation: [u8; 16],
        request: u64,
        event: &worker::PttEvent,
        now: Option<u64>,
    ) -> PttOutcome {
        // A correlated command outcome is not a native release observation or proof.
        if self.pending_release.as_ref().is_some_and(|pending| {
            pending.worker_generation == worker_generation && pending.request == request
        }) {
            return PttOutcome::ReleaseComplete(self.pending_release.take().unwrap());
        }
        if event.grant_id == 0 && !self.native_after_fence(event, request, now) {
            PttOutcome::Ignore
        } else {
            PttOutcome::Observe
        }
    }
    fn bind_native_worker(&mut self, generation: [u8; 16]) {
        if self.native_worker_generation == Some(generation) {
            return;
        }
        self.native_worker_generation = Some(generation);
        self.native_epoch = None;
        self.native_epoch_floor = None;
        self.native_epoch_exhausted = false;
        self.native_status = None;
        self.native_suppressed = true;
        self.native_fence = 0;
        self.native_press = None;
        self.native_parent = None;
        self.native_expires = 0;
    }
    fn take_holds(&mut self) -> [Option<(fsm::Parent, [u8; 16])>; 2] {
        let gui = self.held.take().map(|held| (held.parent, held.press));
        let native = self.native_parent.take().zip(self.native_press.take());
        self.native_expires = 0;
        [gui, native]
    }
    fn fence_holds(&mut self, now: u64) {
        self.native_fence = now;
        self.native_epoch_floor = self
            .native_epoch
            .unwrap_or(0)
            .max(self.native_epoch_floor.unwrap_or(0))
            .checked_add(1);
        self.native_epoch_exhausted |= self.native_epoch_floor.is_none();
        self.native_suppressed = true;
        self.take_holds();
    }
    fn observe_native_epoch(&mut self, epoch: u64) -> bool {
        if self.native_epoch_exhausted
            || self.native_epoch_floor.is_some_and(|floor| epoch < floor)
            || self.native_epoch.is_some_and(|current| epoch < current)
        {
            return false;
        }
        if self.native_epoch != Some(epoch) {
            self.native_epoch = Some(epoch);
            self.native_suppressed = true;
            self.native_press = None;
            self.native_parent = None;
            self.native_expires = 0;
        }
        true
    }
    fn observe_native_status(&mut self, status: &worker::PttStatus) -> bool {
        if !self.observe_native_epoch(status.safety_epoch) {
            return false;
        }
        if !status.registered || status.state == worker::PttArmingState::Unavailable as i32 {
            self.native_press = None;
            self.native_parent = None;
            self.native_expires = 0;
        }
        self.native_status = Some(*status);
        true
    }
    fn native_arming(&self) -> Option<i32> {
        let status = self.native_status.as_ref().filter(|status| {
            Some(status.safety_epoch) == self.native_epoch
                && !self.native_epoch_exhausted
                && self
                    .native_epoch_floor
                    .is_none_or(|floor| status.safety_epoch >= floor)
        })?;
        Some(
            if !status.registered || status.state == worker::PttArmingState::Unavailable as i32 {
                gui::AudioPttArmingState::Unavailable as i32
            } else if self.native_suppressed
                || self.native_fence == u64::MAX
                || status.state != worker::PttArmingState::Armed as i32
            {
                gui::AudioPttArmingState::NeedsPhysicalRelease as i32
            } else {
                gui::AudioPttArmingState::Armed as i32
            },
        )
    }
    fn native_unavailable_reason(&self) -> Option<i32> {
        if self.native_arming() != Some(gui::AudioPttArmingState::Unavailable as i32) {
            return None;
        }
        let status = self.native_status.as_ref()?;
        if status.state != worker::PttArmingState::Unavailable as i32 {
            return None;
        }
        match worker::PttUnavailableReason::try_from(status.reason) {
            Ok(worker::PttUnavailableReason::HostClockUnavailable) => {
                Some(gui::AudioPttUnavailableReason::HostClockUnavailable as i32)
            }
            Ok(worker::PttUnavailableReason::PortalFreshnessUnverifiable) => {
                Some(gui::AudioPttUnavailableReason::PortalFreshnessUnverifiable as i32)
            }
            Ok(worker::PttUnavailableReason::RegistrationUnavailable) => {
                Some(gui::AudioPttUnavailableReason::RegistrationUnavailable as i32)
            }
            _ => None,
        }
    }
    fn recover_native_clock_fence(&mut self, now: Option<u64>) {
        if self.native_fence != u64::MAX || self.native_suppressed || self.native_epoch_exhausted {
            return;
        }
        let armed = self.native_status.as_ref().is_some_and(|status| {
            Some(status.safety_epoch) == self.native_epoch
                && status.safety_epoch != 0
                && self
                    .native_epoch_floor
                    .is_none_or(|floor| status.safety_epoch >= floor)
                && status.registered
                && status.state == worker::PttArmingState::Armed as i32
        });
        if let (true, Some(now)) = (armed, now.filter(|now| *now != u64::MAX)) {
            self.native_fence = now;
        }
    }
    fn native_after_fence(
        &mut self,
        event: &worker::PttEvent,
        request_id: u64,
        now: Option<u64>,
    ) -> bool {
        if !self.observe_native_epoch(event.safety_epoch) {
            return false;
        }
        if event.kind == worker::PttEventKind::Released as i32 {
            self.native_suppressed = !(event.grant_id == 0
                && request_id == 0
                && event.safety_epoch != 0
                && matches!(
                    worker::PttReleaseReason::try_from(event.reason),
                    Ok(worker::PttReleaseReason::PhysicalRelease
                        | worker::PttReleaseReason::PortalRelease)
                ));
            self.recover_native_clock_fence(now);
        }
        match worker::PttEventKind::try_from(event.kind) {
            Ok(worker::PttEventKind::Pressed) => {
                self.native_arming() == Some(gui::AudioPttArmingState::Armed as i32)
                    && event.observed_at_ns > self.native_fence
            }
            Ok(worker::PttEventKind::Renewed) => {
                self.native_arming() == Some(gui::AudioPttArmingState::Armed as i32)
                    && self.native_press.is_some()
            }
            _ => true,
        }
    }
    fn take_activation(&mut self, mute: bool) -> Option<PendingMic> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| mute || pending.muted.is_none())
        {
            self.pending.take()
        } else {
            None
        }
    }
    fn gui_activation(&self, op: Op, revision: u64, current: u64) -> Result<(), ResultCode> {
        if matches!(op, Op::Release | Op::ReleaseAll | Op::SetMute(true)) {
            return Ok(());
        }
        if revision != current {
            return Err(ResultCode::StaleRevision);
        }
        if self.pending.is_some()
            || self.pending_release.is_some()
            || self.safety_mute.is_some()
            || self.outbound.len() >= 16
        {
            return Err(ResultCode::Busy);
        }
        Ok(())
    }
    fn set_local_intent(&mut self, muted: bool) {
        self.config_muted = Some(muted);
        self.authority.set_local_muted(muted);
    }
    fn remote_talk_generation(
        &self,
        parent: fsm::Parent,
        state: &net::MicState,
    ) -> std::cmp::Ordering {
        self.remote
            .as_ref()
            .filter(|remote| remote.parent == parent)
            .map_or(std::cmp::Ordering::Equal, |remote| {
                state.talk_generation.cmp(&remote.state.talk_generation)
            })
    }
    fn fence_remote_talk_generation(
        &mut self,
        parent: fsm::Parent,
        state: &net::MicState,
        now: u64,
    ) -> bool {
        if self.remote_talk_generation(parent, state) != std::cmp::Ordering::Greater {
            return false;
        }
        self.fence_holds(now);
        true
    }
    fn update_remote(
        &mut self,
        parent: fsm::Parent,
        state: &net::MicState,
        now_ms: u64,
        now_ns: u64,
    ) -> Option<[u8; 16]> {
        if self.remote.as_ref().is_some_and(|remote| {
            remote.parent == parent
                && (state.talk_generation < remote.state.talk_generation
                    || state.revision < remote.state.revision
                    || (state.revision == remote.state.revision
                        && (state.expires_ns < remote.state.expires_ns
                            || state.challenge < remote.state.challenge)))
        }) {
            return None;
        }
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
        if active && !matches {
            return;
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
    fn mic_bind_native_worker(&mut self) {
        if let Some(worker) = &self.worker {
            self.mic
                .bind_native_worker(super::generation(worker.generation, self.worker_epoch));
        }
    }
    pub(super) fn mic_safety_endpoint(&self) -> Option<u64> {
        if self.config.role != config::AudioRole::Listener {
            return None;
        }
        self.mic
            .endpoint
            .as_ref()
            .filter(|snapshot| snapshot.guard_active)
            .map(|snapshot| snapshot.generation)
    }
    pub(super) async fn mic_stop(&mut self) {
        self.clear_gui_safety_target();
        self.mic_bind_native_worker();
        let now = host_ns().unwrap_or(u64::MAX);
        self.mic.fence_holds(now);
        if self.mic_effect(Effect::ReleaseAll, now).await {
            return;
        }
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
                    self.native_privacy_failure(&error);
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
        self.mic_bind_native_worker();
        self.mic.recovered = false;
        self.mic.recovery_error = None;
        self.mic.recovery = None;
        self.mic.authority.revoke();
        self.mic.take_holds();
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
        if muted {
            self.mic.fence_holds(host_ns().unwrap_or(u64::MAX));
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
        if self.mic.safety_mute.as_ref().is_some_and(|pending| {
            self.config.role != config::AudioRole::Source
                || !self.mic_configured()
                || self
                    .config
                    .group_id
                    .is_none_or(|group| group.0 != pending.group)
                || !self.mic_parent_allowed(pending.parent)
        }) {
            self.mic.safety_mute = None;
        }
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
                self.mic_publish_state(parent, host_ns().unwrap_or(u64::MAX))
                    .await;
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
                        native_ptt_arming: self.mic.native_arming(),
                        native_ptt_unavailable_reason: self.mic.native_unavailable_reason(),
                    };
                }
            }
            return gui::AudioMicrophoneState {
                muted: false,
                native_ptt_arming: self.mic.native_arming(),
                native_ptt_unavailable_reason: self.mic.native_unavailable_reason(),
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
            native_ptt_arming: self.mic.native_arming(),
            native_ptt_unavailable_reason: self.mic.native_unavailable_reason(),
        }
    }
    pub(super) fn mic_reset(&mut self) {
        self.mic.authority.reset();
        self.mic.lock_paused = false;
        self.mic.endpoint = None;
        self.publish_gui_safety_target();
        self.mic.acquire = None;
        self.mic.configured = None;
        self.mic.take_holds();
        self.mic.native_suppressed = true;
        self.mic.native_status = None;
        self.mic.remote = None;
        self.mic.actual_grant = 0;
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
        if let Some(pending) = self.mic.pending_release.take() {
            if let Reply::Gui(request) = pending.reply {
                let _ = request.out.try_send((request.request_id, error.clone()));
            }
        }
        for (_, (_, r, _)) in std::mem::take(&mut self.mic.outbound) {
            let _ = r.out.try_send((r.request_id, error.clone()));
        }
        self.dirty = true;
    }
    async fn mic_save(&self, config: config::AudioConfig) -> Result<(), Code> {
        tracing::info!(
            desired_muted = config.microphone.muted,
            "microphone configuration persistence started"
        );
        self.save(config).await
    }
    async fn mic_retry_safety_mute(&mut self) {
        self.mic_on_parents_changed();
        let now = self.now();
        let Some(pending) = self.mic.safety_mute.as_mut() else {
            return;
        };
        if !pending.begin_retry(now) {
            return;
        }
        let parent = pending.parent;
        let request = if pending.replace_request {
            self.mic_next_id(parent.connection_id)
        } else {
            pending.request
        };
        let pending = self.mic.safety_mute.as_mut().unwrap();
        pending.request = request;
        pending.replace_request = false;
        let control = pending.control();
        self.mic_send(parent, net::audio_control::Body::MicControl(control))
            .await;
    }
    pub(super) async fn mic_gui_safety(&mut self, mute: bool) {
        self.mic_bind_native_worker();
        self.mic_on_parents_changed();
        let now = host_ns().unwrap_or(u64::MAX);
        self.mic.fence_holds(now);
        self.mic.authority.release_all();
        if mute {
            self.config.microphone.muted = true;
            self.mic.set_local_intent(true);
            self.mic.recovery = None;
            if self.config.role == config::AudioRole::Source && self.mic.safety_mute.is_none() {
                if let Some(((parent, revision), group)) = self
                    .mic
                    .remote
                    .as_ref()
                    .map(|remote| (remote.parent, remote.state.revision))
                    .filter(|(parent, _)| self.mic_parent_allowed(*parent))
                    .zip(self.config.group_id)
                {
                    let request = self.mic_next_id(parent.connection_id);
                    self.mic.safety_mute = Some(SafetyMute {
                        parent,
                        group: group.0,
                        request,
                        revision,
                        next: self.now(),
                        replace_request: false,
                    });
                }
            }
        }
        if let Some(pending) = self.mic.take_activation(mute) {
            self.mic_reply(pending.reply, ResultCode::Cancelled).await;
        }
        self.mic_retry_safety_mute().await;
        if !self.mic_effect(Effect::ReleaseAll, now).await {
            if self.config.role == config::AudioRole::Source {
                if !mute {
                    if let Some(parent) = self.mic.remote.as_ref().map(|remote| remote.parent) {
                        self.mic_source_control(parent, [0; 16], Op::ReleaseAll)
                            .await;
                    }
                }
            } else if mute {
                self.mic_effect(Effect::SetMute(true), now).await;
            }
        }
        if mute && self.mic_save(self.config.clone()).await.is_err() {
            tracing::warn!("microphone safety mute persistence failed");
        }
        self.dirty = true;
    }
    pub(super) async fn mic_on_gui(&mut self, request: Request) {
        let (op, revision) = match &request.message {
            gui::gui_frame::Msg::SetMicrophoneMute(v) => {
                (Op::SetMute(v.muted), v.expected_revision)
            }
            gui::gui_frame::Msg::SetPtt(v) => (
                if v.pressed { Op::Press } else { Op::ReleaseAll },
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
        if let Err(code) = self.mic.gui_activation(op, revision, self.revision) {
            reply(&request, gui_result(code)).await;
            return;
        }
        let connection = request.connection_id;
        if op == Op::Press && self.mic.held.is_some() {
            reply(&request, gui_result(ResultCode::Conflict)).await;
            return;
        }
        let listener_now = if self.config.role == config::AudioRole::Listener {
            let admissible = self.mic_ready()
                || op == Op::ReleaseAll
                || (op == Op::SetMute(true)
                    && self
                        .mic
                        .endpoint
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.guard_active));
            if !admissible {
                reply(&request, gui_result(self.mic_state_reason())).await;
                return;
            }
            let Some(now) = host_ns() else {
                reply(&request, gui_result(ResultCode::Disabled)).await;
                return;
            };
            Some(now)
        } else {
            None
        };
        let parent = if self.config.role == config::AudioRole::Listener {
            self.mic_local_parent(connection)
        } else {
            let Some(remote) = &self.mic.remote else {
                reply(&request, gui_result(ResultCode::DeviceGone)).await;
                return;
            };
            let parent = remote.parent;
            if !self.mic_parent_allowed(parent) {
                reply(&request, gui_result(ResultCode::ConsentRequired)).await;
                return;
            }
            parent
        };
        if self.mic.outbound.len() >= 16 {
            if let Some((_, (_, r, _))) = self.mic.outbound.pop_first() {
                reply(&r, gui_result(ResultCode::Cancelled)).await;
            }
        }
        let press = if op == Op::Press {
            self.mic_next_id(connection)
        } else {
            self.mic.held.as_ref().map(|h| h.press).unwrap_or([0; 16])
        };
        let request_id = self.mic_next_id(connection);
        if matches!(op, Op::SetMute(_) | Op::ReleaseAll) {
            self.mic.take_holds();
        }
        if op == Op::Press {
            self.mic.held = Some(Held {
                connection,
                out: request.out.clone(),
                press,
                parent,
                next: self.now() + 100,
            });
        } else if op == Op::ReleaseAll {
            self.mic.held = None;
        }
        if self.config.role == config::AudioRole::Source {
            let Some(remote) = &self.mic.remote else {
                return;
            };
            let c = net::MicControl {
                group_id: remote.state.group_id.clone(),
                request_id: request_id.to_vec(),
                expected_revision: remote.state.revision,
                op: wire_op(op) as i32,
                muted: matches!(op, Op::SetMute(true)),
                press_id: if matches!(op, Op::SetMute(_) | Op::ReleaseAll) {
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
            let now = listener_now.unwrap();
            let (challenge, expires_ns) = self.mic.authority.challenge(now);
            let c = Control {
                request: request_id,
                revision: self.mic.authority.revision,
                op,
                press,
                challenge,
                expires_ns,
            };
            self.mic_apply(now, parent, c, Reply::Gui(Box::new(request)))
                .await;
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
        if c.op == Op::SetMute(true) && self.mic.pending_mute(parent, c.request) {
            return;
        }
        self.mic_on_parents_changed();
        if let Some(effect) = self.mic.privacy_effect.take() {
            if self.mic_effect(effect, now).await {
                return;
            }
        }
        let local = parent.peer == *self.local.as_bytes();
        if !self.mic_ready() && !matches!(c.op, Op::Release | Op::ReleaseAll) {
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
        if self.mic.pending.is_some() && c.op != Op::ReleaseAll {
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
        let mut decision = self.mic.authority.decide(now, parent, enabled, consent, c);
        if c.op == Op::SetMute(true)
            && decision.effect == Effect::None
            && decision.result == policy::ResultCode::Ok
            && !self.mic_ready()
        {
            decision.effect = Effect::SetMute(true);
        }
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
        if matches!(decision.effect, Effect::ReleaseAll | Effect::SetMute(true)) {
            self.mic_bind_native_worker();
            self.mic.fence_holds(now);
            if let Some(pending) = self.mic.take_activation(false) {
                self.mic_reply(pending.reply, ResultCode::Cancelled).await;
            }
        }
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
            Effect::ReleaseAll => {
                let Some(w) = &self.worker else {
                    self.mic_reply(to, ResultCode::DeviceGone).await;
                    return;
                };
                w.handle.ptt_release_all().await
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
                let pending = PendingMic {
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
                };
                if c.op == Op::ReleaseAll {
                    if let Some(previous) = self.mic.pending_release.replace(pending) {
                        self.mic_reply(previous.reply, ResultCode::Cancelled).await;
                    }
                } else {
                    self.mic.pending = Some(pending);
                }
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
        self.mic_bind_native_worker();
        let Some(w) = &self.worker else {
            return false;
        };
        let write = match effect {
            Effect::SetMute(muted) => {
                let Some(endpoint) = &self.mic.endpoint else {
                    return false;
                };
                let generation = endpoint.generation;
                if muted {
                    self.mic.fence_holds(host_ns().unwrap_or(u64::MAX));
                }
                w.handle.set_mic_mute(generation, muted).await
            }
            Effect::Revoke(id) => w.handle.ptt_revoke(id).await,
            Effect::ReleaseAll => w.handle.ptt_release_all().await,
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
                    Ok(MicControlOp::ReleaseAll) => Op::ReleaseAll,
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
                if self.mic.remote.as_ref().is_some_and(|r| {
                    r.parent == parent
                        && (r.state.revision > s.revision
                            || r.state.talk_generation > s.talk_generation)
                }) {
                    return;
                }
                let now = host_ns().unwrap_or(u64::MAX);
                self.mic_bind_native_worker();
                if self.mic.fence_remote_talk_generation(parent, s, now) {
                    // A safety event changes activation CAS even when visible facts are identical.
                    self.last_status = None;
                    self.dirty = true;
                    if self.mic_effect(Effect::ReleaseAll, now).await {
                        return;
                    }
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
                self.mic.acknowledge_safety_mute(parent, key, code);
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
            AudioSupervisorEvent::PttStatus { generation, status } => {
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.alive && worker.generation == *generation)
                {
                    return;
                }
                self.mic_bind_native_worker();
                if self.mic.observe_native_status(status) {
                    self.mic.recover_native_clock_fence(host_ns());
                    self.dirty = true;
                }
            }
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
                self.publish_gui_safety_target();
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
                        self.mic_bind_native_worker();
                        self.mic.fence_holds(host_ns().unwrap_or(u64::MAX));
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
                        let safety_generation = self.applied_gui_safety;
                        let mut next = self.config.clone();
                        next.microphone.muted = desired;
                        let saved = self.mic_save(next.clone()).await;
                        self.sync_gui_safety().await;
                        let superseded;
                        {
                            // Order the local commit against a concurrently raised mute latch.
                            let latch = self.gui_safety.borrow();
                            superseded = !desired && latch.pending(safety_generation) == Some(true);
                            if !superseded {
                                if saved.is_ok() || desired {
                                    self.config = next;
                                    self.mic.set_local_intent(desired);
                                    self.mic.recovered = true;
                                    self.mic.recovery_error = None;
                                } else {
                                    self.mic
                                        .authority
                                        .set_local_muted(self.config.microphone.muted);
                                }
                            }
                        }
                        if superseded {
                            self.sync_gui_safety().await;
                            if let Err(error) = saved {
                                if let Reply::Gui(request) = p.reply {
                                    reply(&request, Err(error)).await;
                                }
                            } else {
                                self.mic_reply(p.reply, ResultCode::Cancelled).await;
                            }
                            return;
                        }
                        if saved.is_err() && !desired && p.previous {
                            self.mic_bind_native_worker();
                            self.mic.fence_holds(host_ns().unwrap_or(u64::MAX));
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
                        }
                        if let Err(error) = saved {
                            if let Reply::Gui(request) = p.reply {
                                reply(&request, Err(error)).await;
                            }
                            return;
                        }
                        ResultCode::Ok
                    };
                    self.mic_reply(p.reply, code).await;
                }
            }
            AudioSupervisorEvent::PttEvent {
                generation,
                request_id,
                event,
            } => {
                if !self
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.alive && w.generation == *generation)
                {
                    return;
                }
                self.mic_bind_native_worker();
                match self.mic.ptt_outcome(
                    super::generation(*generation, self.worker_epoch),
                    *request_id,
                    event,
                    host_ns(),
                ) {
                    PttOutcome::ReleaseComplete(pending) => {
                        self.mic_reply(
                            pending.reply,
                            ptt_result(event, host_ns().unwrap_or(u64::MAX)),
                        )
                        .await;
                        self.dirty = true;
                        return;
                    }
                    PttOutcome::Ignore => return,
                    PttOutcome::Observe => {}
                }
                if event.grant_id == 0 {
                    self.dirty = true;
                }
                if !self.mic_enabled() {
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
        self.mic_retry_safety_mute().await;
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
        if self
            .mic
            .pending_release
            .as_ref()
            .is_some_and(|pending| pending.deadline <= ms)
        {
            let pending = self.mic.pending_release.take().unwrap();
            self.mic_reply(pending.reply, ResultCode::Timeout).await;
        }
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
                for (parent, press) in self.mic.take_holds().into_iter().flatten() {
                    self.mic_release(parent, press, now).await;
                }
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
                    if self.config.role == config::AudioRole::Listener
                        && self.mic.mute_on_lock()
                        && self.mic_effect(Effect::SetMute(true), now).await
                    {
                        return;
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
            self.publish_gui_safety_target();
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
                for parent in allowed.into_iter().flatten() {
                    self.mic_publish_state(parent, now).await;
                }
            }
        }
    }
    async fn mic_publish_state(&mut self, parent: fsm::Parent, now: u64) {
        if !self.mic_parent_allowed(parent) {
            return;
        }
        let Some(group) = self.config.group_id else {
            return;
        };
        let (challenge, expires_ns) = self.mic.authority.challenge(now);
        let grant = self
            .mic
            .authority
            .grant
            .filter(|grant| grant.id == self.mic.actual_grant && grant.expires_ns > now);
        let native = self
            .mic
            .native_press
            .filter(|_| self.mic.native_expires > now);
        let active = self.mic_ready()
            && self.config.microphone.ptt_enabled
            && (grant.is_some() || native.is_some())
            && self
                .mic
                .endpoint
                .as_ref()
                .is_some_and(|snapshot| !snapshot.muted);
        self.mic_send(
            parent,
            net::audio_control::Body::MicState(net::MicState {
                group_id: group.0.to_vec(),
                revision: self.mic.authority.revision,
                muted: self
                    .mic
                    .endpoint
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.muted),
                ptt_enabled: self.config.microphone.ptt_enabled,
                ptt_active: active,
                supported: self.mic_ready(),
                reason: self.mic_state_reason() as i32,
                controller_node_id: if active {
                    grant
                        .map(|grant| grant.parent.peer.to_vec())
                        .unwrap_or_else(|| self.local.as_bytes().to_vec())
                } else {
                    Vec::new()
                },
                press_id: if active {
                    grant
                        .map(|grant| grant.press)
                        .or(native)
                        .map(|press| press.to_vec())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                },
                challenge,
                expires_ns,
                desired_muted: self.mic.authority.desired_muted,
                talk_generation: self.mic.authority.talk_generation,
            }),
        )
        .await;
    }
    async fn mic_source_control(&mut self, parent: fsm::Parent, press: [u8; 16], op: Op) {
        if matches!(op, Op::Press | Op::Renew) && self.mic.safety_mute.is_some() {
            return;
        }
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
                        muted: matches!(op, Op::SetMute(true)),
                        press_id: if matches!(op, Op::SetMute(_) | Op::ReleaseAll) {
                            Vec::new()
                        } else {
                            press.to_vec()
                        },
                        challenge: if matches!(op, Op::Press | Op::Renew) {
                            r.state.challenge
                        } else {
                            0
                        },
                        expires_ns: if matches!(op, Op::Press | Op::Renew) {
                            r.state.expires_ns
                        } else {
                            0
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
                self.mic_effect(e, now).await;
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
            self.mic_effect(d.effect, now).await;
        }
    }
}
fn wire_op(op: Op) -> MicControlOp {
    match op {
        Op::SetMute(_) => MicControlOp::SetMute,
        Op::Press => MicControlOp::Press,
        Op::Renew => MicControlOp::Renew,
        Op::Release => MicControlOp::Release,
        Op::ReleaseAll => MicControlOp::ReleaseAll,
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
    fn safety_mute() -> SafetyMute {
        SafetyMute {
            parent: fsm::Parent {
                peer: [1; 32],
                connection_id: [2; 16],
                generation: [3; 16],
            },
            group: [4; 16],
            request: id(1, [2; 16]),
            revision: 7,
            next: 0,
            replace_request: false,
        }
    }
    #[test]
    fn safety_mute_retry_is_constant_state_same_id_and_at_most_once_per_100ms() {
        let mut pending = safety_mute();
        let request = pending.request;
        for now in [0, 100, 200] {
            assert!(pending.begin_retry(now));
            assert!(!pending.begin_retry(now));
            assert!(!pending.begin_retry(now + 99));
            assert_eq!(pending.next, now + 100);
            assert_eq!(pending.request, request);
            let control = pending.control();
            assert_eq!(control.request_id, request);
            assert_eq!(control.group_id, pending.group);
            assert_eq!(control.expected_revision, 7);
            assert_eq!(control.op, MicControlOp::SetMute as i32);
            assert!(control.muted);
            assert!(control.press_id.is_empty());
            assert_eq!(control.challenge, 0);
            assert_eq!(control.expires_ns, 0);
        }
    }
    #[test]
    fn safety_mute_requires_matching_ack_and_survives_own_child_reset() {
        let mut state = State::default();
        let pending = safety_mute();
        let parent = pending.parent;
        let request = pending.request;
        state.safety_mute = Some(pending);
        state.acknowledge_safety_mute(parent, id(2, parent.connection_id), ResultCode::Ok);
        let replacement = fsm::Parent {
            generation: [5; 16],
            ..parent
        };
        state.acknowledge_safety_mute(replacement, request, ResultCode::Ok);
        assert!(state.safety_mute.is_some());
        state.update_remote(
            parent,
            &net::MicState {
                group_id: [4; 16].to_vec(),
                revision: 7,
                muted: true,
                ..Default::default()
            },
            0,
            0,
        );
        assert!(state.safety_mute.is_some());
        assert_eq!(state.gui_activation(Op::Press, 7, 7), Err(ResultCode::Busy));
        assert_eq!(
            state.gui_activation(Op::SetMute(false), 7, 7),
            Err(ResultCode::Busy)
        );
        assert_eq!(state.gui_activation(Op::SetMute(true), 0, 7), Ok(()));
        assert_eq!(state.gui_activation(Op::ReleaseAll, 0, 7), Ok(()));
        state.bind_native_worker([6; 16]);
        state.authority.reset();
        state.remote = None;
        state.bind_native_worker([7; 16]);
        assert_eq!(state.safety_mute.as_ref().unwrap().parent, parent);
        assert_eq!(state.safety_mute.as_ref().unwrap().request, request);
        assert_eq!(state.safety_mute.as_ref().unwrap().group, [4; 16]);
        state.acknowledge_safety_mute(parent, request, ResultCode::DeviceGone);
        assert!(state.safety_mute.as_ref().unwrap().replace_request);
        state.acknowledge_safety_mute(parent, request, ResultCode::Ok);
        assert!(state.safety_mute.is_none());
        assert_eq!(state.gui_activation(Op::Press, 7, 7), Ok(()));
        state.safety_mute = Some(safety_mute());
        state.acknowledge_safety_mute(parent, request, ResultCode::ConsentRequired);
        assert!(state.safety_mute.is_none());
    }
    #[test]
    fn pending_duplicate_safety_mute_cannot_complete_before_native_outcome() {
        let pending = safety_mute();
        let parent = pending.parent;
        let control = Control {
            request: pending.request,
            revision: 7,
            op: Op::SetMute(true),
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        let mut state = State::default();
        assert_eq!(
            state
                .authority
                .decide(0, parent, true, true, control)
                .effect,
            Effect::SetMute(true)
        );
        state.pending = Some(PendingMic {
            reply: Reply::Network(parent, control.request.to_vec()),
            worker_generation: [5; 16],
            request: 9,
            deadline: 1000,
            muted: Some(true),
            previous: false,
            endpoint_generation: 1,
        });
        assert!(state.pending_mute(parent, control.request));
        assert!(!state.pending_mute(parent, id(2, parent.connection_id)));
        assert!(!state.pending_mute(
            fsm::Parent {
                generation: [6; 16],
                ..parent
            },
            control.request
        ));
        assert_eq!(
            state
                .authority
                .decide(1, parent, true, true, control)
                .effect,
            Effect::None,
            "the accepted decision cache alone is not native confirmation"
        );
        assert_eq!(state.pending.as_ref().unwrap().request, 9);
        state.pending = None;
        state
            .authority
            .complete(parent, control.request, policy::ResultCode::DeviceGone);
        assert_eq!(
            state
                .authority
                .decide(2, parent, true, true, control)
                .result,
            policy::ResultCode::DeviceGone
        );
        let retry = Control {
            request: id(2, parent.connection_id),
            ..control
        };
        assert_eq!(
            state.authority.decide(3, parent, true, true, retry).effect,
            Effect::SetMute(true)
        );
    }
    #[test]
    fn correlated_release_ack_completes_without_native_epoch_proof_or_persistent_cancellation() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let worker_generation = [4; 16];
        let mut state = State::default();
        state.bind_native_worker(worker_generation);
        state.set_local_intent(false);
        state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease,
        ));
        state.fence_holds(100);
        state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
        let pending = |request, muted| PendingMic {
            reply: Reply::Network(parent, id(request, parent.connection_id).to_vec()),
            worker_generation,
            request,
            deadline: 1000,
            muted,
            previous: false,
            endpoint_generation: 5,
        };
        state.pending = Some(pending(8, Some(false)));
        state.pending_release = Some(pending(7, None));
        // Owned-guard ReleaseAll uses the remote command-outcome epoch, not a native proof epoch.
        let ack = worker::PttEvent {
            grant_id: 0,
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::Revoked as i32,
            safety_epoch: 0,
            ..Default::default()
        };
        assert!(!state.native_after_fence(&ack, 7, Some(200)));
        for (generation, request) in [([6; 16], 7), (worker_generation, 0), (worker_generation, 8)]
        {
            assert!(matches!(
                state.ptt_outcome(generation, request, &ack, Some(200)),
                PttOutcome::Ignore
            ));
        }
        let PttOutcome::ReleaseComplete(completed) =
            state.ptt_outcome(worker_generation, 7, &ack, Some(200))
        else {
            panic!("correlated release must complete before native epoch filtering");
        };
        assert_eq!(completed.request, 7);
        assert_eq!(ptt_result(&ack, 200), ResultCode::Ok);
        assert!(state.pending_release.is_none());
        assert_eq!(state.pending.as_ref().unwrap().request, 8);
        assert_eq!(state.pending.as_ref().unwrap().muted, Some(false));
        assert_eq!(state.native_epoch, Some(1));
        assert_eq!(state.native_epoch_floor, Some(1));
        assert_eq!(state.native_fence, 100);
        assert!(state.native_suppressed);
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        assert_eq!(state.config_muted, Some(false));
        assert!(!state.authority.recovery_muted());
        assert!(state.endpoint.is_none());
        assert!(matches!(
            state.ptt_outcome(worker_generation, 0, &ack, Some(200)),
            PttOutcome::Ignore
        ));
    }
    #[test]
    fn clock_fence_recovers_in_either_proof_status_order_without_a_second_release() {
        for proof in [
            worker::PttReleaseReason::PhysicalRelease,
            worker::PttReleaseReason::PortalRelease,
        ] {
            for proof_first in [false, true] {
                for clock_ready in [false, true] {
                    let mut state = State::default();
                    state.bind_native_worker([1; 16]);
                    state.observe_native_status(&arming_status(
                        0,
                        worker::PttArmingState::NeedsPhysicalRelease,
                    ));
                    state.fence_holds(u64::MAX);
                    state.observe_native_status(&arming_status(
                        1,
                        worker::PttArmingState::NeedsPhysicalRelease,
                    ));
                    let mut event = worker::PttEvent {
                        kind: worker::PttEventKind::Released as i32,
                        reason: worker::PttReleaseReason::Revoked as i32,
                        safety_epoch: 1,
                        ..Default::default()
                    };
                    state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
                    state.recover_native_clock_fence(Some(200));
                    assert_eq!(state.native_fence, u64::MAX);
                    assert!(state.native_after_fence(&event, 0, Some(200)));
                    assert_eq!(state.native_fence, u64::MAX);
                    event.reason = proof as i32;
                    assert!(state.native_after_fence(&event, 7, Some(200)));
                    assert_eq!(state.native_fence, u64::MAX);
                    event.safety_epoch = 0;
                    assert!(!state.native_after_fence(&event, 0, Some(200)));
                    assert_eq!(state.native_fence, u64::MAX);
                    event.safety_epoch = 1;
                    state.observe_native_status(&arming_status(
                        1,
                        worker::PttArmingState::NeedsPhysicalRelease,
                    ));
                    let clock = clock_ready.then_some(200);
                    if proof_first {
                        assert!(state.native_after_fence(&event, 0, clock));
                        assert_eq!(state.native_fence, u64::MAX);
                        assert_ne!(
                            state.native_arming(),
                            Some(gui::AudioPttArmingState::Armed as i32)
                        );
                        state.recover_native_clock_fence(Some(200));
                        assert_eq!(state.native_fence, u64::MAX);
                        state.observe_native_status(&arming_status(
                            1,
                            worker::PttArmingState::Armed,
                        ));
                        state.recover_native_clock_fence(clock);
                    } else {
                        state.observe_native_status(&arming_status(
                            1,
                            worker::PttArmingState::Armed,
                        ));
                        state.recover_native_clock_fence(clock);
                        assert_eq!(state.native_fence, u64::MAX);
                        assert!(state.native_after_fence(&event, 0, clock));
                    }
                    if !clock_ready {
                        assert_eq!(state.native_fence, u64::MAX);
                        assert_ne!(
                            state.native_arming(),
                            Some(gui::AudioPttArmingState::Armed as i32)
                        );
                        state.recover_native_clock_fence(Some(u64::MAX));
                        assert_eq!(state.native_fence, u64::MAX);
                        state.observe_native_status(&arming_status(
                            1,
                            worker::PttArmingState::Armed,
                        ));
                        state.recover_native_clock_fence(Some(200));
                    }
                    assert_eq!(state.native_fence, 200);
                    assert_eq!(
                        state.native_arming(),
                        Some(gui::AudioPttArmingState::Armed as i32)
                    );
                    event.kind = worker::PttEventKind::Pressed as i32;
                    event.expires_at_ns = 250;
                    for observed in [199, 200] {
                        event.observed_at_ns = observed;
                        assert!(!state.native_after_fence(&event, 0, Some(202)));
                    }
                    event.observed_at_ns = 201;
                    assert!(state.native_after_fence(&event, 0, Some(202)));
                    assert!(native_live(&event, 202));
                }
            }
        }
    }
    #[test]
    fn clock_repair_forgets_proof_after_synthetic_release_new_epoch_or_next_fence() {
        for next_fence in [false, true] {
            let mut state = State::default();
            state.observe_native_status(&arming_status(
                0,
                worker::PttArmingState::NeedsPhysicalRelease,
            ));
            state.fence_holds(u64::MAX);
            state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
            let mut event = worker::PttEvent {
                kind: worker::PttEventKind::Released as i32,
                reason: worker::PttReleaseReason::PhysicalRelease as i32,
                safety_epoch: 1,
                ..Default::default()
            };
            assert!(state.native_after_fence(&event, 0, None));
            event.reason = worker::PttReleaseReason::Revoked as i32;
            assert!(state.native_after_fence(&event, 0, Some(200)));
            state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
            state.recover_native_clock_fence(Some(200));
            assert_eq!(state.native_fence, u64::MAX);
            event.reason = worker::PttReleaseReason::PhysicalRelease as i32;
            assert!(state.native_after_fence(&event, 0, None));
            if next_fence {
                state.fence_holds(u64::MAX);
            }
            state.observe_native_status(&arming_status(2, worker::PttArmingState::Armed));
            state.recover_native_clock_fence(Some(200));
            assert_eq!(state.native_fence, u64::MAX);
            assert!(!state.native_after_fence(&event, 0, Some(200)));
            event.safety_epoch = 2;
            assert!(state.native_after_fence(&event, 0, Some(200)));
            assert_eq!(state.native_fence, 200);
        }
    }
    #[test]
    fn clock_recovery_cannot_repair_exhausted_native_epoch_counter() {
        let mut state = State::default();
        state.observe_native_status(&arming_status(u64::MAX, worker::PttArmingState::Armed));
        state.fence_holds(u64::MAX);
        let event = worker::PttEvent {
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::PhysicalRelease as i32,
            safety_epoch: u64::MAX,
            ..Default::default()
        };
        assert!(!state.native_after_fence(&event, 0, Some(200)));
        state.recover_native_clock_fence(Some(200));
        assert!(state.native_epoch_exhausted);
        assert_eq!(state.native_fence, u64::MAX);
        assert_eq!(state.native_arming(), None);
    }
    #[test]
    fn synthetic_terminal_native_events_withdraw_existing_armed_readiness() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        for reason in [
            worker::PttReleaseReason::Expired,
            worker::PttReleaseReason::Revoked,
            worker::PttReleaseReason::Replaced,
            worker::PttReleaseReason::ExplicitMute,
            worker::PttReleaseReason::BackendFailed,
        ] {
            let mut state = State::default();
            state.set_local_intent(false);
            let (out, _receive) = mpsc::channel(1);
            state.held = Some(Held {
                connection: [4; 16],
                out,
                press: [5; 16],
                parent,
                next: 100,
            });
            state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
            let mut event = worker::PttEvent {
                kind: worker::PttEventKind::Released as i32,
                reason: worker::PttReleaseReason::PhysicalRelease as i32,
                safety_epoch: 1,
                ..Default::default()
            };
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::Armed as i32)
            );
            event.reason = reason as i32;
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
            );
            assert_eq!(state.held.as_ref().unwrap().press, [5; 16]);
            assert_eq!(state.config_muted, Some(false));
            assert!(!state.authority.recovery_muted());
            event.kind = worker::PttEventKind::Renewed as i32;
            event.expires_at_ns = 200;
            assert!(!state.native_after_fence(&event, 0, Some(500)));
            state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
            );
            event.kind = worker::PttEventKind::Released as i32;
            event.reason = worker::PttReleaseReason::PortalRelease as i32;
            event.observed_at_ns = 100;
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::Armed as i32)
            );
            event.kind = worker::PttEventKind::Pressed as i32;
            event.observed_at_ns = 101;
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(state.held.as_ref().unwrap().press, [5; 16]);
            assert!(!state.authority.recovery_muted());
        }
    }
    #[test]
    fn correlated_or_remote_release_outcomes_never_prove_native_rearming() {
        let mut state = State::default();
        state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
        for reason in [
            worker::PttReleaseReason::PhysicalRelease,
            worker::PttReleaseReason::PortalRelease,
        ] {
            let mut event = worker::PttEvent {
                kind: worker::PttEventKind::Released as i32,
                reason: reason as i32,
                safety_epoch: 1,
                ..Default::default()
            };
            assert!(state.native_after_fence(&event, 7, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
            );
            event.grant_id = 9;
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
            );
            event.grant_id = 0;
            assert!(state.native_after_fence(&event, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::Armed as i32)
            );
        }
    }
    fn arming_status(epoch: u64, state: worker::PttArmingState) -> worker::PttStatus {
        worker::PttStatus {
            state: state as i32,
            safety_epoch: epoch,
            registered: true,
            ..Default::default()
        }
    }
    #[test]
    fn trusted_portal_logical_release_is_distinct_and_current_epoch_only() {
        let mut state = State::default();
        state.bind_native_worker([1; 16]);
        state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease,
        ));
        state.fence_holds(100);
        state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed));
        let mut event = worker::PttEvent {
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::PortalRelease as i32,
            observed_at_ns: 101,
            safety_epoch: 0,
            ..Default::default()
        };
        assert_ne!(
            event.reason,
            worker::PttReleaseReason::PhysicalRelease as i32
        );
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        event.safety_epoch = 1;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::Armed as i32)
        );
        state.fence_holds(200);
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        state.observe_native_status(&arming_status(2, worker::PttArmingState::Armed));
        event.safety_epoch = 2;
        event.reason = worker::PttReleaseReason::Expired as i32;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        event.reason = worker::PttReleaseReason::PortalRelease as i32;
        event.observed_at_ns = 201;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        event.kind = worker::PttEventKind::Pressed as i32;
        event.observed_at_ns = 202;
        event.expires_at_ns = 250;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert!(native_live(&event, 203));
    }
    #[test]
    fn gui_unavailable_reason_requires_current_actual_unavailable_status() {
        let mut state = State::default();
        assert_eq!(state.native_unavailable_reason(), None);
        for (reason, gui_reason) in [
            (1, gui::AudioPttUnavailableReason::HostClockUnavailable),
            (
                2,
                gui::AudioPttUnavailableReason::PortalFreshnessUnverifiable,
            ),
            (3, gui::AudioPttUnavailableReason::RegistrationUnavailable),
        ] {
            let status = worker::PttStatus {
                reason,
                ..arming_status(0, worker::PttArmingState::Unavailable)
            };
            assert!(state.observe_native_status(&status));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::Unavailable as i32)
            );
            assert_eq!(state.native_unavailable_reason(), Some(gui_reason as i32));
        }
        for reason in [0, 4] {
            state.observe_native_status(&worker::PttStatus {
                reason,
                ..arming_status(0, worker::PttArmingState::Unavailable)
            });
            assert_eq!(state.native_unavailable_reason(), None);
        }
        state.observe_native_status(&worker::PttStatus {
            reason: 3,
            ..arming_status(0, worker::PttArmingState::NeedsPhysicalRelease)
        });
        assert_eq!(state.native_unavailable_reason(), None);
        assert_eq!(state.gui_activation(Op::Press, 7, 7), Ok(()));
        state.fence_holds(100);
        assert!(!state.observe_native_status(&worker::PttStatus {
            reason: 1,
            ..arming_status(0, worker::PttArmingState::Unavailable)
        }));
        assert_eq!(state.native_unavailable_reason(), None);
    }
    #[test]
    fn repeated_native_safety_commands_require_latest_epoch_but_duplicates_do_not_raise_floor() {
        let mut state = State::default();
        state.bind_native_worker([1; 16]);
        state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease,
        ));
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let first = Control {
            request: id(1, parent.connection_id),
            revision: 0,
            op: Op::ReleaseAll,
            press: [0; 16],
            challenge: 0,
            expires_ns: 0,
        };
        let second = Control {
            request: id(2, parent.connection_id),
            ..first
        };
        for (now, control) in [(100, first), (101, second), (102, second)] {
            let decision = state.authority.decide(now, parent, true, true, control);
            if decision.effect == Effect::ReleaseAll {
                state.fence_holds(now);
            }
        }
        assert_eq!(state.native_epoch_floor, Some(2));
        assert!(!state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed)));
        let mut event = worker::PttEvent {
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::PhysicalRelease as i32,
            safety_epoch: 1,
            ..Default::default()
        };
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        assert!(state.observe_native_status(&arming_status(2, worker::PttArmingState::Armed)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        event.safety_epoch = 2;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::Armed as i32)
        );
        event.kind = worker::PttEventKind::Pressed as i32;
        event.observed_at_ns = 103;
        event.expires_at_ns = 200;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert!(native_live(&event, 104));
    }
    #[test]
    fn epoch_zero_is_observed_but_status_alone_never_rearms_native_ptt() {
        let mut state = State::default();
        state.bind_native_worker([1; 16]);
        assert_eq!(state.native_arming(), None);
        assert!(state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease
        )));
        assert_eq!(state.native_epoch, Some(0));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        assert!(state.observe_native_status(&arming_status(0, worker::PttArmingState::Armed)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        let mut release = worker::PttEvent {
            kind: worker::PttEventKind::Released as i32,
            reason: worker::PttReleaseReason::PhysicalRelease as i32,
            safety_epoch: 0,
            ..Default::default()
        };
        assert!(state.native_after_fence(&release, 0, Some(500)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        state.fence_holds(100);
        assert_eq!(state.native_arming(), None);
        assert!(!state.observe_native_status(&arming_status(0, worker::PttArmingState::Armed)));
        assert!(!state.native_after_fence(&release, 0, Some(500)));
        assert!(state.observe_native_status(&arming_status(1, worker::PttArmingState::Armed)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        assert!(!state.native_after_fence(&release, 0, Some(500)));
        release.safety_epoch = 1;
        for reason in [
            worker::PttReleaseReason::Revoked,
            worker::PttReleaseReason::Expired,
            worker::PttReleaseReason::PersistentUnmute,
        ] {
            release.reason = reason as i32;
            assert!(state.native_after_fence(&release, 0, Some(500)));
            assert_eq!(
                state.native_arming(),
                Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
            );
        }
        release.reason = worker::PttReleaseReason::PhysicalRelease as i32;
        assert!(state.native_after_fence(&release, 0, Some(500)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::Armed as i32)
        );
        assert!(state.observe_native_status(&arming_status(1, worker::PttArmingState::Unavailable)));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::Unavailable as i32)
        );
    }
    #[test]
    fn only_replacement_worker_resets_native_epoch_baseline() {
        let mut state = State::default();
        state.bind_native_worker([1; 16]);
        state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease,
        ));
        state.fence_holds(100);
        state.native_status = None;
        state.bind_native_worker([1; 16]);
        assert_eq!(state.native_epoch_floor, Some(1));
        assert!(!state.observe_native_status(&arming_status(0, worker::PttArmingState::Armed)));
        state.bind_native_worker([2; 16]);
        assert!(state.observe_native_status(&arming_status(
            0,
            worker::PttArmingState::NeedsPhysicalRelease
        )));
        assert_eq!(
            state.native_arming(),
            Some(gui::AudioPttArmingState::NeedsPhysicalRelease as i32)
        );
        state.observe_native_status(&arming_status(u64::MAX, worker::PttArmingState::Armed));
        state.fence_holds(200);
        assert!(state.native_epoch_exhausted);
        assert!(
            !state.observe_native_status(&arming_status(u64::MAX, worker::PttArmingState::Armed))
        );
        assert_eq!(state.native_arming(), None);
    }
    #[test]
    fn stale_remote_revision_or_expiry_cannot_replace_cas_or_challenge() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut state = State::default();
        let current = net::MicState {
            revision: 8,
            challenge: 9,
            expires_ns: 250_000_000,
            ..Default::default()
        };
        state.update_remote(parent, &current, 0, 100);
        for stale in [
            net::MicState {
                revision: 7,
                challenge: 8,
                ..current.clone()
            },
            net::MicState {
                challenge: 8,
                expires_ns: 200_000_000,
                ..current.clone()
            },
            net::MicState {
                challenge: 8,
                ..current.clone()
            },
        ] {
            assert_eq!(state.update_remote(parent, &stale, 1, 101), None);
            assert_eq!(state.remote.as_ref().unwrap().state, current);
        }
        let unsupported = net::MicState {
            revision: current.revision + 1,
            supported: false,
            challenge: 0,
            expires_ns: 0,
            ..current.clone()
        };
        state.update_remote(parent, &unsupported, 1, 101);
        assert_eq!(state.remote.as_ref().unwrap().state, unsupported);
        let replacement = fsm::Parent {
            generation: [4; 16],
            ..parent
        };
        let baseline = net::MicState {
            revision: 1,
            challenge: 1,
            expires_ns: 100,
            ..Default::default()
        };
        state.update_remote(replacement, &baseline, 1, 101);
        assert_eq!(state.remote.as_ref().unwrap().state, baseline);
    }
    #[test]
    fn unchanged_visible_remote_safety_event_forces_queued_activation_cas() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let mut state = State::default();
        let initial = net::MicState {
            revision: 7,
            supported: true,
            ptt_enabled: true,
            muted: true,
            ..Default::default()
        };
        assert!(!state.fence_remote_talk_generation(parent, &initial, 100));
        state.update_remote(parent, &initial, 0, 100);
        let queued_gui_revision = 7;
        assert_eq!(
            state.gui_activation(Op::Press, queued_gui_revision, 7),
            Ok(())
        );
        let safety = net::MicState {
            talk_generation: 1,
            ..initial.clone()
        };
        assert_eq!(safety.muted, initial.muted);
        assert_eq!(safety.ptt_active, initial.ptt_active);
        assert!(state.fence_remote_talk_generation(parent, &safety, 200));
        // The event forces publication, bypassing equal visible-status suppression.
        let published_gui_revision = queued_gui_revision + 1;
        assert_eq!(
            state.gui_activation(Op::Press, queued_gui_revision, published_gui_revision),
            Err(ResultCode::StaleRevision)
        );
        state.update_remote(parent, &safety, 0, 200);
        assert!(!state.fence_remote_talk_generation(parent, &safety, 201));
        assert_eq!(state.update_remote(parent, &initial, 0, 202), None);
        assert_eq!(state.remote.as_ref().unwrap().state.talk_generation, 1);
        let replacement = fsm::Parent {
            generation: [4; 16],
            ..parent
        };
        assert!(!state.fence_remote_talk_generation(replacement, &safety, 203));
        state.update_remote(replacement, &initial, 0, 203);
        assert_eq!(state.remote.as_ref().unwrap().state.talk_generation, 0);
        assert!(!state.fence_remote_talk_generation(replacement, &initial, 204));
    }
    #[test]
    fn global_release_preserves_pending_persistent_intent_and_correlation() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        for muted in [Some(false), Some(true), None] {
            let mut state = State::default();
            state.set_local_intent(false);
            state.pending = Some(PendingMic {
                reply: Reply::Network(parent, id(1, parent.connection_id).to_vec()),
                worker_generation: [4; 16],
                request: 5,
                deadline: 1000,
                muted,
                previous: true,
                endpoint_generation: 6,
            });
            let cancelled = state.take_activation(false);
            assert_eq!(cancelled.is_some(), muted.is_none());
            state.authority.release_all();
            if muted.is_some() {
                let pending = state.pending.as_ref().unwrap();
                assert_eq!(pending.worker_generation, [4; 16]);
                assert_eq!(pending.request, 5);
                assert_eq!(pending.muted, muted);
                assert_eq!(pending.endpoint_generation, 6);
            }
            assert_eq!(state.config_muted, Some(false));
            assert!(!state.authority.desired_muted);
            assert!(!state.authority.recovery_muted());
        }
    }
    #[test]
    fn global_release_fence_rejects_queued_native_edges_and_ownerless_renewals() {
        let mut state = State {
            native_press: Some([1; 16]),
            native_expires: 200,
            ..Default::default()
        };
        state.fence_holds(100);
        assert!(state.observe_native_status(&worker::PttStatus {
            state: worker::PttArmingState::Armed as i32,
            safety_epoch: 1,
            registered: true,
            ..Default::default()
        }));
        let mut event = worker::PttEvent {
            kind: worker::PttEventKind::Pressed as i32,
            observed_at_ns: 99,
            expires_at_ns: 200,
            safety_epoch: 1,
            ..Default::default()
        };
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        event.kind = worker::PttEventKind::Renewed as i32;
        event.observed_at_ns = 0;
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        event.kind = worker::PttEventKind::Released as i32;
        event.reason = worker::PttReleaseReason::Revoked as i32;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert!(state.native_suppressed);
        event.kind = worker::PttEventKind::Pressed as i32;
        event.observed_at_ns = 101;
        assert!(!state.native_after_fence(&event, 0, Some(500)));
        event.kind = worker::PttEventKind::Released as i32;
        event.reason = worker::PttReleaseReason::PhysicalRelease as i32;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        assert!(!state.native_suppressed);
        event.kind = worker::PttEventKind::Pressed as i32;
        event.observed_at_ns = 101;
        assert!(state.native_after_fence(&event, 0, Some(500)));
        state.native_press = Some([1; 16]);
        event.kind = worker::PttEventKind::Renewed as i32;
        event.observed_at_ns = 0;
        assert!(state.native_after_fence(&event, 0, Some(500)));
    }
    #[test]
    fn rejected_unmute_preserves_both_hold_owners_and_fresh_state_renewal() {
        for stale in [true, false] {
            let parent = fsm::Parent {
                peer: [1; 32],
                connection_id: [2; 16],
                generation: [3; 16],
            };
            let (out, _receive) = mpsc::channel(1);
            let mut state = State {
                held: Some(Held {
                    connection: [4; 16],
                    out: out.clone(),
                    press: [5; 16],
                    parent,
                    next: 100,
                }),
                native_parent: Some(parent),
                native_press: Some([6; 16]),
                native_expires: 250_000_000,
                pending: Some(PendingMic {
                    reply: Reply::Gui(Box::new(Request {
                        request_id: 1,
                        connection_id: [4; 16],
                        message: gui::gui_frame::Msg::SetPtt(gui::SetPtt {
                            pressed: true,
                            expected_revision: 7,
                        }),
                        out,
                    })),
                    worker_generation: [7; 16],
                    request: 1,
                    deadline: 1000,
                    muted: None,
                    previous: true,
                    endpoint_generation: 1,
                }),
                ..Default::default()
            };
            assert_eq!(
                state.gui_activation(Op::SetMute(false), if stale { 6 } else { 7 }, 7),
                Err(if stale {
                    ResultCode::StaleRevision
                } else {
                    ResultCode::Busy
                })
            );
            assert_eq!(state.native_press, Some([6; 16]));
            assert_eq!(state.native_parent, Some(parent));
            assert_eq!(state.native_expires, 250_000_000);
            let remote = net::MicState {
                revision: 8,
                supported: true,
                ptt_enabled: true,
                expires_ns: 300_000_000,
                ..Default::default()
            };
            assert_eq!(
                state.update_remote(parent, &remote, 100, 100_000_000),
                Some([5; 16])
            );
            assert_eq!(state.held.as_ref().unwrap().next, 200);
            assert_eq!(state.gui_activation(Op::SetMute(true), 0, 7), Ok(()));
            assert_eq!(state.gui_activation(Op::Release, 0, 7), Ok(()));
        }
    }
    #[test]
    fn safety_cancellation_removes_every_renewal_owner() {
        let parent = fsm::Parent {
            peer: [1; 32],
            connection_id: [2; 16],
            generation: [3; 16],
        };
        let (out, _receive) = mpsc::channel(1);
        let mut state = State {
            held: Some(Held {
                connection: [4; 16],
                out,
                press: [5; 16],
                parent,
                next: 100,
            }),
            native_parent: Some(parent),
            native_press: Some([6; 16]),
            native_expires: 250_000_000,
            ..Default::default()
        };
        assert_eq!(
            state.take_holds(),
            [Some((parent, [5; 16])), Some((parent, [6; 16]))]
        );
        for now in [100, 200, 300] {
            let remote = net::MicState {
                revision: now,
                supported: true,
                ptt_enabled: true,
                expires_ns: now * 1_000_000 + policy::LEASE_NS,
                ..Default::default()
            };
            assert_eq!(
                state.update_remote(parent, &remote, now, now * 1_000_000),
                None
            );
        }
        assert!(state.held.is_none());
        assert!(state.native_parent.is_none());
        assert!(state.native_press.is_none());
        assert_eq!(state.native_expires, 0);
    }
    #[test]
    fn retained_local_safety_mute_is_the_replacement_child_recovery_base() {
        let mut state = State::default();
        state.set_local_intent(false);
        state.authority.observe(true);
        state.set_local_intent(true);
        assert!(state.authority.muted);
        assert_eq!(state.config_muted, Some(true));
        assert!(state.authority.desired_muted_by.is_none());
        state.authority.reset();
        state.sync_config_intent(true);
        assert_eq!(
            state.authority.recovery_mode,
            Some(policy::RecoveryMode::Persistent(true))
        );
        assert!(state.authority.recovery_muted());
    }
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
        let mut source = State {
            remote: Some(Remote {
                parent,
                state: net::MicState {
                    revision: authority.revision,
                    challenge,
                    expires_ns,
                    supported: true,
                    ptt_enabled: true,
                    ..Default::default()
                },
            }),
            ..Default::default()
        };
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
