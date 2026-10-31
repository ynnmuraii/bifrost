//! Authoritative audio IPC adapter. Pending intents never change playback or privacy state.
use crate::audio_player::{AppKind, AudioPlayerAction, AudioPlayerCard, StreamState};
use crate::view_model::{Modal, Request, ViewModel};
use audio_model::status as model;
use protocol::ipc::{self, gui_frame::Msg};
use std::time::{Duration, Instant};
use ui_text::{text, Key, Lang};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRequest {
    Snapshot,
    Devices,
    Mutation,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AudioAction {
    EditConfig(Box<ipc::AudioConfig>),
    ApplyConfig,
    Refresh,
    UseThisListener,
    ListDevices(ipc::AudioDeviceKind),
    Start(Vec<u8>),
    StopAll,
    SetMicrophoneMute(bool),
    SetPtt(bool),
    Player(AudioPlayerAction),
    OpenSettings,
}

#[derive(Debug, Clone)]
pub struct AudioView {
    pub status: Option<ipc::AudioStatus>,
    pub draft: ipc::AudioConfig,
    pub devices: Vec<ipc::AudioDevices>,
    pub pending: bool,
    pub available: bool,
    pub error: Option<String>,
    pending_id: Option<u64>,
    pending_revision: Option<u64>,
    pending_since: Option<Instant>,
    device_requests: Vec<(u64, i32)>,
    pub dirty: bool,
    /// Ownership of a GUI hold button, separate from authoritative/global PTT state.
    pub gui_ptt_held: bool,
}

impl Default for AudioView {
    fn default() -> Self {
        Self {
            status: None,
            draft: default_config(),
            devices: Vec::new(),
            pending: false,
            available: false,
            error: None,
            pending_id: None,
            pending_revision: None,
            pending_since: None,
            device_requests: Vec::new(),
            dirty: false,
            gui_ptt_held: false,
        }
    }
}

pub fn default_config() -> ipc::AudioConfig {
    ipc::AudioConfig {
        enabled: false,
        role: ipc::AudioRole::Source as i32,
        group_id_override: Some(false),
        mode: ipc::AudioMode::MixAll as i32,
        profile: ipc::AudioProfile::Ethernet as i32,
        udp_port: 47903,
        output_device: "default".into(),
        capture_device: "default_monitor".into(),
        focus_debounce_ms: 250,
        focus_ramp_ms: 150,
        background_gain_db: -18,
        microphone: Some(ipc::AudioMicrophoneConfig {
            capture_device: "default".into(),
            muted: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl AudioView {
    fn settle(&mut self) {
        self.pending = false;
        self.pending_id = None;
        self.pending_revision = None;
        self.pending_since = None;
    }
    fn fail(&mut self, lang: Lang, key: Key) {
        self.settle();
        self.error = Some(text(lang, key).into());
        if let Some(config) = self.status.as_ref().and_then(|s| s.config.as_ref()) {
            self.draft = config.clone();
        }
        self.dirty = false;
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.pending_since.map(|t| t + Duration::from_secs(10))
    }
}

pub fn error_key(code: &str) -> Key {
    match code {
        "AUDIO_CONSENT_REQUIRED" => Key::AudioConsentRequired,
        "AUDIO_ROLE_CONFLICT" => Key::AudioRoleConflict,
        "AUDIO_UNSUPPORTED_BACKEND" => Key::AudioUnsupportedBackend,
        "AUDIO_NO_UDP_PATH" => Key::AudioNoUdpPath,
        "AUDIO_DEVICE_UNAVAILABLE" => Key::AudioDeviceUnavailable,
        "AUDIO_VIRTUAL_MIC_MISSING" => Key::AudioVirtualMicMissing,
        "AUDIO_CONFIG_WRITE" => Key::AudioConfigWrite,
        "AUDIO_REVISION_CONFLICT" => Key::AudioRevisionConflict,
        "AUDIO_STALE_MEDIA" => Key::AudioStaleMedia,
        "AUDIO_UNSUPPORTED_COMMAND" => Key::AudioUnsupportedCommand,
        "AUDIO_INVALID_CONFIG" => Key::AudioInvalidConfig,
        _ => Key::AudioUnavailable,
    }
}

impl ViewModel {
    pub fn request_audio_status(&mut self, now: Instant) {
        if !self
            .inflight
            .iter()
            .any(|r| r.request == Request::Audio(AudioRequest::Snapshot))
        {
            let id = self.begin_request(Request::Audio(AudioRequest::Snapshot), now);
            self.outgoing
                .push((id, Msg::GetAudioStatus(ipc::GetAudioStatus {})));
        }
    }
    pub fn apply_audio_devices(&mut self, id: u64, mut page: ipc::AudioDevices) {
        if !self.audio.device_requests.contains(&(id, page.kind)) {
            return;
        }
        if page.complete {
            self.audio
                .device_requests
                .retain(|(request, _)| *request != id);
            self.inflight.retain(|request| request.id != id);
        }
        if let Some(index) = self
            .audio
            .devices
            .iter()
            .position(|old| old.kind == page.kind)
        {
            let old = &mut self.audio.devices[index];
            if old.revision > page.revision {
                return;
            }
            if old.revision == page.revision && !old.complete {
                if old.devices.len() + page.devices.len() > 64
                    || page
                        .devices
                        .iter()
                        .any(|device| old.devices.iter().any(|prior| prior.id == device.id))
                {
                    self.audio.error =
                        Some(text(self.look.language, Key::AudioDeviceUnavailable).into());
                    self.audio
                        .device_requests
                        .retain(|(request, _)| *request != id);
                    self.inflight.retain(|request| request.id != id);
                    return;
                }
                old.devices.append(&mut page.devices);
                old.complete = page.complete;
            } else {
                *old = page;
            }
        } else {
            self.audio.devices.push(page);
        }
    }
    pub fn apply_audio_status(&mut self, status: ipc::AudioStatus) {
        if self
            .audio
            .status
            .as_ref()
            .is_some_and(|old| old.revision >= status.revision)
        {
            return;
        }
        let Ok(model) = crate::audio_wire::decode_status(&status) else {
            self.audio.available = false;
            self.audio.fail(self.look.language, Key::AudioUnavailable);
            self.audio_player.replace_cards(Vec::new());
            return;
        };
        if !self.audio.dirty || self.audio.pending {
            if let Some(config) = status.config.as_ref() {
                self.audio.draft = config.clone();
            }
            self.audio.dirty = false;
        }
        if self
            .audio
            .pending_revision
            .is_some_and(|revision| status.revision > revision)
        {
            self.audio.settle();
        }
        self.audio.error = None;
        self.audio.available = true;
        let cards = map_cards(
            &model,
            self.status
                .as_ref()
                .map_or(&[][..], |status| &status.members),
        );
        self.audio.status = Some(status);
        self.audio_player.replace_cards(cards);
    }
    pub(crate) fn refresh_audio_peer_names(&mut self) {
        let Some(audio) = &self.audio.status else {
            return;
        };
        let members = self
            .status
            .as_ref()
            .map_or(&[][..], |status| &status.members);
        for card in &mut self.audio_player.cards {
            if let Some(session) = audio
                .sessions
                .iter()
                .find(|session| matches_id(&card.stream_id, &session.session_id))
            {
                card.source_name = peer_name(members, &session.peer);
            }
        }
    }
    pub fn audio_result(&mut self, id: u64, kind: AudioRequest, result: ipc::OpResult) {
        if kind == AudioRequest::Devices {
            self.audio
                .device_requests
                .retain(|(request, _)| *request != id);
        }
        if kind == AudioRequest::Mutation && self.audio.pending_id != Some(id) {
            return;
        }
        if !result.ok {
            let key = error_key(&result.error_code);
            self.audio.fail(self.look.language, key);
            if key == Key::AudioUnavailable {
                self.audio.available = false;
                self.audio_player.replace_cards(Vec::new());
            }
            return;
        }
        // ACK means accepted/persisted, not active. Only a new snapshot settles controls.
        if kind == AudioRequest::Mutation {
            self.request_audio_status(Instant::now());
        }
    }
    pub fn tick_audio(&mut self, now: Instant) {
        let expired = self
            .inflight
            .iter()
            .any(|r| matches!(r.request, Request::Audio(_)) && now >= r.deadline);
        if expired || self.audio.deadline().is_some_and(|d| now >= d) {
            self.inflight
                .retain(|r| !matches!(r.request, Request::Audio(_)) || now < r.deadline);
            self.audio.available = false;
            self.audio.fail(self.look.language, Key::AudioUnavailable);
        }
        self.audio
            .device_requests
            .retain(|(id, _)| self.inflight.iter().any(|request| request.id == *id));
    }
    pub fn audio_action(&mut self, action: &AudioAction, now: Instant) {
        match action {
            AudioAction::EditConfig(config) => {
                self.audio.draft.clone_from(config.as_ref());
                self.audio.dirty = true;
                return;
            }
            AudioAction::UseThisListener => {
                use ring::rand::SecureRandom;
                let was_listener = self.audio.draft.role == ipc::AudioRole::Listener as i32;
                if !was_listener || self.audio.draft.group_id.len() != 16 {
                    let mut id = vec![0; 16];
                    if ring::rand::SystemRandom::new().fill(&mut id).is_err() {
                        self.audio.error =
                            Some(text(self.look.language, Key::AudioUnavailable).into());
                        return;
                    }
                    self.audio.draft.group_id = id;
                }
                self.audio.draft.role = ipc::AudioRole::Listener as i32;
                self.audio.draft.group_id_override = Some(false);
                self.audio.draft.listener_node_id.clear();
                if !was_listener {
                    self.audio.draft.enabled = false;
                }
                self.audio.dirty = true;
                return;
            }
            AudioAction::OpenSettings => {
                self.appearance_open = false;
                self.modal = Some(Modal::Audio);
                self.request_audio_status(now);
                return;
            }
            AudioAction::Refresh => {
                self.request_audio_status(now);
                return;
            }
            AudioAction::Player(AudioPlayerAction::OpenSettings) => {
                self.audio_action(&AudioAction::OpenSettings, now);
                return;
            }
            AudioAction::Player(AudioPlayerAction::HidePlayer) => {
                self.audio_player.open = false;
                return;
            }
            _ => {}
        }
        if let AudioAction::ListDevices(kind) = action {
            if self
                .audio
                .device_requests
                .iter()
                .any(|(_, pending_kind)| *pending_kind == *kind as i32)
            {
                return;
            }
            let id = self.begin_request(Request::Audio(AudioRequest::Devices), now);
            self.audio.device_requests.push((id, *kind as i32));
            self.outgoing.push((
                id,
                Msg::ListAudioDevices(ipc::ListAudioDevices { kind: *kind as i32 }),
            ));
            return;
        }
        // Fail-closed stop/mute/release remain admissible even when a status request failed.
        let urgent = matches!(
            action,
            AudioAction::StopAll
                | AudioAction::SetMicrophoneMute(true)
                | AudioAction::SetPtt(false)
                | AudioAction::Player(AudioPlayerAction::Stop(_))
                | AudioAction::Player(AudioPlayerAction::SetMuted { muted: true, .. })
        );
        if (!self.audio.available && !urgent) || !self.connection.is_connected() {
            self.audio.error = Some(text(self.look.language, Key::AudioUnavailable).into());
            return;
        }
        if self.audio.pending && !urgent {
            return;
        }
        let Some(status) = self.audio.status.as_ref() else {
            return;
        };
        let revision = status.revision;
        let message = match action {
            AudioAction::ApplyConfig => {
                if ::ipc::audio::validate_config(&self.audio.draft).is_err() {
                    self.audio.error =
                        Some(text(self.look.language, Key::AudioInvalidConfig).into());
                    return;
                }
                Msg::SetAudio(ipc::SetAudio {
                    config: Some(self.audio.draft.clone()),
                    expected_revision: revision,
                })
            }
            AudioAction::Start(peer) => Msg::StartAudio(ipc::StartAudio {
                peer_node_id: peer.clone(),
                kind: ipc::AudioKind::System as i32,
                profile: status
                    .config
                    .as_ref()
                    .map_or(ipc::AudioProfile::Ethernet as i32, |c| c.profile),
                expected_revision: revision,
            }),
            AudioAction::StopAll => Msg::StopAudio(ipc::StopAudio {
                stop_all: true,
                ..Default::default()
            }),
            AudioAction::SetMicrophoneMute(muted) => {
                Msg::SetMicrophoneMute(ipc::SetMicrophoneMute {
                    muted: *muted,
                    expected_revision: revision,
                })
            }
            AudioAction::SetPtt(pressed) => Msg::SetPtt(ipc::SetPtt {
                pressed: *pressed,
                expected_revision: revision,
            }),
            AudioAction::Player(intent) => {
                let Some(message) = player_request(status, intent) else {
                    self.audio.error = Some(text(self.look.language, Key::AudioStaleMedia).into());
                    return;
                };
                message
            }
            _ => return,
        };
        let id = self.begin_request(Request::Audio(AudioRequest::Mutation), now);
        if let AudioAction::SetPtt(pressed) = action {
            self.audio.gui_ptt_held = *pressed;
        }
        self.audio.pending = true;
        self.audio.pending_id = Some(id);
        self.audio.pending_revision = Some(revision);
        self.audio.pending_since = Some(now);
        self.audio.error = None;
        self.outgoing.push((id, message));
    }
}

fn eligible(session: &model::SessionStatus) -> bool {
    session.local_role == model::AudioRole::Listener
        && matches!(
            session.state,
            model::SessionState::PathValidation
                | model::SessionState::Running
                | model::SessionState::Rebuffering
                | model::SessionState::DevicePaused
        )
}
fn media_allowed(session: &model::SessionStatus) -> bool {
    eligible(session)
        && session
            .media_info
            .as_ref()
            .is_some_and(|info| info.session_token != 0 && info.revision != 0)
        && session.consent.local.allow_system_receive
        && session.consent.remote.allow_system_send
        && session.consent.remote.allow_media_control
}
pub fn stream_id(id: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(id.len() * 2);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
pub fn map_cards(
    status: &model::AudioStatus,
    members: &[ipc::MemberStatus],
) -> Vec<AudioPlayerCard> {
    if !status.enabled {
        return Vec::new();
    }
    status
        .sessions
        .iter()
        .filter_map(|s| {
            let path = crate::audio_path::AudioPathObservation::from_model(s);
            let diagnosed_failure = s.local_role == model::AudioRole::Listener
                && s.state == model::SessionState::Failed
                && path.is_some_and(|observation| observation.has_diagnostic());
            if !eligible(s) && !diagnosed_failure {
                return None;
            }
            let media = s.media_info.as_ref();
            let playing = media.is_some_and(|m| m.playback == model::PlaybackState::Playing);
            let allowed = media_allowed(s);
            Some(AudioPlayerCard {
                stream_id: stream_id(&s.session_id),
                source_name: peer_name(members, &s.peer),
                title: media
                    .filter(|m| !m.title.is_empty())
                    .map(|m| m.title.clone()),
                subtitle: media
                    .filter(|m| !m.subtitle.is_empty())
                    .map(|m| m.subtitle.clone()),
                has_media_session: media.is_some(),
                app: AppKind::System,
                position_ms: media.and_then(|m| m.position_ms),
                duration_ms: media.and_then(|m| m.duration_ms),
                playing,
                can_prev: allowed && media.is_some_and(|m| m.commands.previous),
                can_next: allowed && media.is_some_and(|m| m.commands.next),
                can_play_pause: allowed
                    && media.is_some_and(|m| {
                        m.commands.toggle
                            || if playing {
                                m.commands.pause
                            } else {
                                m.commands.play
                            }
                    }),
                muted: s.muted,
                gain_percent: s.gain_percent,
                path,
                state: match s.state {
                    model::SessionState::Failed => StreamState::Failed,
                    model::SessionState::DevicePaused => StreamState::Degraded,
                    model::SessionState::PathValidation | model::SessionState::Rebuffering => {
                        StreamState::Buffering
                    }
                    _ => match s.health_state {
                        model::StreamHealth::Ready => StreamState::Ready,
                        model::StreamHealth::Buffering => StreamState::Buffering,
                        model::StreamHealth::Degraded => StreamState::Degraded,
                    },
                },
                background: status.mode == model::AudioMode::FollowFocus && !s.focus_primary,
            })
        })
        .collect()
}

pub(crate) fn peer_name(members: &[ipc::MemberStatus], id: &[u8]) -> String {
    members
        .iter()
        .find(|member| matches_id(&member.node_id, id))
        .filter(|member| !member.display_name.trim().is_empty())
        .map_or_else(
            || stream_id(&id[..id.len().min(4)]),
            |member| member.display_name.clone(),
        )
}

pub(crate) fn matches_id(hex: &str, id: &[u8]) -> bool {
    hex.len() == id.len() * 2
        && hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(id)
            .all(|(pair, byte)| {
                let digit = |b: u8| (b as char).to_digit(16);
                digit(pair[0])
                    .zip(digit(pair[1]))
                    .is_some_and(|(hi, lo)| (hi * 16 + lo) as u8 == *byte)
            })
}

pub fn player_request(status: &ipc::AudioStatus, action: &AudioPlayerAction) -> Option<Msg> {
    let id = match action {
        AudioPlayerAction::Previous(id)
        | AudioPlayerAction::Next(id)
        | AudioPlayerAction::PlayPause(id)
        | AudioPlayerAction::Stop(id) => id,
        AudioPlayerAction::SetMuted { stream_id, .. }
        | AudioPlayerAction::SetGain { stream_id, .. } => stream_id,
        _ => return None,
    };
    let model = crate::audio_wire::decode_status(status).ok()?;
    if !model.enabled {
        return None;
    }
    let s = model
        .sessions
        .iter()
        .find(|s| eligible(s) && stream_id(&s.session_id) == *id)?;
    match action {
        AudioPlayerAction::Stop(_) => Some(Msg::StopAudio(ipc::StopAudio {
            peer_node_id: s.peer.to_vec(),
            session_id: s.session_id.to_vec(),
            stop_all: false,
        })),
        AudioPlayerAction::SetMuted { muted, .. } => {
            Some(Msg::SetAudioPeerGain(ipc::SetAudioPeerGain {
                peer_node_id: s.peer.to_vec(),
                gain_percent: u32::from(s.gain_percent),
                muted: *muted,
                expected_revision: status.revision,
            }))
        }
        AudioPlayerAction::SetGain { percent, .. } if *percent <= 100 => {
            Some(Msg::SetAudioPeerGain(ipc::SetAudioPeerGain {
                peer_node_id: s.peer.to_vec(),
                gain_percent: u32::from(*percent),
                muted: s.muted,
                expected_revision: status.revision,
            }))
        }
        AudioPlayerAction::Previous(_)
        | AudioPlayerAction::Next(_)
        | AudioPlayerAction::PlayPause(_)
            if media_allowed(s) =>
        {
            let m = s.media_info.as_ref()?;
            let command = match action {
                AudioPlayerAction::Previous(_) if m.commands.previous => {
                    ipc::AudioMediaCommand::Previous
                }
                AudioPlayerAction::Next(_) if m.commands.next => ipc::AudioMediaCommand::Next,
                AudioPlayerAction::PlayPause(_) if m.commands.toggle => {
                    ipc::AudioMediaCommand::Toggle
                }
                AudioPlayerAction::PlayPause(_)
                    if m.playback == model::PlaybackState::Playing && m.commands.pause =>
                {
                    ipc::AudioMediaCommand::Pause
                }
                AudioPlayerAction::PlayPause(_)
                    if m.playback != model::PlaybackState::Playing && m.commands.play =>
                {
                    ipc::AudioMediaCommand::Play
                }
                _ => return None,
            };
            Some(Msg::SendMediaCommand(ipc::SendMediaCommand {
                peer_node_id: s.peer.to_vec(),
                session_id: s.session_id.to_vec(),
                session_token: m.session_token,
                media_revision: m.revision,
                command: command as i32,
            }))
        }
        _ => None,
    }
}

/// A generic single-instance show signal wakes the GUI; only then read its bounded intent file.
pub static NAVIGATION_WAKE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn consume_navigation(view: &mut ViewModel, startup: bool) {
    use std::io::Read;
    let from_file = view.config.paths.as_ref().and_then(|paths| {
        let file = paths.state_dir().join("gui-audio-navigation");
        let mut value = String::new();
        let result = std::fs::File::open(&file)
            .and_then(|handle| handle.take(16).read_to_string(&mut value));
        if result.is_ok() {
            let _ = std::fs::remove_file(file);
            Some(value)
        } else {
            None
        }
    });
    let intent = from_file.or_else(|| {
        if !startup {
            return None;
        }
        std::env::args().find_map(|arg| match arg.as_str() {
            "--audio-player" => Some("player".to_owned()),
            "--audio-settings" => Some("settings".to_owned()),
            _ => None,
        })
    });
    match intent.as_deref() {
        Some("player") => view.audio_player.open = true,
        Some("settings") => view.audio_action(&AudioAction::OpenSettings, Instant::now()),
        _ => {}
    }
}
