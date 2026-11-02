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

/// Clock actually used by the media worker, not merely a native position capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum ClockSource {
    #[default]
    Unknown = 0,
    CaptureHostTimestamp = 1,
    CaptureCallbackFrames = 2,
    RenderDevicePosition = 3,
    RenderCallbackFrames = 4,
}
impl ClockSource {
    pub const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::CaptureHostTimestamp,
            2 => Self::CaptureCallbackFrames,
            3 => Self::RenderDevicePosition,
            4 => Self::RenderCallbackFrames,
            _ => Self::Unknown,
        }
    }
}

/// Per-session PI observations; absent when that direction has no drift controller.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DriftDiagnostics {
    pub ratio_ppm: i32,
    pub filtered_fill_error_frames: f64,
    pub coarse_corrections: u64,
}

/// Bounded local stage observations in microseconds, not a synchronized network latency.
/// Jitter occupancy and target overlap; never add both to an end-to-end estimate.
/// Native output depth is unknown without a valid device playhead/host-clock snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyDiagnostics {
    pub packetization_us: u32,
    pub capture_queue_us: u32,
    pub codec_lookahead_us: u32,
    pub jitter_target_us: u32,
    pub jitter_buffer_us: u32,
    pub decoded_queue_us: u32,
    pub render_queue_us: u32,
    pub resampler_delay_us: u32,
    pub native_render_buffer_us: Option<u32>,
}
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

/// Observed path failure, without attributing it to a firewall or network policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PathDiagnostic {
    #[default]
    None,
    NoAuthenticatedPeerPackets,
    PeerProbesMissing,
    ProbeResponsesMissing,
    AuthenticationFailures,
}

/// Receiver-reported quality, separate from the Source's local pipeline state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RemoteQuality {
    pub received_packets: u64,
    pub lost_frames: u64,
    pub late_frames: u64,
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
    pub seek: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    SeekTo { position_ms: u64 },
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
    pub seek_min_ms: Option<u64>,
    pub seek_max_ms: Option<u64>,
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
        if matches!((self.position_ms, self.duration_ms),
            (Some(position), Some(duration)) if position > duration)
            || if self.commands.seek {
                self.seek_range().is_none()
            } else {
                self.seek_min_ms.is_some() || self.seek_max_ms.is_some()
            }
        {
            return Err(StatusError::InvalidMediaTimeline);
        }
        Ok(())
    }

    pub fn seek_range(&self) -> Option<(u64, u64)> {
        match (
            self.commands.seek,
            self.position_ms,
            self.duration_ms,
            self.seek_min_ms,
            self.seek_max_ms,
        ) {
            (true, Some(position), Some(duration), Some(min), Some(max))
                if duration > 0 && position <= duration && min <= max && max <= duration =>
            {
                Some((min, max))
            }
            _ => None,
        }
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
    pub path_diagnostic: PathDiagnostic,
    /// Zero means unknown, and is allowed only without a path diagnostic.
    pub local_udp_port: u16,
    pub peer_udp_port: u16,
    pub remote_quality: Option<RemoteQuality>,
    pub latency_stages: Option<LatencyDiagnostics>,
}

/// Authoritative, revisioned GUI snapshot. Omitted capabilities mean unavailable,
/// never permission to assume support. At most five distinct capability entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioStatus {
    pub revision: u64,
    pub enabled: bool,
    /// Explicit runtime request, not observed native activity. None is legacy/unknown.
    /// Source intent resets on restart unless resumed; Listener intent is receive policy.
    pub system_sharing_requested: Option<bool>,
    /// Bounded typed AUDIO_* code only; never native error text or metadata.
    /// None means no current system-sharing error.
    pub system_sharing_error: Option<String>,
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
    InvalidUdpPort,
    LatencyOutOfRange,
    InvalidSystemSharingError,
    InvalidMediaTimeline,
}

impl AudioStatus {
    pub fn validate(&self) -> Result<(), StatusError> {
        if self.system_sharing_error.as_ref().is_some_and(|code| {
            code.len() > 64
                || !code.strip_prefix("AUDIO_").is_some_and(|suffix| {
                    !suffix.is_empty()
                        && suffix.bytes().all(|byte| {
                            byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'
                        })
                })
        }) {
            return Err(StatusError::InvalidSystemSharingError);
        }
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
            if [session.local_udp_port, session.peer_udp_port]
                .into_iter()
                .any(|port| {
                    port == 47902 || (port == 0 && session.path_diagnostic != PathDiagnostic::None)
                })
            {
                return Err(StatusError::InvalidUdpPort);
            }
            if session.latency_stages.is_some_and(|latency| {
                [
                    latency.packetization_us,
                    latency.capture_queue_us,
                    latency.codec_lookahead_us,
                    latency.jitter_target_us,
                    latency.jitter_buffer_us,
                    latency.decoded_queue_us,
                    latency.render_queue_us,
                    latency.resampler_delay_us,
                    latency.native_render_buffer_us.unwrap_or(0),
                ]
                .into_iter()
                .any(|delay| delay > 2_000_000)
            }) {
                return Err(StatusError::LatencyOutOfRange);
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
            path_diagnostic: PathDiagnostic::None,
            local_udp_port: 0,
            peer_udp_port: 0,
            remote_quality: None,
            latency_stages: None,
        }
    }

