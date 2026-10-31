//! Strict local audio IPC boundary. Errors never contain user metadata.
//! Identity trust, self-targeting and bilateral consent remain core checks.
use protocol::ipc as wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioError(pub &'static str);
impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for AudioError {}
pub type Result<T, E = AudioError> = std::result::Result<T, E>;
fn require(ok: bool, reason: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(AudioError(reason))
    }
}
fn id(value: &[u8], length: usize, optional: bool) -> Result<()> {
    require(
        value.len() == length || (optional && value.is_empty()),
        "invalid audio identity length",
    )
}
fn text(value: &str, max: usize, empty: bool) -> Result<()> {
    require(
        value.len() <= max && (empty || !value.is_empty()) && !value.chars().any(char::is_control),
        "invalid audio device or hotkey string",
    )
}
macro_rules! required_enum {
    ($decode:ident, $wire:ident, [$($variant:ident),+]) => {
        pub fn $decode(value: i32) -> Result<wire::$wire> {
            match wire::$wire::try_from(value) {
                $(Ok(wire::$wire::$variant) => Ok(wire::$wire::$variant),)+
                _ => Err(AudioError("invalid required audio enum")),
            }
        }
    };
}
required_enum!(decode_role, AudioRole, [Source, Listener]);
required_enum!(decode_mode, AudioMode, [MixAll, FollowFocus]);
required_enum!(decode_profile, AudioProfile, [Ethernet, Wifi]);
required_enum!(
    decode_session_state,
    AudioSessionState,
    [
        Idle,
        AwaitConsent,
        Preparing,
        AwaitCommit,
        PathValidation,
        Running,
        Rebuffering,
        DevicePaused,
        Stopping,
        Failed
    ]
);
required_enum!(
    decode_health,
    AudioStreamHealth,
    [Ready, Buffering, Degraded]
);
required_enum!(
    decode_capability,
    AudioBackendCapability,
    [
        SystemCapture,
        Render,
        EndpointMute,
        GlobalPtt,
        MediaSessions
    ]
);
required_enum!(
    decode_reason,
    AudioUnavailableReason,
    [
        FeatureNotBuilt,
        NotSupportedByPlatform,
        OsBuildTooOld,
        ProcessExclusionUnavailable,
        ReleaseObservationUnavailable,
        ServerUnavailable,
        PermissionDenied,
        DeviceGone,
        DeviceNotSelected,
        WorkerUnavailable
    ]
);
required_enum!(
    decode_playback,
    AudioPlaybackState,
    [Unknown, Playing, Paused, Stopped]
);
required_enum!(
    decode_command,
    AudioMediaCommand,
    [Play, Pause, Toggle, Next, Previous]
);
fn device_kind(value: i32) -> Result<()> {
    require(
        matches!(
            wire::AudioDeviceKind::try_from(value),
            Ok(wire::AudioDeviceKind::Output
                | wire::AudioDeviceKind::Capture
                | wire::AudioDeviceKind::Microphone)
        ),
        "invalid audio device kind",
    )
}

