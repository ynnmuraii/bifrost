//! Control-plane status snapshots. No device handles, samples, keys, or wall clocks.
//!
//! Core constructs these values from child reports and consent state. GUI IPC must
//! validate the same bounds before accepting a snapshot. Metadata is ephemeral user
//! content: never persist it or log it, including through Debug formatting.

/// Maximum simultaneous remote system streams at one Listener.
pub const MAX_AUDIO_SESSIONS: usize = 3;
pub const MAX_MEDIA_APP_ID_BYTES: usize = 128;
pub const MAX_MEDIA_TITLE_BYTES: usize = 256;
pub const MAX_MEDIA_SUBTITLE_BYTES: usize = 256;
pub const MAX_SOURCE_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioRole {
    #[default]
    Source,
    Listener,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioMode {
    #[default]
    MixAll,
    FollowFocus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioProfile {
    #[default]
    Ethernet,
    Wifi,
}

/// Reported lifecycle, not an implementation of the session state machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Idle,
    AwaitConsent,
    Preparing,
    AwaitCommit,
    PathValidation,
    Running,
    Rebuffering,
    DevicePaused,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCapability {
    SystemCapture,
    Render,
    EndpointMute,
    GlobalPtt,
    MediaSessions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    FeatureNotBuilt,
    NotSupportedByPlatform,
    OsBuildTooOld,
    ProcessExclusionUnavailable,
    ReleaseObservationUnavailable,
    ServerUnavailable,
    PermissionDenied,
    DeviceGone,
    DeviceNotSelected,
    WorkerUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unsupported(UnavailableReason),
    Unavailable(UnavailableReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityAvailability {
    pub capability: BackendCapability,
    pub availability: Availability,
}

/// Permissions granted by one node, not inferred from backend implementation support.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsentFlags {
    pub allow_system_send: bool,
    pub allow_system_receive: bool,
    pub allow_mic_control: bool,
    pub allow_media_control: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionConsent {
    pub local: ConsentFlags,
    pub remote: ConsentFlags,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioHealth {
    pub received_packets: u64,
    pub lost_frames: u64,
    pub late_frames: u64,
    pub concealed_frames: u64,
    pub underruns: u64,
    pub discontinuities: u64,
    pub limiter_frames: u64,
}

/// Current playout health, independent of cumulative counters and playback/mute.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamHealth {
    #[default]
    Ready,
    Buffering,
    Degraded,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlaybackState {
    #[default]
    Unknown,
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaCommands {
    pub play: bool,
    pub pause: bool,
    pub toggle: bool,
    pub next: bool,
    pub previous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
}

/// A bounded, transient now-playing snapshot, sent only for a consented live stream.
///
/// `None` position/duration means unknown (including live media), not zero.
/// Commands target this stable local token and revision, never whichever app becomes active.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MediaInfo {
    pub app_id: String,
    pub title: String,
    pub subtitle: String,
    pub position_ms: Option<u64>,
    /// Opaque token scoped to the Source's audio worker generation / active audio session.
    pub session_token: u64,
    pub duration_ms: Option<u64>,
    pub playback: PlaybackState,
    pub commands: MediaCommands,
    pub revision: u64,
}

impl std::fmt::Debug for MediaInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaInfo")
            .field("metadata", &"[REDACTED]")
            .field("playback", &self.playback)
            .field("revision", &self.revision)
            .finish()
    }
}

impl MediaInfo {
    pub fn validate(&self) -> Result<(), StatusError> {
        if self.app_id.len() > MAX_MEDIA_APP_ID_BYTES {
            return Err(StatusError::AppIdTooLong);
        }
        if self.title.len() > MAX_MEDIA_TITLE_BYTES {
            return Err(StatusError::TitleTooLong);
        }
        if self.subtitle.len() > MAX_MEDIA_SUBTITLE_BYTES {
            return Err(StatusError::SubtitleTooLong);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStatus {
    pub session_id: [u8; 16],
    pub peer: [u8; 32],
    /// Bounded authenticated peer display name, never the stream identity.
    pub source_name: String,
    pub local_role: AudioRole,
    pub state: SessionState,
    pub profile: AudioProfile,
    pub health: AudioHealth,
    pub health_state: StreamHealth,
    /// Device/network/pipeline estimate, not a clock-synchronized one-way measurement.
    pub latency_estimate_ms: Option<u32>,
    /// Listener-local stream gain; 0..=100, without boost. Muting preserves this setting.
    pub gain_percent: u8,
    pub muted: bool,
    pub consent: SessionConsent,
    pub media_info: Option<MediaInfo>,
    pub focus_primary: bool,
}

/// Authoritative, revisioned GUI snapshot. Omitted capabilities mean unavailable,
/// never permission to assume support. At most five distinct capability entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioStatus {
    pub revision: u64,
    pub enabled: bool,
    pub role: AudioRole,
    pub mode: AudioMode,
    pub backend: Vec<CapabilityAvailability>,
    pub sessions: Vec<SessionStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusError {
    TooManySessions,
    DuplicatePeer,
    GainOutOfRange,
    TooManyCapabilities,
    DuplicateCapability,
    AppIdTooLong,
    TitleTooLong,
    SubtitleTooLong,
    SourceNameTooLong,
}

impl AudioStatus {
    pub fn validate(&self) -> Result<(), StatusError> {
        if self.sessions.len() > MAX_AUDIO_SESSIONS {
            return Err(StatusError::TooManySessions);
        }
        if self.backend.len() > 5 {
            return Err(StatusError::TooManyCapabilities);
        }
        for (index, capability) in self.backend.iter().enumerate() {
            if self.backend[..index]
                .iter()
                .any(|entry| entry.capability == capability.capability)
            {
                return Err(StatusError::DuplicateCapability);
            }
        }
        for (index, session) in self.sessions.iter().enumerate() {
            if session.gain_percent > 100 {
                return Err(StatusError::GainOutOfRange);
            }
            if session.source_name.len() > MAX_SOURCE_NAME_BYTES {
                return Err(StatusError::SourceNameTooLong);
            }
            if self.sessions[..index]
                .iter()
                .any(|entry| entry.peer == session.peer)
            {
                return Err(StatusError::DuplicatePeer);
            }
            if let Some(media_info) = &session.media_info {
                media_info.validate()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(peer: u8) -> SessionStatus {
        SessionStatus {
            session_id: [peer; 16],
            peer: [peer; 32],
            source_name: "WORKSTATION".to_owned(),
            local_role: AudioRole::Listener,
            state: SessionState::Running,
            profile: AudioProfile::Ethernet,
            health: AudioHealth::default(),
            health_state: StreamHealth::Ready,
            latency_estimate_ms: None,
            gain_percent: 100,
            muted: false,
            consent: SessionConsent::default(),
            media_info: None,
            focus_primary: false,
        }
    }

    #[test]
    fn status_bounds_sessions_gain_and_capabilities() {
        let mut status = AudioStatus::default();
        assert_eq!(status.validate(), Ok(()));
        status.sessions = (0..3).map(session).collect();
        assert_eq!(status.validate(), Ok(()));
        status.sessions.push(session(3));
        assert_eq!(status.validate(), Err(StatusError::TooManySessions));
        status.sessions.pop();
        status.sessions[1].peer = status.sessions[0].peer;
        assert_eq!(status.validate(), Err(StatusError::DuplicatePeer));
        status.sessions.clear();
        let mut invalid = session(0);
        invalid.gain_percent = 101;
        status.sessions.push(invalid);
        assert_eq!(status.validate(), Err(StatusError::GainOutOfRange));
        status.sessions[0].gain_percent = 100;
        status.sessions[0].source_name = "n".repeat(MAX_SOURCE_NAME_BYTES);
        assert_eq!(status.validate(), Ok(()));
        status.sessions[0].source_name.push('n');
        assert_eq!(status.validate(), Err(StatusError::SourceNameTooLong));
        status.sessions.clear();
        let capability = CapabilityAvailability {
            capability: BackendCapability::GlobalPtt,
            availability: Availability::Unsupported(
                UnavailableReason::ReleaseObservationUnavailable,
            ),
        };
        status.backend = vec![capability; 2];
        assert_eq!(status.validate(), Err(StatusError::DuplicateCapability));
        status.backend = vec![capability; 6];
        assert_eq!(status.validate(), Err(StatusError::TooManyCapabilities));
    }

    #[test]
    fn metadata_is_byte_bounded_and_never_debugged() {
        let mut info = MediaInfo {
            app_id: "a".repeat(128),
            title: "é".repeat(128),
            subtitle: "s".repeat(256),
            ..Default::default()
        };
        assert_eq!(info.validate(), Ok(()));
        let debug = format!("{info:?}");
        assert!(!debug.contains(&info.app_id));
        assert!(!debug.contains(&info.title));
        assert!(!debug.contains(&info.subtitle));
        info.app_id.push('a');
        assert_eq!(info.validate(), Err(StatusError::AppIdTooLong));
        info.app_id.clear();
        info.title.push('a');
        assert_eq!(info.validate(), Err(StatusError::TitleTooLong));
        info.title.clear();
        info.subtitle.push('s');
        assert_eq!(info.validate(), Err(StatusError::SubtitleTooLong));
        let mut status = AudioStatus {
            sessions: vec![session(0)],
            ..Default::default()
        };
        status.sessions[0].media_info = Some(info);
        assert_eq!(status.validate(), Err(StatusError::SubtitleTooLong));
    }
}