    #[test]
    fn system_request_preserves_presence_without_claiming_native_activity() {
        assert_eq!(AudioStatus::default().system_sharing_requested, None);
        for requested in [None, Some(false), Some(true)] {
            let status = AudioStatus {
                system_sharing_requested: requested,
                ..Default::default()
            };
            assert_eq!(status.clone().system_sharing_requested, requested);
            assert!(!status.enabled);
            assert!(status.sessions.is_empty());
            assert_eq!(status.validate(), Ok(()));
        }
    }

    #[test]
    fn system_error_requires_bounded_typed_code_not_native_text() {
        assert_eq!(AudioStatus::default().system_sharing_error, None);
        for code in [
            "AUDIO_DEVICE_UNAVAILABLE".to_owned(),
            format!("AUDIO_{}", "X".repeat(58)),
        ] {
            let status = AudioStatus {
                system_sharing_error: Some(code),
                ..Default::default()
            };
            assert_eq!(status.validate(), Ok(()));
        }
        for code in [
            String::new(),
            "AUDIO_".to_owned(),
            "DEVICE_UNAVAILABLE".to_owned(),
            "AUDIO_native_error".to_owned(),
            "AUDIO_DEVICE unavailable".to_owned(),
            "AUDIO_ERROR\n".to_owned(),
            "AUDIO_É".to_owned(),
            format!("AUDIO_{}", "X".repeat(59)),
        ] {
            let status = AudioStatus {
                system_sharing_error: Some(code),
                ..Default::default()
            };
            assert_eq!(
                status.validate(),
                Err(StatusError::InvalidSystemSharingError)
            );
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
    fn path_diagnostics_require_usable_ports() {
        let mut status = AudioStatus {
            sessions: vec![session(0)],
            ..Default::default()
        };
        for diagnostic in [
            PathDiagnostic::None,
            PathDiagnostic::NoAuthenticatedPeerPackets,
            PathDiagnostic::PeerProbesMissing,
            PathDiagnostic::ProbeResponsesMissing,
            PathDiagnostic::AuthenticationFailures,
        ] {
            status.sessions[0].path_diagnostic = diagnostic;
            for (local, peer) in [(0, 0), (0, 47903), (47903, 0)] {
                status.sessions[0].local_udp_port = local;
                status.sessions[0].peer_udp_port = peer;
                assert_eq!(
                    status.validate(),
                    if diagnostic == PathDiagnostic::None {
                        Ok(())
                    } else {
                        Err(StatusError::InvalidUdpPort)
                    }
                );
            }
            for (local, peer) in [(1, u16::MAX), (47901, 47903)] {
                status.sessions[0].local_udp_port = local;
                status.sessions[0].peer_udp_port = peer;
                assert_eq!(status.validate(), Ok(()));
            }
            for (local, peer) in [(47902, 47903), (47903, 47902)] {
                status.sessions[0].local_udp_port = local;
                status.sessions[0].peer_udp_port = peer;
                assert_eq!(status.validate(), Err(StatusError::InvalidUdpPort));
            }
        }
    }

    #[test]
    fn receiver_quality_is_independent_of_local_health() {
        let mut status = AudioStatus {
            sessions: vec![session(0)],
            ..Default::default()
        };
        status.sessions[0].remote_quality = Some(RemoteQuality {
            received_packets: 1,
            lost_frames: u64::MAX,
            late_frames: 3,
        });
        assert_eq!(status.validate(), Ok(()));
        assert_eq!(status.sessions[0].health_state, StreamHealth::Ready);
        assert_eq!(status.sessions[0].health, AudioHealth::default());
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

    #[test]
    fn latency_stages_bound_each_observed_delay() {
        let mut status = AudioStatus {
            sessions: vec![session(0)],
            ..Default::default()
        };
        for stage in 0..9 {
            for delay in [0, 2_000_000, 2_000_001, u32::MAX] {
                let mut delays = [0; 9];
                delays[stage] = delay;
                status.sessions[0].latency_stages = Some(LatencyDiagnostics {
                    packetization_us: delays[0],
                    capture_queue_us: delays[1],
                    codec_lookahead_us: delays[2],
                    jitter_target_us: delays[3],
                    jitter_buffer_us: delays[4],
                    decoded_queue_us: delays[5],
                    render_queue_us: delays[6],
                    resampler_delay_us: delays[7],
                    native_render_buffer_us: Some(delays[8]),
                });
                assert_eq!(
                    status.validate(),
                    if delay <= 2_000_000 {
                        Ok(())
                    } else {
                        Err(StatusError::LatencyOutOfRange)
                    }
                );
            }
        }
        status.sessions[0].latency_stages = Some(LatencyDiagnostics::default());
        assert_eq!(status.validate(), Ok(()));
    }
}