pub fn validate_config(config: &wire::AudioConfig) -> Result<()> {
    let role = decode_role(config.role)?;
    decode_mode(config.mode)?;
    decode_profile(config.profile)?;
    id(&config.listener_node_id, 32, true)?;
    id(&config.group_id, 16, true)?;
    require(
        !config.enabled || config.group_id.len() == 16,
        "enabled audio requires group",
    )?;
    require(
        !config.enabled || role != wire::AudioRole::Source || config.listener_node_id.len() == 32,
        "enabled source requires listener",
    )?;
    require(
        (1..=65535).contains(&config.udp_port) && config.udp_port != 47902,
        "invalid audio UDP port",
    )?;
    require(
        (100..=2000).contains(&config.focus_debounce_ms)
            && (20..=1000).contains(&config.focus_ramp_ms)
            && (-60..=0).contains(&config.background_gain_db),
        "audio tuning out of range",
    )?;
    text(&config.output_device, 256, false)?;
    text(&config.capture_device, 256, false)?;
    let mic = config
        .microphone
        .as_ref()
        .ok_or(AudioError("missing audio microphone config"))?;
    text(&mic.capture_device, 256, false)?;
    text(&mic.virtual_device, 256, true)?;
    text(&mic.ptt_hotkey, 256, true)?;
    id(&mic.pinned_target_node_id, 32, true)?;
    require(!mic.streaming_enabled, "microphone streaming deferred")?;
    require(
        !mic.ptt_enabled || mic.control_enabled,
        "PTT requires microphone control",
    )?;
    if mic.ptt_enabled || !mic.ptt_hotkey.is_empty() {
        config::hotkey::Hotkey::parse(&mic.ptt_hotkey)
            .map_err(|_| AudioError("invalid audio PTT hotkey"))?;
    }
    require(config.peers.len() <= 3, "too many audio peers")?;
    for (index, peer) in config.peers.iter().enumerate() {
        id(&peer.node_id, 32, false)?;
        require(
            !config.peers[..index]
                .iter()
                .any(|p| p.node_id == peer.node_id),
            "duplicate audio peer",
        )?;
        require(peer.volume_percent <= 100, "audio volume out of range")?;
        require(
            match role {
                wire::AudioRole::Source => !peer.allow_system_receive && !peer.allow_mic_send,
                wire::AudioRole::Listener => {
                    !peer.allow_system_send && !peer.allow_mic_receive && !peer.allow_media_control
                }
                wire::AudioRole::Unspecified => false,
            },
            "audio role permission conflict",
        )?;
        require(
            role != wire::AudioRole::Source
                || !peer.allow_system_send
                || peer.node_id == config.listener_node_id,
            "audio send peer is not selected listener",
        )?;
    }
    Ok(())
}

pub fn validate_status(status: &wire::AudioStatus) -> Result<()> {
    decode_role(status.role)?;
    decode_mode(status.mode)?;
    let config = status
        .config
        .as_ref()
        .ok_or(AudioError("missing audio config"))?;
    validate_config(config)?;
    require(
        status.enabled == config.enabled
            && status.role == config.role
            && status.mode == config.mode,
        "audio status config mismatch",
    )?;
    let mic = status
        .microphone
        .as_ref()
        .ok_or(AudioError("missing microphone state"))?;
    id(&mic.target_node_id, 32, true)?;
    require(!mic.capture_active, "microphone streaming deferred")?;
    require(
        !mic.ptt_pressed || mic.control_active,
        "PTT requires active control",
    )?;
    require(
        status.backend.len() <= 5 && status.sessions.len() <= 3,
        "audio snapshot count exceeded",
    )?;
    for (index, cap) in status.backend.iter().enumerate() {
        decode_capability(cap.capability)?;
        require(
            !status.backend[..index]
                .iter()
                .any(|c| c.capability == cap.capability),
            "duplicate audio capability",
        )?;
        match wire::AudioAvailability::try_from(cap.availability) {
            Ok(wire::AudioAvailability::Available) => {
                require(cap.reason == 0, "available audio capability has reason")?
            }
            Ok(wire::AudioAvailability::Unsupported | wire::AudioAvailability::Unavailable) => {
                decode_reason(cap.reason)?;
            }
            _ => return Err(AudioError("invalid audio availability")),
        }
    }
    for (index, session) in status.sessions.iter().enumerate() {
        id(&session.session_id, 16, false)?;
        id(&session.peer, 32, false)?;
        require(
            !status.sessions[..index]
                .iter()
                .any(|s| s.peer == session.peer || s.session_id == session.session_id),
            "duplicate audio session identity",
        )?;
        require(
            session.source_name.len() <= 128 && session.gain_percent <= 100,
            "audio session bounds exceeded",
        )?;
        require(
            session.kind == wire::AudioKind::System as i32,
            "unsupported audio kind",
        )?;
        decode_role(session.local_role)?;
        require(
            session.local_role == status.role,
            "audio session role mismatch",
        )?;
        decode_session_state(session.state)?;
        decode_profile(session.profile)?;
        decode_health(session.health_state)?;
        require(session.health.is_some(), "missing audio health")?;
        let consent = session
            .consent
            .as_ref()
            .ok_or(AudioError("missing audio consent"))?;
        let local = consent
            .local
            .as_ref()
            .ok_or(AudioError("missing local audio consent"))?;
        let remote = consent
            .remote
            .as_ref()
            .ok_or(AudioError("missing remote audio consent"))?;
        if let Some(media) = &session.media_info {
            require(
                media.app_id.len() <= 128
                    && media.title.len() <= 256
                    && media.subtitle.len() <= 256,
                "audio metadata bounds exceeded",
            )?;
            decode_playback(media.playback)?;
            require(
                media.commands.is_some(),
                "missing media command availability",
            )?;
            let consented = match decode_role(session.local_role)? {
                wire::AudioRole::Source => local.allow_system_send && remote.allow_system_receive,
                wire::AudioRole::Listener => local.allow_system_receive && remote.allow_system_send,
                wire::AudioRole::Unspecified => false,
            };
            require(
                status.enabled
                    && consented
                    && matches!(
                        wire::AudioSessionState::try_from(session.state),
                        Ok(wire::AudioSessionState::Running
                            | wire::AudioSessionState::Rebuffering
                            | wire::AudioSessionState::DevicePaused)
                    ),
                "media outside consented active session",
            )?;
        }
    }
    Ok(())
}

