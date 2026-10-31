//! GUI-owned audio-model adapter; shared IPC remains wire-only.
use audio_model::status as model;
use ipc::audio::{validate_status, AudioError, Result};
use protocol::ipc as wire;

macro_rules! enums {
    ($decode:ident, $encode:ident, $wire:ident, $model:ident, [$($variant:ident),+]) => {
        pub fn $decode(value: i32) -> Result<model::$model> {
            match wire::$wire::try_from(value) {
                $(Ok(wire::$wire::$variant) => Ok(model::$model::$variant),)+
                _ => Err(AudioError("invalid required audio enum")),
            }
        }
        pub fn $encode(value: model::$model) -> i32 {
            match value { $(model::$model::$variant => wire::$wire::$variant as i32,)+ }
        }
    };
}
enums!(
    decode_role,
    encode_role,
    AudioRole,
    AudioRole,
    [Source, Listener]
);
enums!(
    decode_mode,
    encode_mode,
    AudioMode,
    AudioMode,
    [MixAll, FollowFocus]
);
enums!(
    decode_profile,
    encode_profile,
    AudioProfile,
    AudioProfile,
    [Ethernet, Wifi]
);
enums!(
    decode_session_state,
    encode_session_state,
    AudioSessionState,
    SessionState,
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
enums!(
    decode_health,
    encode_health,
    AudioStreamHealth,
    StreamHealth,
    [Ready, Buffering, Degraded]
);
enums!(
    decode_capability,
    encode_capability,
    AudioBackendCapability,
    BackendCapability,
    [
        SystemCapture,
        Render,
        EndpointMute,
        GlobalPtt,
        MediaSessions
    ]
);
enums!(
    decode_reason,
    encode_reason,
    AudioUnavailableReason,
    UnavailableReason,
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
enums!(
    decode_playback,
    encode_playback,
    AudioPlaybackState,
    PlaybackState,
    [Unknown, Playing, Paused, Stopped]
);
enums!(
    decode_path_diagnostic,
    encode_path_diagnostic,
    AudioPathDiagnostic,
    PathDiagnostic,
    [
        None,
        NoAuthenticatedPeerPackets,
        PeerProbesMissing,
        ProbeResponsesMissing,
        AuthenticationFailures
    ]
);

fn decode_flags(flags: &wire::AudioConsentFlags) -> model::ConsentFlags {
    model::ConsentFlags {
        allow_system_send: flags.allow_system_send,
        allow_system_receive: flags.allow_system_receive,
        allow_mic_control: flags.allow_mic_control,
        allow_media_control: flags.allow_media_control,
    }
}
fn encode_flags(flags: model::ConsentFlags) -> wire::AudioConsentFlags {
    wire::AudioConsentFlags {
        allow_system_send: flags.allow_system_send,
        allow_system_receive: flags.allow_system_receive,
        allow_mic_control: flags.allow_mic_control,
        allow_media_control: flags.allow_media_control,
    }
}

/// Converts the model subset, preserving optional numeric presence and all counters.
pub fn decode_status(status: &wire::AudioStatus) -> Result<model::AudioStatus> {
    validate_status(status)?;
    let backend = status
        .backend
        .iter()
        .map(|cap| {
            Ok(model::CapabilityAvailability {
                capability: decode_capability(cap.capability)?,
                availability: match wire::AudioAvailability::try_from(cap.availability) {
                    Ok(wire::AudioAvailability::Available) => model::Availability::Available,
                    Ok(wire::AudioAvailability::Unsupported) => {
                        model::Availability::Unsupported(decode_reason(cap.reason)?)
                    }
                    Ok(wire::AudioAvailability::Unavailable) => {
                        model::Availability::Unavailable(decode_reason(cap.reason)?)
                    }
                    _ => return Err(AudioError("invalid audio availability")),
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let sessions = status
        .sessions
        .iter()
        .map(|s| {
            let health = s
                .health
                .as_ref()
                .ok_or(AudioError("missing audio health"))?;
            let consent = s
                .consent
                .as_ref()
                .ok_or(AudioError("missing audio consent"))?;
            let media_info = s
                .media_info
                .as_ref()
                .map(|m| {
                    let commands = m
                        .commands
                        .as_ref()
                        .ok_or(AudioError("missing media commands"))?;
                    Ok::<_, AudioError>(model::MediaInfo {
                        app_id: m.app_id.clone(),
                        title: m.title.clone(),
                        subtitle: m.subtitle.clone(),
                        position_ms: m.position_ms,
                        session_token: m.session_token,
                        duration_ms: m.duration_ms,
                        playback: decode_playback(m.playback)?,
                        commands: model::MediaCommands {
                            play: commands.play,
                            pause: commands.pause,
                            toggle: commands.toggle,
                            next: commands.next,
                            previous: commands.previous,
                        },
                        revision: m.revision,
                    })
                })
                .transpose()?;
            Ok(model::SessionStatus {
                session_id: s
                    .session_id
                    .as_slice()
                    .try_into()
                    .map_err(|_| AudioError("invalid session identity"))?,
                peer: s
                    .peer
                    .as_slice()
                    .try_into()
                    .map_err(|_| AudioError("invalid peer identity"))?,
                source_name: s.source_name.clone(),
                local_role: decode_role(s.local_role)?,
                state: decode_session_state(s.state)?,
                profile: decode_profile(s.profile)?,
                health: model::AudioHealth {
                    received_packets: health.received_packets,
                    lost_frames: health.lost_frames,
                    late_frames: health.late_frames,
                    concealed_frames: health.concealed_frames,
                    underruns: health.underruns,
                    discontinuities: health.discontinuities,
                    limiter_frames: health.limiter_frames,
                },
                health_state: decode_health(s.health_state)?,
                latency_estimate_ms: s.latency_estimate_ms,
                gain_percent: s.gain_percent as u8,
                muted: s.muted,
                consent: model::SessionConsent {
                    local: decode_flags(
                        consent
                            .local
                            .as_ref()
                            .ok_or(AudioError("missing local consent"))?,
                    ),
                    remote: decode_flags(
                        consent
                            .remote
                            .as_ref()
                            .ok_or(AudioError("missing remote consent"))?,
                    ),
                },
                media_info,
                focus_primary: s.focus_primary,
                path_diagnostic: decode_path_diagnostic(s.path_diagnostic)?,
                local_udp_port: u16::try_from(s.local_udp_port)
                    .map_err(|_| AudioError("invalid audio path UDP port"))?,
                peer_udp_port: u16::try_from(s.peer_udp_port)
                    .map_err(|_| AudioError("invalid audio path UDP port"))?,
                remote_quality: s.remote_quality.as_ref().map(|q| model::RemoteQuality {
                    received_packets: q.received_packets,
                    lost_frames: q.lost_frames,
                    late_frames: q.late_frames,
                }),
                latency_stages: s
                    .latency_stages
                    .as_ref()
                    .map(|l| model::LatencyDiagnostics {
                        packetization_us: l.packetization_us,
                        capture_queue_us: l.capture_queue_us,
                        codec_lookahead_us: l.codec_lookahead_us,
                        jitter_target_us: l.jitter_target_us,
                        jitter_buffer_us: l.jitter_buffer_us,
                        decoded_queue_us: l.decoded_queue_us,
                        render_queue_us: l.render_queue_us,
                        resampler_delay_us: l.resampler_delay_us,
                        native_render_buffer_us: l.native_render_buffer_us,
                    }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(model::AudioStatus {
        revision: status.revision,
        enabled: status.enabled,
        role: decode_role(status.role)?,
        mode: decode_mode(status.mode)?,
        backend,
        sessions,
    })
}

/// Core supplies the persisted config and actual microphone state; neither is invented.
pub fn encode_status(
    status: &model::AudioStatus,
    config: wire::AudioConfig,
    microphone: wire::AudioMicrophoneState,
) -> Result<wire::AudioStatus> {
    status
        .validate()
        .map_err(|_| AudioError("invalid model audio status"))?;
    let backend = status
        .backend
        .iter()
        .map(|cap| {
            let (availability, reason) = match cap.availability {
                model::Availability::Available => (wire::AudioAvailability::Available, 0),
                model::Availability::Unsupported(reason) => {
                    (wire::AudioAvailability::Unsupported, encode_reason(reason))
                }
                model::Availability::Unavailable(reason) => {
                    (wire::AudioAvailability::Unavailable, encode_reason(reason))
                }
            };
            wire::AudioCapabilityAvailability {
                capability: encode_capability(cap.capability),
                availability: availability as i32,
                reason,
            }
        })
        .collect();
    let sessions = status
        .sessions
        .iter()
        .map(|s| wire::AudioSessionStatus {
            session_id: s.session_id.to_vec(),
            peer: s.peer.to_vec(),
            source_name: s.source_name.clone(),
            local_role: encode_role(s.local_role),
            state: encode_session_state(s.state),
            profile: encode_profile(s.profile),
            health: Some(wire::AudioHealth {
                received_packets: s.health.received_packets,
                lost_frames: s.health.lost_frames,
                late_frames: s.health.late_frames,
                concealed_frames: s.health.concealed_frames,
                underruns: s.health.underruns,
                discontinuities: s.health.discontinuities,
                limiter_frames: s.health.limiter_frames,
            }),
            health_state: encode_health(s.health_state),
            latency_estimate_ms: s.latency_estimate_ms,
            gain_percent: u32::from(s.gain_percent),
            muted: s.muted,
            consent: Some(wire::AudioSessionConsent {
                local: Some(encode_flags(s.consent.local)),
                remote: Some(encode_flags(s.consent.remote)),
            }),
            media_info: s.media_info.as_ref().map(|m| wire::MediaInfo {
                app_id: m.app_id.clone(),
                title: m.title.clone(),
                subtitle: m.subtitle.clone(),
                position_ms: m.position_ms,
                session_token: m.session_token,
                duration_ms: m.duration_ms,
                playback: encode_playback(m.playback),
                commands: Some(wire::AudioMediaCommands {
                    play: m.commands.play,
                    pause: m.commands.pause,
                    toggle: m.commands.toggle,
                    next: m.commands.next,
                    previous: m.commands.previous,
                }),
                revision: m.revision,
            }),
            focus_primary: s.focus_primary,
            path_diagnostic: encode_path_diagnostic(s.path_diagnostic),
            local_udp_port: u32::from(s.local_udp_port),
            peer_udp_port: u32::from(s.peer_udp_port),
            remote_quality: s.remote_quality.map(|q| wire::AudioRemoteQuality {
                received_packets: q.received_packets,
                lost_frames: q.lost_frames,
                late_frames: q.late_frames,
            }),
            latency_stages: s.latency_stages.map(|l| wire::AudioLatencyDiagnostics {
                packetization_us: l.packetization_us,
                capture_queue_us: l.capture_queue_us,
                codec_lookahead_us: l.codec_lookahead_us,
                jitter_target_us: l.jitter_target_us,
                jitter_buffer_us: l.jitter_buffer_us,
                decoded_queue_us: l.decoded_queue_us,
                render_queue_us: l.render_queue_us,
                resampler_delay_us: l.resampler_delay_us,
                native_render_buffer_us: l.native_render_buffer_us,
            }),
            kind: wire::AudioKind::System as i32,
            capture_active: None,
            sending: None,
        })
        .collect();
    let result = wire::AudioStatus {
        revision: status.revision,
        enabled: status.enabled,
        role: encode_role(status.role),
        mode: encode_mode(status.mode),
        backend,
        sessions,
        config: Some(config),
        microphone: Some(microphone),
    };
    validate_status(&result)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> wire::AudioConfig {
        wire::AudioConfig {
            role: wire::AudioRole::Listener as i32,
            group_id_override: Some(false),
            mode: wire::AudioMode::MixAll as i32,
            profile: wire::AudioProfile::Ethernet as i32,
            udp_port: 47903,
            output_device: "default".into(),
            capture_device: "default_monitor".into(),
            focus_debounce_ms: 250,
            focus_ramp_ms: 150,
            background_gain_db: -18,
            microphone: Some(wire::AudioMicrophoneConfig {
                capture_device: "default".into(),
                muted: true,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    #[test]
    fn config_adapter_round_trip_preserves_group_override_presence() {
        let status = model::AudioStatus {
            revision: 1,
            enabled: false,
            role: model::AudioRole::Source,
            mode: model::AudioMode::MixAll,
            backend: Vec::new(),
            sessions: Vec::new(),
        };
        for group_id_override in [None, Some(false), Some(true)] {
            let mut config = config();
            config.role = wire::AudioRole::Source as i32;
            config.listener_node_id = vec![2; 32];
            config.group_id = vec![7; 16];
            config.group_id_override = group_id_override;
            let wire = encode_status(
                &status,
                config.clone(),
                wire::AudioMicrophoneState::default(),
            )
            .unwrap();
            let model = decode_status(&wire).unwrap();
            let round_trip =
                encode_status(&model, wire.config.unwrap(), wire.microphone.unwrap()).unwrap();
            assert_eq!(round_trip.config, Some(config));
        }
    }

    #[test]
    fn round_trip_preserves_model_health_and_optional_media_numbers() {
        let mut c = config();
        c.enabled = true;
        c.group_id = vec![7; 16];
        let model = model::AudioStatus {
            revision: 42,
            enabled: true,
            role: model::AudioRole::Listener,
            mode: model::AudioMode::MixAll,
            backend: vec![model::CapabilityAvailability {
                capability: model::BackendCapability::Render,
                availability: model::Availability::Unavailable(
                    model::UnavailableReason::DeviceGone,
                ),
            }],
            sessions: vec![model::SessionStatus {
                session_id: [1; 16],
                peer: [2; 32],
                source_name: "WORKSTATION".into(),
                local_role: model::AudioRole::Listener,
                state: model::SessionState::Running,
                profile: model::AudioProfile::Wifi,
                health: model::AudioHealth {
                    received_packets: u64::MAX,
                    lost_frames: 2,
                    late_frames: 3,
                    concealed_frames: 4,
                    underruns: 5,
                    discontinuities: 6,
                    limiter_frames: 7,
                },
                health_state: model::StreamHealth::Degraded,
                latency_estimate_ms: Some(0),
                gain_percent: 73,
                muted: true,
                consent: model::SessionConsent {
                    local: model::ConsentFlags {
                        allow_system_receive: true,
                        ..Default::default()
                    },
                    remote: model::ConsentFlags {
                        allow_system_send: true,
                        allow_media_control: true,
                        ..Default::default()
                    },
                },
                media_info: Some(model::MediaInfo {
                    app_id: "private-app".into(),
                    title: "private-title".into(),
                    subtitle: "private-subtitle".into(),
                    position_ms: Some(0),
                    duration_ms: None,
                    session_token: 99,
                    revision: 4,
                    playback: model::PlaybackState::Playing,
                    commands: model::MediaCommands {
                        next: true,
                        ..Default::default()
                    },
                }),
                focus_primary: true,
                path_diagnostic: model::PathDiagnostic::None,
                local_udp_port: 0,
                peer_udp_port: 0,
                remote_quality: None,
                latency_stages: None,
            }],
        };
        let wire = encode_status(&model, c, wire::AudioMicrophoneState::default()).unwrap();
        assert_eq!(decode_status(&wire).unwrap(), model);
        let debug = format!("{wire:?}");
        for secret in ["private-app", "private-title", "private-subtitle"] {
            assert!(!debug.contains(secret));
        }
        let mut invalid = wire.clone();
        invalid.sessions[0].gain_percent = 256;
        assert!(validate_status(&invalid).is_err());
        invalid = wire.clone();
        invalid.sessions[0].state = 0;
        assert!(validate_status(&invalid).is_err());
        invalid = wire.clone();
        invalid.sessions[0]
            .consent
            .as_mut()
            .unwrap()
            .local
            .as_mut()
            .unwrap()
            .allow_system_receive = false;
        assert!(validate_status(&invalid).is_err());
        invalid = wire.clone();
        invalid.backend.push(invalid.backend[0]);
        assert!(validate_status(&invalid).is_err());
        invalid = wire.clone();
        invalid.sessions.push(invalid.sessions[0].clone());
        assert!(validate_status(&invalid).is_err());
        invalid = wire;
        invalid.sessions[0].media_info.as_mut().unwrap().title = "\u{00e9}".repeat(129);
        let error = validate_status(&invalid).unwrap_err();
        assert!(!format!("{error:?}").contains('\u{00e9}'));
    }

    #[test]
    fn round_trip_preserves_path_ports_and_remote_quality_without_changing_source_state() {
        let mut c = config();
        c.enabled = true;
        c.role = wire::AudioRole::Source as i32;
        c.listener_node_id = vec![4; 32];
        let status = model::AudioStatus {
            revision: 7,
            enabled: true,
            role: model::AudioRole::Source,
            mode: model::AudioMode::MixAll,
            backend: Vec::new(),
            sessions: vec![model::SessionStatus {
                session_id: [3; 16],
                peer: [4; 32],
                source_name: "SOURCE".into(),
                local_role: model::AudioRole::Source,
                state: model::SessionState::PathValidation,
                profile: model::AudioProfile::Ethernet,
                health: model::AudioHealth {
                    received_packets: 11,
                    ..Default::default()
                },
                health_state: model::StreamHealth::Buffering,
                latency_estimate_ms: None,
                gain_percent: 0,
                muted: false,
                consent: model::SessionConsent {
                    local: model::ConsentFlags {
                        allow_system_send: true,
                        ..Default::default()
                    },
                    remote: model::ConsentFlags::default(),
                },
                media_info: None,
                focus_primary: false,
                path_diagnostic: model::PathDiagnostic::NoAuthenticatedPeerPackets,
                local_udp_port: 47903,
                peer_udp_port: 61000,
                remote_quality: Some(model::RemoteQuality {
                    received_packets: 12,
                    lost_frames: 3,
                    late_frames: 2,
                }),
                latency_stages: Some(model::LatencyDiagnostics {
                    packetization_us: 1,
                    capture_queue_us: 2,
                    codec_lookahead_us: 3,
                    jitter_target_us: 4,
                    jitter_buffer_us: 5,
                    decoded_queue_us: 6,
                    render_queue_us: 7,
                    resampler_delay_us: 8,
                    native_render_buffer_us: None,
                }),
            }],
        };
        let encoded = encode_status(&status, c, wire::AudioMicrophoneState::default()).unwrap();
        assert_eq!(
            encoded.sessions[0].path_diagnostic,
            wire::AudioPathDiagnostic::NoAuthenticatedPeerPackets as i32
        );
        assert_eq!(encoded.sessions[0].local_udp_port, 47903);
        assert_eq!(encoded.sessions[0].peer_udp_port, 61000);
        assert_eq!(
            encoded.sessions[0].remote_quality,
            Some(wire::AudioRemoteQuality {
                received_packets: 12,
                lost_frames: 3,
                late_frames: 2,
            })
        );
        let decoded = decode_status(&encoded).unwrap();
        assert_eq!(decoded, status);
        assert_eq!(
            decoded.sessions[0].state,
            model::SessionState::PathValidation
        );
        let mut invalid = encoded.clone();
        invalid.sessions[0].path_diagnostic = 5;
        assert!(decode_status(&invalid).is_err());
        invalid = encoded;
        invalid.sessions[0].local_udp_port = 65536;
        assert!(decode_status(&invalid).is_err());
        let mut stages = status.clone();
        stages.sessions[0]
            .latency_stages
            .as_mut()
            .unwrap()
            .native_render_buffer_us = Some(0);
        let mut encoded = encode_status(
            &stages,
            invalid.config.clone().unwrap(),
            wire::AudioMicrophoneState::default(),
        )
        .unwrap();
        assert_eq!(decode_status(&encoded).unwrap(), stages);
        encoded.sessions[0]
            .latency_stages
            .as_mut()
            .unwrap()
            .native_render_buffer_us = Some(2_000_001);
        assert!(decode_status(&encoded).is_err());
    }
}