pub fn validate_frame(frame: &wire::GuiFrame) -> Result<()> {
    use wire::gui_frame::Msg;
    let Some(message) = &frame.msg else {
        return Ok(());
    };
    match message {
        Msg::AudioStatus(status) => return validate_status(status),
        Msg::AudioDevices(devices) => {
            device_kind(devices.kind)?;
            require(devices.devices.len() <= 64, "too many audio devices")?;
            for (index, device) in devices.devices.iter().enumerate() {
                text(&device.id, 256, false)?;
                text(&device.name, 256, true)?;
                require(
                    !devices.devices[..index].iter().any(|d| d.id == device.id),
                    "duplicate audio device",
                )?;
            }
        }
        Msg::GetAudioStatus(_) | Msg::SetMicrophoneMute(_) | Msg::SetPtt(_) => {}
        Msg::SetAudio(request) => validate_config(
            request
                .config
                .as_ref()
                .ok_or(AudioError("missing requested audio config"))?,
        )?,
        Msg::StartAudio(request) => {
            id(&request.peer_node_id, 32, false)?;
            require(
                request.kind == wire::AudioKind::System as i32,
                "unsupported audio kind",
            )?;
            decode_profile(request.profile)?;
        }
        Msg::StopAudio(request) => {
            if request.stop_all {
                require(
                    request.peer_node_id.is_empty() && request.session_id.is_empty(),
                    "stop all must not specify identities",
                )?;
            } else {
                id(&request.peer_node_id, 32, false)?;
                id(&request.session_id, 16, false)?;
            }
        }
        Msg::ListAudioDevices(request) => device_kind(request.kind)?,
        Msg::SetAudioPeerGain(request) => {
            id(&request.peer_node_id, 32, false)?;
            require(request.gain_percent <= 100, "audio gain out of range")?;
        }
        Msg::SendMediaCommand(request) => {
            id(&request.peer_node_id, 32, false)?;
            id(&request.session_id, 16, false)?;
            decode_command(request.command)?;
        }
        _ => return Ok(()),
    }
    require(
        frame.request_id != 0,
        "audio request or device reply requires request id",
    )
}

pub mod codes {
    pub const AUDIO_CONSENT_REQUIRED: &str = "AUDIO_CONSENT_REQUIRED";
    pub const AUDIO_ROLE_CONFLICT: &str = "AUDIO_ROLE_CONFLICT";
    pub const AUDIO_UNSUPPORTED_BACKEND: &str = "AUDIO_UNSUPPORTED_BACKEND";
    pub const AUDIO_NO_UDP_PATH: &str = "AUDIO_NO_UDP_PATH";
    pub const AUDIO_DEVICE_UNAVAILABLE: &str = "AUDIO_DEVICE_UNAVAILABLE";
    pub const AUDIO_VIRTUAL_MIC_MISSING: &str = "AUDIO_VIRTUAL_MIC_MISSING";
    pub const AUDIO_CONFIG_WRITE: &str = "AUDIO_CONFIG_WRITE";
    pub const AUDIO_REVISION_CONFLICT: &str = "AUDIO_REVISION_CONFLICT";
    pub const AUDIO_STALE_MEDIA: &str = "AUDIO_STALE_MEDIA";
    pub const AUDIO_UNSUPPORTED_COMMAND: &str = "AUDIO_UNSUPPORTED_COMMAND";
    pub const AUDIO_UNAVAILABLE: &str = "AUDIO_UNAVAILABLE";
}
