//! Pure audio boundary conversions. No native backends, I/O, or secret-bearing diagnostics.
use audio_model::{jitter::Profile, keys, session as session_model, status as model};
use config::audio as cfg;
use protocol::{ipc as wire, wire as net};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Protocol(protocol::validate::AudioViolation),
    Ipc(ipc::audio::AudioError),
    InvalidConfig,
    InvalidModel,
    Unsupported,
    InvalidObservation,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;
impl From<protocol::validate::AudioViolation> for Error {
    fn from(value: protocol::validate::AudioViolation) -> Self {
        Self::Protocol(value)
    }
}
impl From<ipc::audio::AudioError> for Error {
    fn from(value: ipc::audio::AudioError) -> Self {
        Self::Ipc(value)
    }
}
fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| Error::InvalidModel)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusControl {
    Snapshot(audio_model::focus::Snapshot),
    Request([u8; 16]),
}
pub fn is_focus(message: &net::AudioControl) -> bool {
    matches!(
        message.body,
        Some(net::audio_control::Body::Focus(_) | net::audio_control::Body::FocusRequest(_))
    )
}
pub fn decode_focus(message: &net::AudioControl) -> Result<FocusControl> {
    protocol::validate::audio_control(message)?;
    match message.body.as_ref().ok_or(Error::InvalidModel)? {
        net::audio_control::Body::Focus(value) => {
            Ok(FocusControl::Snapshot(audio_model::focus::Snapshot {
                owner: audio_model::focus::Owner {
                    node: fixed(&value.owner_node_id)?,
                    serial: value.owner_serial,
                },
                revision: value.focus_revision,
                active: if value.active_node_id.is_empty() {
                    None
                } else {
                    Some(fixed(&value.active_node_id)?)
                },
                lease: if value.lease_id.is_empty() {
                    None
                } else {
                    Some(fixed(&value.lease_id)?)
                },
                committed: value.committed,
            }))
        }
        net::audio_control::Body::FocusRequest(value) => {
            Ok(FocusControl::Request(fixed(&value.group_id)?))
        }
        _ => Err(Error::InvalidModel),
    }
}
pub fn encode_focus(snapshot: audio_model::focus::Snapshot) -> net::AudioControl {
    net::AudioControl {
        control_version: protocol::limits::AUDIO_CONTROL_VERSION,
        body: Some(net::audio_control::Body::Focus(net::AudioFocusSnapshot {
            owner_node_id: snapshot.owner.node.to_vec(),
            owner_serial: snapshot.owner.serial,
            focus_revision: snapshot.revision,
            active_node_id: snapshot.active.map_or_else(Vec::new, |id| id.to_vec()),
            lease_id: snapshot.lease.map_or_else(Vec::new, |id| id.to_vec()),
            committed: snapshot.committed,
        })),
    }
}
pub fn encode_focus_request(group: [u8; 16]) -> net::AudioControl {
    net::AudioControl {
        control_version: protocol::limits::AUDIO_CONTROL_VERSION,
        body: Some(net::audio_control::Body::FocusRequest(
            net::AudioFocusRequest {
                group_id: group.to_vec(),
            },
        )),
    }
}
pub fn format_sha256(format: keys::Format) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, &format.canonical_bytes());
    let mut result = [0; 32];
    result.copy_from_slice(digest.as_ref());
    result
}
pub fn encode_format(format: keys::Format) -> Result<net::AudioFormat> {
    let result = net::AudioFormat {
        media_version: 1,
        codec: match format.codec {
            keys::Codec::Opus => net::AudioCodec::Opus as i32,
        },
        sample_rate: format.sample_rate,
        channels: u32::from(format.channels),
        frame_samples: u32::from(format.frame_samples),
        bitrate_bps: format.bitrate,
        fec: format.fec,
        profile: match format.profile {
            Profile::Ethernet => net::AudioLatencyProfile::Ethernet as i32,
            Profile::Wifi => net::AudioLatencyProfile::Wifi as i32,
        },
        max_datagram_bytes: u32::from(format.max_datagram),
        aead: match format.aead {
            keys::Aead::ChaCha20Poly1305 => net::AudioAead::Chacha20Poly1305 as i32,
        },
    };
    protocol::validate::audio_format(&result)?;
    Ok(result)
}
pub fn decode_format(format: &net::AudioFormat) -> Result<keys::Format> {
    protocol::validate::audio_format(format)?;
    Ok(keys::Format {
        codec: keys::Codec::Opus,
        sample_rate: format.sample_rate,
        channels: u16::try_from(format.channels).map_err(|_| Error::InvalidModel)?,
        frame_samples: u16::try_from(format.frame_samples).map_err(|_| Error::InvalidModel)?,
        bitrate: format.bitrate_bps,
        fec: format.fec,
        aead: keys::Aead::ChaCha20Poly1305,
        profile: match net::AudioLatencyProfile::try_from(format.profile) {
            Ok(net::AudioLatencyProfile::Ethernet) => Profile::Ethernet,
            Ok(net::AudioLatencyProfile::Wifi) => Profile::Wifi,
            _ => return Err(Error::Unsupported),
        },
        max_datagram: u16::try_from(format.max_datagram_bytes).map_err(|_| Error::InvalidModel)?,
    })
}
macro_rules! reasons {
    ($($variant:ident),+) => {
        fn encode_stop_reason(reason: session_model::Reason) -> i32 {
            match reason { $(session_model::Reason::$variant => net::AudioStopCode::$variant as i32,)+ }
        }
        fn decode_stop_reason(reason: i32) -> Result<session_model::Reason> {
            match net::AudioStopCode::try_from(reason) {
                $(Ok(net::AudioStopCode::$variant) => Ok(session_model::Reason::$variant),)+
                _ => Err(Error::InvalidModel),
            }
        }
    };
}
reasons!(
    Disabled,
    ConsentRequired,
    RoleConflict,
    UnsupportedFormat,
    DeviceUnavailable,
    VirtualMicMissing,
    ParentLost,
    WorkerLost,
    Timeout,
    RateLimited,
    Revoked,
    UserStop,
    ProtocolViolation,
    GroupMismatch
);
fn direction(role: model::AudioRole) -> u32 {
    match role {
        model::AudioRole::Source => protocol::limits::AUDIO_DIRECTION_SEND,
        model::AudioRole::Listener => protocol::limits::AUDIO_DIRECTION_RECEIVE,
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlExtras {
    None,
    Hello {
        udp_port: u16,
        media_versions: Vec<u32>,
    },
    Commit {
        start_muted: bool,
    },
}
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedControl {
    Control {
        message: session_model::Control,
        extras: ControlExtras,
    },
    Report(net::AudioReport),
}
/// The connection ID is the actual authenticated envelope/exporter parent, not peer input.
pub fn decode_control(
    message: &net::AudioControl,
    remote_role: model::AudioRole,
    parent_connection_id: [u8; 16],
) -> Result<DecodedControl> {
    use net::audio_control::Body;
    use session_model::Control;
    protocol::validate::audio_control(message)?;
    let mut extras = ControlExtras::None;
    let message = match message.body.as_ref().ok_or(Error::InvalidModel)? {
        Body::Hello(h) => {
            let role = match net::AudioNodeRole::try_from(h.role) {
                Ok(net::AudioNodeRole::Source) => model::AudioRole::Source,
                Ok(net::AudioNodeRole::Listener) => model::AudioRole::Listener,
                _ => return Err(Error::Unsupported),
            };
            if role != remote_role || h.directions & !direction(role) != 0 {
                return Err(Error::Unsupported);
            }
            extras = ControlExtras::Hello {
                udp_port: u16::try_from(h.udp_port).map_err(|_| Error::InvalidModel)?,
                media_versions: h.media_versions.clone(),
            };
            Control::Hello(session_model::Hello {
                role,
                group: if h.group_id.is_empty() {
                    None
                } else {
                    Some(fixed(&h.group_id)?)
                },
                listener: fixed(&h.listener_node_id)?,
                available: h.listener_available,
                allowed: h.directions != 0,
                consent_revision: h.consent_revision,
            })
        }
        Body::Offer(o) => {
            if o.parent_connection_id != parent_connection_id {
                return Err(Error::Protocol(
                    protocol::validate::AudioViolation::ParentConnection,
                ));
            }
            let format = decode_format(o.format.as_ref().ok_or(Error::InvalidModel)?)?;
            Control::Offer(session_model::Offer {
                session: fixed(&o.session_id)?,
                listener_nonce: fixed(&o.session_nonce)?,
                parent_connection_id,
                group: fixed(&o.group_id)?,
                listener: fixed(&o.listener_node_id)?,
                source: fixed(&o.source_node_id)?,
                format,
                format_sha256: format_sha256(format),
                listener_port: u16::try_from(o.udp_port).map_err(|_| Error::InvalidModel)?,
                consent_revision: o.consent_revision,
                request_id: if o.request_id.is_empty() {
                    None
                } else {
                    Some(fixed(&o.request_id)?)
                },
            })
        }
        Body::Answer(a) => Control::Answer(session_model::Answer {
            session: fixed(&a.session_id)?,
            accepted: a.accepted,
            reason: if a.accepted {
                None
            } else {
                Some(decode_stop_reason(a.reason)?)
            },
            format: a.accepted_format.as_ref().map(decode_format).transpose()?,
            source_nonce: if a.source_nonce.is_empty() {
                None
            } else {
                Some(fixed(&a.source_nonce)?)
            },
            source_port: u16::try_from(a.udp_port).map_err(|_| Error::InvalidModel)?,
            consent_revision: a.consent_revision,
        }),
        Body::Commit(c) => {
            extras = ControlExtras::Commit {
                start_muted: c.start_muted,
            };
            Control::Commit {
                session: fixed(&c.session_id)?,
                transcript_sha256: fixed(&c.transcript_sha256)?,
            }
        }
        Body::Stop(s) => Control::Stop {
            session: fixed(&s.session_id)?,
            reason: decode_stop_reason(s.reason)?,
        },
        Body::Keepalive(k) => Control::Keepalive {
            session: fixed(&k.session_id)?,
            serial: k.control_serial,
            authorized_for_ms: k.authorized_for_ms,
        },
        Body::Consent(c) => {
            if c.allowed_directions & !direction(remote_role) != 0 {
                return Err(Error::Unsupported);
            }
            Control::Consent {
                group: fixed(&c.group_id)?,
                revision: c.consent_revision,
                allowed: c.allowed_directions != 0,
            }
        }
        Body::Report(r) => return Ok(DecodedControl::Report(r.clone())),
        Body::StartRequest(r) => Control::StartRequest(session_model::StartRequest {
            request_id: fixed(&r.request_id)?,
            group: fixed(&r.group_id)?,
            source: fixed(&r.source_node_id)?,
            consent_revision: r.consent_revision,
            kind: match net::AudioStreamKind::try_from(r.kind) {
                Ok(net::AudioStreamKind::System) => keys::StreamKind::System,
                _ => return Err(Error::Unsupported),
            },
            profile: match net::AudioLatencyProfile::try_from(r.profile) {
                Ok(net::AudioLatencyProfile::Ethernet) => Profile::Ethernet,
                Ok(net::AudioLatencyProfile::Wifi) => Profile::Wifi,
                _ => return Err(Error::Unsupported),
            },
        }),
        Body::StartRefusal(r) => Control::StartRefusal {
            request_id: fixed(&r.request_id)?,
            reason: decode_stop_reason(r.reason)?,
        },
        Body::Focus(_)
        | Body::FocusRequest(_)
        | Body::MicControl(_)
        | Body::MicState(_)
        | Body::MicAck(_)
        | Body::MediaInfo(_)
        | Body::MediaCommand(_)
        | Body::MediaAck(_) => return Err(Error::Unsupported),
    };
    Ok(DecodedControl::Control { message, extras })
}
/// Model-originated Hello advertises V1; Commit starts unmuted unless local policy gates it.
pub fn encode_control(
    message: &session_model::Control,
    local_role: model::AudioRole,
    hello_port: u16,
    parent_connection_id: [u8; 16],
) -> Result<net::AudioControl> {
    use net::audio_control::Body;
    use session_model::Control;
    let body = match *message {
        Control::Hello(h) => {
            if h.role != local_role {
                return Err(Error::InvalidModel);
            }
            Body::Hello(net::AudioHello {
                group_id: h.group.map(|g| g.to_vec()).unwrap_or_default(),
                listener_node_id: h.listener.to_vec(),
                role: match h.role {
                    model::AudioRole::Source => net::AudioNodeRole::Source as i32,
                    model::AudioRole::Listener => net::AudioNodeRole::Listener as i32,
                },
                udp_port: u32::from(hello_port),
                media_versions: vec![1],
                consent_revision: h.consent_revision,
                directions: if h.allowed { direction(h.role) } else { 0 },
                listener_available: h.available,
            })
        }
        Control::Offer(o) => {
            if o.parent_connection_id != parent_connection_id {
                return Err(Error::Protocol(
                    protocol::validate::AudioViolation::ParentConnection,
                ));
            }
            if o.format_sha256 != format_sha256(o.format) {
                return Err(Error::InvalidModel);
            }
            Body::Offer(net::AudioOffer {
                session_id: o.session.to_vec(),
                session_nonce: o.listener_nonce.to_vec(),
                parent_connection_id: o.parent_connection_id.to_vec(),
                group_id: o.group.to_vec(),
                listener_node_id: o.listener.to_vec(),
                source_node_id: o.source.to_vec(),
                kind: net::AudioStreamKind::System as i32,
                format: Some(encode_format(o.format)?),
                udp_port: u32::from(o.listener_port),
                consent_revision: o.consent_revision,
                key_epoch: 1,
                control_ttl_ms: session_model::PEER_TTL_MS,
                request_id: o.request_id.map(|id| id.to_vec()).unwrap_or_default(),
            })
        }
        Control::Answer(a) => {
            if a.accepted != a.reason.is_none() {
                return Err(Error::InvalidModel);
            }
            Body::Answer(net::AudioAnswer {
                session_id: a.session.to_vec(),
                accepted: a.accepted,
                reason: a.reason.map(encode_stop_reason).unwrap_or(0),
                accepted_format: a.format.map(encode_format).transpose()?,
                udp_port: u32::from(a.source_port),
                consent_revision: a.consent_revision,
                source_nonce: a.source_nonce.map(|n| n.to_vec()).unwrap_or_default(),
            })
        }
        Control::Commit {
            session,
            transcript_sha256,
        } => Body::Commit(net::AudioCommit {
            session_id: session.to_vec(),
            transcript_sha256: transcript_sha256.to_vec(),
            key_epoch: 1,
            start_muted: false,
        }),
        Control::Stop { session, reason } => Body::Stop(net::AudioStop {
            session_id: session.to_vec(),
            reason: encode_stop_reason(reason),
        }),
        Control::Keepalive {
            session,
            serial,
            authorized_for_ms,
        } => Body::Keepalive(net::AudioKeepalive {
            session_id: session.to_vec(),
            control_serial: serial,
            authorized_for_ms,
        }),
        Control::Consent {
            group,
            revision,
            allowed,
        } => Body::Consent(net::AudioConsentChanged {
            group_id: group.to_vec(),
            consent_revision: revision,
            allowed_directions: if allowed { direction(local_role) } else { 0 },
        }),
        Control::StartRequest(r) => Body::StartRequest(net::AudioStartRequest {
            request_id: r.request_id.to_vec(),
            group_id: r.group.to_vec(),
            source_node_id: r.source.to_vec(),
            consent_revision: r.consent_revision,
            kind: match r.kind {
                keys::StreamKind::System => net::AudioStreamKind::System as i32,
                keys::StreamKind::Microphone => return Err(Error::Unsupported),
            },
            profile: match r.profile {
                Profile::Ethernet => net::AudioLatencyProfile::Ethernet as i32,
                Profile::Wifi => net::AudioLatencyProfile::Wifi as i32,
            },
        }),
        Control::StartRefusal { request_id, reason } => {
            Body::StartRefusal(net::AudioStartRefusal {
                request_id: request_id.to_vec(),
                reason: encode_stop_reason(reason),
            })
        }
    };
    let result = net::AudioControl {
        control_version: 1,
        body: Some(body),
    };
    protocol::validate::audio_control(&result)?;
    Ok(result)
}

fn encode_device(selector: &cfg::AudioDeviceSelector) -> Result<String> {
    match selector {
        cfg::AudioDeviceSelector::FollowDefault {
            role: cfg::DefaultDeviceRole::Multimedia,
        } => Ok("default".into()),
        cfg::AudioDeviceSelector::FollowDefault {
            role: cfg::DefaultDeviceRole::Communications,
        } => Ok("default:communications".into()),
        cfg::AudioDeviceSelector::StableId { id }
            if id != "default" && id != "default:communications" =>
        {
            Ok(id.clone())
        }
        _ => Err(Error::Unsupported),
    }
}
fn decode_device(selector: &str) -> cfg::AudioDeviceSelector {
    match selector {
        "default" => cfg::AudioDeviceSelector::FollowDefault {
            role: cfg::DefaultDeviceRole::Multimedia,
        },
        "default:communications" => cfg::AudioDeviceSelector::FollowDefault {
            role: cfg::DefaultDeviceRole::Communications,
        },
        _ => cfg::AudioDeviceSelector::StableId {
            id: selector.into(),
        },
    }
}
/// Auto has no wire discriminant; the caller supplies its concrete display policy.
pub fn encode_config(
    config: &cfg::AudioConfig,
    auto_profile: model::AudioProfile,
) -> Result<wire::AudioConfig> {
    config.validate().map_err(|_| Error::InvalidConfig)?;
    if config.background_gain_db.fract() != 0.0 {
        return Err(Error::Unsupported);
    }
    let ids = config
        .allow_send_to
        .iter()
        .chain(&config.allow_system_audio_from)
        .chain(config.peers.iter().map(|p| &p.node_id))
        .copied()
        .collect::<BTreeSet<_>>();
    let profile = match config.profile {
        cfg::AudioProfile::Auto => auto_profile,
        cfg::AudioProfile::Ethernet => model::AudioProfile::Ethernet,
        cfg::AudioProfile::Wifi => model::AudioProfile::Wifi,
    };
    let mic = &config.microphone;
    let result = wire::AudioConfig {
        enabled: config.enabled,
        role: match config.role {
            cfg::AudioRole::Off | cfg::AudioRole::Source => wire::AudioRole::Source as i32,
            cfg::AudioRole::Listener => wire::AudioRole::Listener as i32,
        },
        listener_node_id: config
            .listener_node_id
            .map(|id| id.0.to_vec())
            .unwrap_or_default(),
        group_id: config.group_id.map(|id| id.0.to_vec()).unwrap_or_default(),
        group_id_override: config.group_id_override,
        system_sharing: config.system_sharing,
        mode: match config.mode {
            cfg::AudioMode::MixAll => wire::AudioMode::MixAll as i32,
            cfg::AudioMode::FollowFocus => wire::AudioMode::FollowFocus as i32,
        },
        udp_port: u32::from(config.media_port),
        profile: encode_profile(profile),
        output_device: encode_device(&config.output_device)?,
        output_fallback_to_default: config.output_fallback_to_default,
        capture_device: match &config.capture_device {
            cfg::SystemCaptureSelector::DefaultMonitor {} => "default_monitor".into(),
            cfg::SystemCaptureSelector::StableId { id } if id != "default_monitor" => id.clone(),
            _ => return Err(Error::Unsupported),
        },
        resume_system_audio: config.resume_system_audio,
        focus_debounce_ms: config.focus_debounce_ms,
        focus_ramp_ms: config.focus_ramp_ms,
        background_gain_db: config.background_gain_db as i32,
        background_mute: config.background_mute,
        microphone: Some(wire::AudioMicrophoneConfig {
            control_enabled: mic.control_enabled,
            streaming_enabled: mic.streaming_enabled,
            experimental_acknowledged: mic.experimental_acknowledged,
            capture_device: encode_device(&mic.capture_device)?,
            allow_default_mic_changes: mic.allow_default_mic_changes,
            virtual_device: mic.virtual_device.clone(),
            muted: mic.muted,
            ptt_enabled: mic.ptt_enabled,
            ptt_hotkey: mic.ptt_hotkey.clone(),
            pinned_target_node_id: mic
                .pinned_target_node_id
                .map(|id| id.0.to_vec())
                .unwrap_or_default(),
        }),
        peers: ids
            .into_iter()
            .map(|id| {
                let preference = config.peers.iter().find(|peer| peer.node_id == id);
                wire::AudioPeerConfig {
                    node_id: id.0.to_vec(),
                    allow_system_send: config.allow_send_to.contains(&id),
                    allow_system_receive: config.allow_system_audio_from.contains(&id),
                    allow_mic_send: false,
                    allow_mic_receive: false,
                    allow_mic_control: preference.is_some_and(|peer| peer.allow_mic_control),
                    allow_media_control: preference.is_some_and(|peer| peer.allow_media_control),
                    volume_percent: preference.map_or(100, |peer| peer.gain_percent),
                    system_send_override: preference.and_then(|peer| peer.system_send_override),
                    system_receive_override: preference
                        .and_then(|peer| peer.system_receive_override),
                }
            })
            .collect(),
    };
    ipc::audio::validate_config(&result)?;
    Ok(result)
}

/// Preserve wire-inexpressible preferences, never invent consent from trust or defaults.
pub fn decode_config(
    value: &wire::AudioConfig,
    previous: &cfg::AudioConfig,
    local: [u8; 32],
    trusted: &BTreeSet<cfg::AudioNodeId>,
    auto_profile: model::AudioProfile,
) -> Result<cfg::AudioConfig> {
    let mic = value.microphone.as_ref().ok_or(Error::InvalidConfig)?;
    if mic.streaming_enabled
        || !mic.pinned_target_node_id.is_empty()
        || !mic.virtual_device.is_empty()
        || mic.allow_default_mic_changes
        || value
            .peers
            .iter()
            .any(|p| p.allow_mic_send || p.allow_mic_receive)
    {
        return Err(Error::Unsupported);
    }
    ipc::audio::validate_config(value)?;
    let wire_role = ipc::audio::decode_role(value.role)?;
    let mut result = cfg::AudioConfig {
        enabled: value.enabled,
        system_sharing: value.system_sharing,
        role: match wire_role {
            wire::AudioRole::Source
                if previous.role == cfg::AudioRole::Off
                    && !value.enabled
                    && value.listener_node_id.as_slice()
                        == previous
                            .listener_node_id
                            .as_ref()
                            .map_or(&[][..], |id| id.0.as_slice())
                    && value.group_id.as_slice()
                        == previous
                            .group_id
                            .as_ref()
                            .map_or(&[][..], |id| id.0.as_slice())
                    && value.peers.iter().all(|p| {
                        !p.allow_system_send && !p.allow_system_receive && !p.allow_media_control
                    }) =>
            {
                cfg::AudioRole::Off
            }
            wire::AudioRole::Source => cfg::AudioRole::Source,
            wire::AudioRole::Listener => cfg::AudioRole::Listener,
            _ => return Err(Error::InvalidConfig),
        },
        group_id: if value.group_id.is_empty() {
            if wire_role == wire::AudioRole::Listener && previous.role == cfg::AudioRole::Listener {
                previous.group_id
            } else {
                None
            }
        } else {
            Some(cfg::HexId(fixed(&value.group_id)?))
        },
        group_id_override: value.group_id_override,
        listener_node_id: if value.listener_node_id.is_empty() {
            None
        } else {
            Some(cfg::HexId(fixed(&value.listener_node_id)?))
        },
        media_port: u16::try_from(value.udp_port).map_err(|_| Error::InvalidConfig)?,
        profile: match ipc::audio::decode_profile(value.profile)? {
            p if previous.profile == cfg::AudioProfile::Auto
                && p as i32 == encode_profile(auto_profile) =>
            {
                cfg::AudioProfile::Auto
            }
            wire::AudioProfile::Ethernet => cfg::AudioProfile::Ethernet,
            wire::AudioProfile::Wifi => cfg::AudioProfile::Wifi,
            _ => return Err(Error::InvalidConfig),
        },
        mode: match ipc::audio::decode_mode(value.mode)? {
            wire::AudioMode::MixAll => cfg::AudioMode::MixAll,
            wire::AudioMode::FollowFocus => cfg::AudioMode::FollowFocus,
            _ => return Err(Error::InvalidConfig),
        },
        allow_system_audio_from: Vec::new(),
        allow_send_to: Vec::new(),
        peers: Vec::new(),
        output_device: decode_device(&value.output_device),
        output_fallback_to_default: value.output_fallback_to_default,
        capture_device: if value.capture_device == "default_monitor" {
            cfg::SystemCaptureSelector::DefaultMonitor {}
        } else {
            cfg::SystemCaptureSelector::StableId {
                id: value.capture_device.clone(),
            }
        },
        resume_system_audio: value.resume_system_audio,
        focus_debounce_ms: value.focus_debounce_ms,
        focus_ramp_ms: value.focus_ramp_ms,
        background_gain_db: value.background_gain_db as f32,
        background_mute: value.background_mute,
        microphone: cfg::AudioMicrophoneConfig {
            control_enabled: mic.control_enabled,
            streaming_enabled: mic.streaming_enabled,
            experimental_acknowledged: mic.experimental_acknowledged,
            capture_device: decode_device(&mic.capture_device),
            allow_default_mic_changes: mic.allow_default_mic_changes,
            virtual_device: mic.virtual_device.clone(),
            muted: mic.muted,
            ptt_enabled: mic.ptt_enabled,
            ptt_hotkey: mic.ptt_hotkey.clone(),
            pinned_target_node_id: None,
        },
    };
    if result.role == cfg::AudioRole::Source {
        let same_listener = previous.role == cfg::AudioRole::Source
            && result.listener_node_id == previous.listener_node_id;
        if value.group_id.is_empty() && same_listener {
            result.group_id = previous.group_id;
        }
    }
    for peer in &value.peers {
        let id = cfg::HexId(fixed(&peer.node_id)?);
        if peer.allow_system_send {
            result.allow_send_to.push(id);
        }
        if peer.allow_system_receive {
            result.allow_system_audio_from.push(id);
        }
        let prior = previous.peers.iter().find(|p| p.node_id == id);
        result.peers.push(cfg::AudioPeerConfig {
            node_id: id,
            system_send_override: peer.system_send_override,
            system_receive_override: peer.system_receive_override,
            allow_media_control: peer.allow_media_control,
            allow_mic_control: peer.allow_mic_control,
            gain_percent: peer.volume_percent,
            muted: result.role == cfg::AudioRole::Listener && prior.is_some_and(|p| p.muted),
        });
        if id.0 == local || !trusted.contains(&id) {
            return Err(Error::InvalidConfig);
        }
    }
    result
        .validate_identity(cfg::HexId(local), trusted)
        .map_err(|_| Error::InvalidConfig)?;
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionObservation {
    pub session: [u8; 16],
    pub peer: [u8; 32],
    pub capture_active: Option<bool>,
    pub sending: Option<bool>,
}

macro_rules! status_enum {
    ($function:ident, $wire:ident, $model:ident, [$($variant:ident),+]) => {
        pub fn $function(value: model::$model) -> i32 {
            match value { $(model::$model::$variant => wire::$wire::$variant as i32,)+ }
        }
    };
}
status_enum!(encode_role, AudioRole, AudioRole, [Source, Listener]);
status_enum!(encode_mode, AudioMode, AudioMode, [MixAll, FollowFocus]);
status_enum!(encode_profile, AudioProfile, AudioProfile, [Ethernet, Wifi]);
status_enum!(
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
status_enum!(
    encode_health,
    AudioStreamHealth,
    StreamHealth,
    [Ready, Buffering, Degraded]
);
status_enum!(
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
status_enum!(
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
status_enum!(
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
status_enum!(
    encode_playback,
    AudioPlaybackState,
    PlaybackState,
    [Unknown, Playing, Paused, Stopped]
);
fn encode_flags(flags: model::ConsentFlags) -> wire::AudioConsentFlags {
    wire::AudioConsentFlags {
        allow_system_send: flags.allow_system_send,
        allow_system_receive: flags.allow_system_receive,
        allow_mic_control: flags.allow_mic_control,
        allow_media_control: flags.allow_media_control,
    }
}
/// Config is the real normalized persisted config; observations are worker facts, not lifecycle guesses.
pub fn encode_status(
    status: &model::AudioStatus,
    config: wire::AudioConfig,
    microphone: wire::AudioMicrophoneState,
    observations: &[SessionObservation],
) -> Result<wire::AudioStatus> {
    status.validate().map_err(|_| Error::InvalidModel)?;
    if observations.len() > status.sessions.len() {
        return Err(Error::InvalidObservation);
    }
    for (index, observation) in observations.iter().enumerate() {
        if !status
            .sessions
            .iter()
            .any(|s| s.session_id == observation.session && s.peer == observation.peer)
            || observations[..index]
                .iter()
                .any(|o| o.session == observation.session || o.peer == observation.peer)
        {
            return Err(Error::InvalidObservation);
        }
    }
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
        .map(|s| {
            let observed = observations
                .iter()
                .find(|o| o.session == s.session_id && o.peer == s.peer);
            wire::AudioSessionStatus {
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
                latency_stages: s
                    .latency_stages
                    .map(|stages| wire::AudioLatencyDiagnostics {
                        packetization_us: stages.packetization_us,
                        capture_queue_us: stages.capture_queue_us,
                        codec_lookahead_us: stages.codec_lookahead_us,
                        jitter_target_us: stages.jitter_target_us,
                        jitter_buffer_us: stages.jitter_buffer_us,
                        decoded_queue_us: stages.decoded_queue_us,
                        render_queue_us: stages.render_queue_us,
                        resampler_delay_us: stages.resampler_delay_us,
                        native_render_buffer_us: stages.native_render_buffer_us,
                    }),
                path_diagnostic: encode_path_diagnostic(s.path_diagnostic),
                local_udp_port: u32::from(s.local_udp_port),
                peer_udp_port: u32::from(s.peer_udp_port),
                remote_quality: s.remote_quality.map(|quality| wire::AudioRemoteQuality {
                    received_packets: quality.received_packets,
                    lost_frames: quality.lost_frames,
                    late_frames: quality.late_frames,
                }),
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
                kind: wire::AudioKind::System as i32,
                capture_active: observed.and_then(|o| o.capture_active),
                sending: observed.and_then(|o| o.sending),
            }
        })
        .collect();
    let result = wire::AudioStatus {
        revision: status.revision,
        system_sharing_requested: status.system_sharing_requested,
        system_sharing_error: status.system_sharing_error.clone().unwrap_or_default(),
        enabled: status.enabled,
        role: encode_role(status.role),
        mode: encode_mode(status.mode),
        backend,
        sessions,
        config: Some(config),
        microphone: Some(microphone),
    };
    ipc::audio::validate_status(&result)?;
    Ok(result)
}

/// Re-encoding decoded controls retains the wire-only P1 fields absent from the FSM.
#[cfg(test)]
pub fn encode_decoded_control(
    decoded: &DecodedControl,
    role: model::AudioRole,
    parent_connection_id: [u8; 16],
) -> Result<net::AudioControl> {
    let result = match decoded {
        DecodedControl::Report(report) => net::AudioControl {
            control_version: 1,
            body: Some(net::audio_control::Body::Report(report.clone())),
        },
        DecodedControl::Control { message, extras } => {
            let port = match extras {
                ControlExtras::Hello { udp_port, .. } => *udp_port,
                _ => 1,
            };
            let mut encoded = encode_control(message, role, port, parent_connection_id)?;
            match (encoded.body.as_mut(), extras) {
                (
                    Some(net::audio_control::Body::Hello(h)),
                    ControlExtras::Hello { media_versions, .. },
                ) => h.media_versions.clone_from(media_versions),
                (
                    Some(net::audio_control::Body::Commit(c)),
                    ControlExtras::Commit { start_muted },
                ) => c.start_muted = *start_muted,
                (_, ControlExtras::None) => {}
                _ => return Err(Error::InvalidModel),
            }
            encoded
        }
    };
    protocol::validate::audio_control(&result)?;
    Ok(result)
}

/// System configuration/start/gain CAS shares a conservative control domain:
/// preferences (including consent/device policy), availability, microphone authority,
/// and session identity/lifecycle/consent. Telemetry, media metadata and focus-derived
/// gain are observations, not new user intent. Media and microphone commands retain
/// their separate token/revision guards.
pub fn same_control_state(old: &wire::AudioStatus, new: &wire::AudioStatus) -> bool {
    fn lifecycle(state: i32) -> i32 {
        if state == wire::AudioSessionState::Rebuffering as i32 {
            wire::AudioSessionState::Running as i32
        } else {
            state
        }
    }
    old.enabled == new.enabled
        && old.role == new.role
        && old.mode == new.mode
        && old.system_sharing_requested == new.system_sharing_requested
        && old.config == new.config
        && old.backend == new.backend
        && old.microphone == new.microphone
        && old.sessions.len() == new.sessions.len()
        && old.sessions.iter().zip(&new.sessions).all(|(old, new)| {
            old.session_id == new.session_id
                && old.peer == new.peer
                && old.local_role == new.local_role
                && old.profile == new.profile
                && old.kind == new.kind
                && lifecycle(old.state) == lifecycle(new.state)
                && old.consent == new.consent
                && old.muted == new.muted
        })
}

/// A published snapshot remains a valid CAS base only within its unchanged control
/// interval. The monotonic barrier also rejects ABA; payload equality cannot revive it.
pub fn control_revision_matches(expected: u64, current: u64, changed: u64) -> bool {
    expected != 0 && current != u64::MAX && changed <= expected && expected <= current
}

#[cfg(test)]
mod tests {
    use super::*;
    fn format() -> keys::Format {
        keys::Format {
            codec: keys::Codec::Opus,
            sample_rate: 48_000,
            channels: 2,
            frame_samples: 480,
            bitrate: 96_000,
            fec: false,
            aead: keys::Aead::ChaCha20Poly1305,
            profile: Profile::Ethernet,
            max_datagram: 1_200,
        }
    }
    fn offer() -> session_model::Offer {
        session_model::Offer {
            session: [1; 16],
            listener_nonce: [2; 32],
            parent_connection_id: [3; 16],
            group: [4; 16],
            listener: [5; 32],
            source: [6; 32],
            format: format(),
            format_sha256: format_sha256(format()),
            listener_port: 47903,
            consent_revision: 7,
            request_id: None,
        }
    }
    #[test]
    fn canonical_format_hash_and_context_use_one_authoritative_encoding() {
        let encoded = encode_control(
            &session_model::Control::Offer(offer()),
            model::AudioRole::Listener,
            47903,
            [3; 16],
        )
        .unwrap();
        let DecodedControl::Control {
            message: session_model::Control::Offer(decoded),
            ..
        } = decode_control(&encoded, model::AudioRole::Listener, [3; 16]).unwrap()
        else {
            panic!("offer");
        };
        assert_eq!(
            decoded.format_sha256,
            [
                0xf1, 0x47, 0x25, 0x51, 0x24, 0x0d, 0x99, 0xe6, 0x1f, 0xf3, 0xe4, 0x6d, 0x94, 0x6c,
                0x21, 0xdb, 0xb1, 0x79, 0xf4, 0xa8, 0x46, 0xba, 0x5c, 0x58, 0x3a, 0xdd, 0x09, 0x13,
                0x6b, 0xb5, 0x9a, 0xee,
            ]
        );
        let agreement = session_model::Agreement {
            offer: decoded,
            source_nonce: [8; 32],
            source_port: 47904,
            source_consent_revision: 9,
        };
        let context = agreement.context().encode();
        assert_eq!(context.len(), 225);
        assert_eq!(&context[..16], b"bifrost-audio-v1");
        assert_eq!(&context[16..32], &[3; 16]);
        assert_eq!(&context[193..], &decoded.format_sha256);
        let correlated = session_model::Agreement {
            offer: session_model::Offer {
                request_id: Some([11; 16]),
                ..decoded
            },
            ..agreement
        };
        assert_eq!(correlated.context().encode(), context);
        let mut malformed = encoded.clone();
        let Some(net::audio_control::Body::Offer(invalid_offer)) = malformed.body.as_mut() else {
            panic!("offer");
        };
        invalid_offer.request_id = vec![11; 15];
        assert_eq!(
            decode_control(&malformed, model::AudioRole::Listener, [3; 16]),
            Err(Error::Protocol(
                protocol::validate::AudioViolation::Identifier
            ))
        );
        assert!(matches!(
            decode_control(&encoded, model::AudioRole::Listener, [0; 16]),
            Err(Error::Protocol(
                protocol::validate::AudioViolation::ParentConnection
            ))
        ));
        let mut bad = offer();
        bad.format_sha256 = [0; 32];
        assert_eq!(
            encode_control(
                &session_model::Control::Offer(bad),
                model::AudioRole::Listener,
                47903,
                [3; 16]
            ),
            Err(Error::InvalidModel)
        );
    }
    #[test]
    fn representative_controls_roundtrip_without_wire_field_loss() {
        use session_model::Control;
        let controls = [
            Control::Hello(session_model::Hello {
                role: model::AudioRole::Listener,
                group: Some([4; 16]),
                listener: [5; 32],
                available: true,
                allowed: true,
                consent_revision: 9,
            }),
            Control::Offer(offer()),
            Control::Offer(session_model::Offer {
                request_id: Some([11; 16]),
                ..offer()
            }),
            Control::Answer(session_model::Answer {
                session: [1; 16],
                accepted: true,
                reason: None,
                format: Some(format()),
                source_nonce: Some([8; 32]),
                source_port: 47904,
                consent_revision: 10,
            }),
            Control::Answer(session_model::Answer {
                session: [1; 16],
                accepted: false,
                reason: Some(session_model::Reason::ConsentRequired),
                format: None,
                source_nonce: None,
                source_port: 0,
                consent_revision: 10,
            }),
            Control::Commit {
                session: [1; 16],
                transcript_sha256: [9; 32],
            },
            Control::Stop {
                session: [1; 16],
                reason: session_model::Reason::Revoked,
            },
            Control::Keepalive {
                session: [1; 16],
                serial: u64::MAX,
                authorized_for_ms: 3000,
            },
            Control::Consent {
                group: [4; 16],
                revision: 42,
                allowed: true,
            },
        ];
        for control in controls {
            let mut encoded =
                encode_control(&control, model::AudioRole::Listener, 47903, [3; 16]).unwrap();
            match encoded.body.as_mut().unwrap() {
                net::audio_control::Body::Hello(h) => h.media_versions.push(2),
                net::audio_control::Body::Commit(c) => c.start_muted = true,
                _ => {}
            }
            let decoded = decode_control(&encoded, model::AudioRole::Listener, [3; 16]).unwrap();
            assert!(
                matches!(&decoded, DecodedControl::Control { message, .. } if message == &control)
            );
            assert_eq!(
                encode_decoded_control(&decoded, model::AudioRole::Listener, [3; 16]).unwrap(),
                encoded
            );
        }
        let report = net::AudioControl {
            control_version: 1,
            body: Some(net::audio_control::Body::Report(net::AudioReport {
                session_id: vec![1; 16],
                received_packets: 8,
                lost_packets: 2,
                buffer_ms: 40,
                underruns: 3,
                suggested_loss_pct: 20,
                device_state: net::AudioDeviceState::Ready as i32,
            })),
        };
        assert!(matches!(
            decode_control(&report, model::AudioRole::Listener, [3; 16]).unwrap(),
            DecodedControl::Report(_)
        ));
        let mut invalid = encode_format(format()).unwrap();
        invalid.codec = 99;
        assert!(decode_format(&invalid).is_err());
    }
    #[test]
    fn config_roundtrip_preserves_explicit_consent_and_private_preferences() {
        let peer = cfg::HexId([2; 32]);
        let config = cfg::AudioConfig {
            enabled: true,
            role: cfg::AudioRole::Listener,
            group_id: Some(cfg::HexId([3; 16])),
            allow_system_audio_from: vec![peer],
            peers: vec![cfg::AudioPeerConfig {
                node_id: peer,
                system_send_override: None,
                system_receive_override: None,
                allow_media_control: false,
                allow_mic_control: false,
                gain_percent: 37,
                muted: true,
            }],
            output_device: cfg::AudioDeviceSelector::StableId {
                id: "stable-output".into(),
            },
            capture_device: cfg::SystemCaptureSelector::StableId {
                id: "stable-monitor".into(),
            },
            ..Default::default()
        };
        let trusted = BTreeSet::from([peer]);
        let encoded = encode_config(&config, model::AudioProfile::Ethernet).unwrap();
        assert!(!encoded.peers[0].allow_system_send);
        assert!(encoded.peers[0].allow_system_receive);
        assert!(!encoded.peers[0].allow_mic_control);
        assert_eq!(
            decode_config(
                &encoded,
                &config,
                [1; 32],
                &trusted,
                model::AudioProfile::Ethernet
            )
            .unwrap(),
            config
        );
        assert!(decode_config(
            &encoded,
            &config,
            [2; 32],
            &trusted,
            model::AudioProfile::Ethernet
        )
        .is_err());
        assert!(decode_config(
            &encoded,
            &config,
            [1; 32],
            &BTreeSet::new(),
            model::AudioProfile::Ethernet
        )
        .is_err());
        let default = cfg::AudioConfig::default();
        let off = encode_config(&default, model::AudioProfile::Ethernet).unwrap();
        assert!(off.peers.is_empty());
        assert_eq!(off.group_id_override, Some(false));
        assert_eq!(
            decode_config(
                &off,
                &default,
                [1; 32],
                &BTreeSet::new(),
                model::AudioProfile::Ethernet
            )
            .unwrap(),
            default
        );
        let mut controls = encoded;
        controls.peers[0].allow_mic_control = true;
        controls.microphone.as_mut().unwrap().control_enabled = true;
        let decoded = decode_config(
            &controls,
            &config,
            [1; 32],
            &trusted,
            model::AudioProfile::Ethernet,
        )
        .unwrap();
        assert!(decoded.microphone.control_enabled);
        assert!(decoded.peers[0].allow_mic_control);
    }
    #[test]
    fn pending_group_intents_preserve_enabled_without_fabricating_group() {
        let previous = cfg::AudioConfig {
            role: cfg::AudioRole::Source,
            listener_node_id: Some(cfg::HexId([2; 32])),
            ..Default::default()
        };
        let mut request = encode_config(&previous, model::AudioProfile::Ethernet).unwrap();
        request.enabled = true;
        let saved = decode_config(
            &request,
            &previous,
            [1; 32],
            &BTreeSet::from([cfg::HexId([2; 32])]),
            model::AudioProfile::Ethernet,
        )
        .unwrap();
        assert!(saved.enabled);
        assert!(saved.group_id.is_none());
        assert_eq!(saved.listener_node_id, previous.listener_node_id);
        let listener = cfg::AudioConfig {
            enabled: true,
            role: cfg::AudioRole::Listener,
            ..Default::default()
        };
        let pending = encode_config(&listener, model::AudioProfile::Ethernet).unwrap();
        assert!(pending.enabled);
        let waiting = model::AudioStatus {
            enabled: true,
            role: model::AudioRole::Listener,
            ..Default::default()
        };
        let status = encode_status(
            &waiting,
            pending,
            wire::AudioMicrophoneState::default(),
            &[],
        )
        .unwrap();
        assert!(status.enabled && status.sessions.is_empty());
    }
    #[test]
    fn source_group_policy_distinguishes_modern_clones_from_legacy_pins() {
        let group = cfg::HexId([3; 16]);
        let source = cfg::AudioConfig {
            enabled: true,
            role: cfg::AudioRole::Source,
            listener_node_id: Some(cfg::HexId([2; 32])),
            group_id: Some(group),
            ..Default::default()
        };
        let decode = |request: &wire::AudioConfig, previous: &cfg::AudioConfig| {
            decode_config(
                request,
                previous,
                [1; 32],
                &BTreeSet::from([cfg::HexId([2; 32]), cfg::HexId([4; 32])]),
                model::AudioProfile::Ethernet,
            )
            .unwrap()
        };
        for policy in [None, Some(false), Some(true)] {
            let mut original = source.clone();
            original.group_id_override = policy;
            let encoded = encode_config(&original, model::AudioProfile::Ethernet).unwrap();
            assert_eq!(encoded.group_id_override, policy);
            let restored = decode(&encoded, &original);
            assert_eq!(restored, original);
            assert_eq!(restored.group_id_override, policy);
            assert_eq!(restored.effective_group_override(), policy != Some(false));
        }
        let mut request = encode_config(&source, model::AudioProfile::Ethernet).unwrap();
        assert_eq!(request.group_id_override, Some(false));
        request.background_gain_db = -12;
        request.microphone.as_mut().unwrap().control_enabled = true;
        let automatic = decode(&request, &source);
        assert_eq!(automatic.group_id, Some(group));
        assert_eq!(automatic.group_id_override, Some(false));
        assert!(!automatic.effective_group_override());

        request.group_id_override = None;
        let legacy_pin = decode(&request, &source);
        assert_eq!(legacy_pin.group_id, Some(group));
        assert_eq!(legacy_pin.group_id_override, None);
        assert!(legacy_pin.effective_group_override());
        request.group_id.clear();
        for previous in [&legacy_pin, &source] {
            let reused = decode(&request, previous);
            assert_eq!(reused.group_id, Some(group));
            assert_eq!(reused.group_id_override, None);
            assert!(reused.effective_group_override());
        }
        request.listener_node_id = vec![4; 32];
        let unresolved_legacy = decode(&request, &legacy_pin);
        assert!(unresolved_legacy.group_id.is_none());
        assert_eq!(unresolved_legacy.group_id_override, None);
        assert!(!unresolved_legacy.effective_group_override());
        request.listener_node_id = vec![2; 32];

        request.group_id = group.0.to_vec();
        request.group_id_override = Some(true);
        let pinned = decode(&request, &source);
        assert_eq!(pinned.group_id, Some(group));
        assert_eq!(pinned.group_id_override, Some(true));
        assert!(pinned.effective_group_override());
        request = encode_config(&pinned, model::AudioProfile::Ethernet).unwrap();
        assert_eq!(request.group_id_override, Some(true));
        request.background_gain_db = -18;
        assert_eq!(decode(&request, &pinned).group_id_override, Some(true));

        request.group_id_override = Some(false);
        let unpinned = decode(&request, &pinned);
        assert_eq!(unpinned.group_id, Some(group));
        assert_eq!(unpinned.group_id_override, Some(false));
        assert!(!unpinned.effective_group_override());
        request.group_id.clear();
        let empty_unpinned = decode(&request, &pinned);
        assert_eq!(empty_unpinned.group_id, Some(group));
        assert_eq!(empty_unpinned.group_id_override, Some(false));
        assert!(!empty_unpinned.effective_group_override());
        assert_eq!(empty_unpinned.listener_node_id, source.listener_node_id);

        request.listener_node_id = vec![4; 32];
        let changed_listener = decode(&request, &pinned);
        assert!(changed_listener.group_id.is_none());
        assert_eq!(changed_listener.group_id_override, Some(false));
        assert!(!changed_listener.effective_group_override());
        request.group_id = vec![5; 16];
        let changed_group = decode(&request, &source);
        assert_eq!(changed_group.group_id, Some(cfg::HexId([5; 16])));
        assert_eq!(changed_group.group_id_override, Some(false));
        assert!(!changed_group.effective_group_override());
    }

    #[test]
    fn listener_group_identity_survives_empty_updates_but_not_role_changes() {
        let group = cfg::HexId([3; 16]);
        let listener = cfg::AudioConfig {
            enabled: true,
            role: cfg::AudioRole::Listener,
            group_id: Some(group),
            ..Default::default()
        };
        let decode = |request: &wire::AudioConfig, previous: &cfg::AudioConfig| {
            decode_config(
                request,
                previous,
                [1; 32],
                &BTreeSet::from([cfg::HexId([2; 32])]),
                model::AudioProfile::Ethernet,
            )
            .unwrap()
        };
        for policy in [None, Some(false)] {
            let mut original = listener.clone();
            original.group_id_override = policy;
            let mut encoded = encode_config(&original, model::AudioProfile::Ethernet).unwrap();
            assert_eq!(encoded.group_id_override, policy);
            assert_eq!(decode(&encoded, &original), original);
            encoded.group_id.clear();
            let reused = decode(&encoded, &original);
            assert_eq!(reused.group_id, Some(group));
            assert_eq!(reused.group_id_override, policy);
            assert!(!reused.effective_group_override());
        }
        let mut request = encode_config(&listener, model::AudioProfile::Ethernet).unwrap();
        assert_eq!(request.group_id_override, Some(false));
        request.group_id.clear();
        let republished = decode(&request, &listener);
        assert_eq!(republished.group_id, Some(group));
        assert_eq!(republished.group_id_override, Some(false));
        let pending = decode(&request, &cfg::AudioConfig::default());
        assert!(pending.group_id.is_none());
        assert_eq!(pending.group_id_override, Some(false));

        let source = cfg::AudioConfig {
            role: cfg::AudioRole::Source,
            listener_node_id: Some(cfg::HexId([2; 32])),
            group_id: Some(group),
            group_id_override: Some(true),
            ..Default::default()
        };
        let changed_to_listener = decode(&request, &source);
        assert!(changed_to_listener.group_id.is_none());
        assert_eq!(changed_to_listener.group_id_override, Some(false));
        request.role = wire::AudioRole::Source as i32;
        request.listener_node_id = vec![2; 32];
        let changed_to_source = decode(&request, &listener);
        assert!(changed_to_source.group_id.is_none());
        assert_eq!(changed_to_source.group_id_override, Some(false));
    }

    #[test]
    fn group_mismatch_stop_reason_roundtrips() {
        assert_eq!(encode_stop_reason(session_model::Reason::GroupMismatch), 14);
        assert_eq!(
            decode_stop_reason(14).unwrap(),
            session_model::Reason::GroupMismatch
        );
    }
    #[test]
    fn authoritative_status_preserves_metadata_counters_and_optional_worker_facts() {
        let config = cfg::AudioConfig {
            enabled: true,
            role: cfg::AudioRole::Listener,
            group_id: Some(cfg::HexId([3; 16])),
            ..Default::default()
        };
        let snapshot = model::AudioStatus {
            revision: u64::MAX,
            enabled: true,
            system_sharing_requested: None,
            system_sharing_error: None,
            role: model::AudioRole::Listener,
            mode: model::AudioMode::MixAll,
            backend: vec![model::CapabilityAvailability {
                capability: model::BackendCapability::Render,
                availability: model::Availability::Available,
            }],
            sessions: vec![model::SessionStatus {
                session_id: [1; 16],
                peer: [2; 32],
                source_name: "SOURCE".into(),
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
                latency_stages: Some(model::LatencyDiagnostics::default()),
                path_diagnostic: model::PathDiagnostic::None,
                local_udp_port: 47903,
                peer_udp_port: 47904,
                remote_quality: None,
                gain_percent: 37,
                muted: true,
                consent: model::SessionConsent {
                    local: model::ConsentFlags {
                        allow_system_receive: true,
                        ..Default::default()
                    },
                    remote: model::ConsentFlags {
                        allow_system_send: true,
                        ..Default::default()
                    },
                },
                media_info: Some(model::MediaInfo {
                    app_id: "private-app".into(),
                    title: "private-title".into(),
                    subtitle: "private-subtitle".into(),
                    position_ms: Some(0),
                    duration_ms: None,
                    session_token: 0,
                    playback: model::PlaybackState::Paused,
                    commands: model::MediaCommands {
                        play: true,
                        ..Default::default()
                    },
                    revision: u64::MAX,
                }),
                focus_primary: true,
            }],
        };
        let wire_config = encode_config(&config, model::AudioProfile::Ethernet).unwrap();
        let mic = wire::AudioMicrophoneState {
            muted: true,
            ..Default::default()
        };
        let observed = SessionObservation {
            session: [1; 16],
            peer: [2; 32],
            capture_active: Some(true),
            sending: Some(false),
        };
        let result =
            encode_status(&snapshot, wire_config.clone(), mic.clone(), &[observed]).unwrap();
        assert_eq!(result.revision, u64::MAX);
        assert_eq!(result.config, Some(wire_config.clone()));
        assert_eq!(result.microphone, Some(mic.clone()));
        let s = &result.sessions[0];
        assert_eq!(s.capture_active, Some(true));
        assert_eq!(s.sending, Some(false));
        assert_eq!(s.health.as_ref().unwrap().received_packets, u64::MAX);
        assert_eq!(s.health.as_ref().unwrap().limiter_frames, 7);
        assert_eq!(s.latency_estimate_ms, Some(0));
        assert_eq!(s.path_diagnostic, wire::AudioPathDiagnostic::None as i32);
        assert_eq!((s.local_udp_port, s.peer_udp_port), (47903, 47904));
        assert_eq!(s.gain_percent, 37);
        assert!(s.muted && s.focus_primary);
        let media = s.media_info.as_ref().unwrap();
        assert_eq!(media.position_ms, Some(0));
        assert_eq!(media.duration_ms, None);
        assert_eq!(media.title, "private-title");
        assert_eq!(media.session_token, 0);
        assert!(!format!("{result:?}").contains("private-title"));
        let absent = encode_status(&snapshot, wire_config.clone(), mic.clone(), &[]).unwrap();
        assert_eq!(absent.sessions[0].capture_active, None);
        assert_eq!(absent.sessions[0].sending, None);
        let stale = SessionObservation {
            session: [9; 16],
            ..observed
        };
        assert_eq!(
            encode_status(&snapshot, wire_config.clone(), mic.clone(), &[stale]),
            Err(Error::InvalidObservation)
        );
        let mut private = snapshot;
        private.enabled = false;
        let mut disabled = wire_config;
        disabled.enabled = false;
        assert!(encode_status(&private, disabled, mic, &[]).is_err());
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn selecting_a_listener_from_off_does_not_lose_source_role() {
        let previous = cfg::AudioConfig::default();
        let mut desired = encode_config(&previous, model::AudioProfile::Ethernet).unwrap();
        desired.listener_node_id = vec![2; 32];
        let config = decode_config(
            &desired,
            &previous,
            [1; 32],
            &BTreeSet::from([cfg::HexId([2; 32])]),
            model::AudioProfile::Ethernet,
        )
        .unwrap();
        assert_eq!(config.role, cfg::AudioRole::Source);
        assert!(!config.enabled);
    }
    #[test]
    fn microphone_streaming_is_typed_unsupported_and_wrong_direction_is_not_collapsed() {
        let previous = cfg::AudioConfig::default();
        let mut desired = encode_config(&previous, model::AudioProfile::Ethernet).unwrap();
        desired.microphone.as_mut().unwrap().streaming_enabled = true;
        assert_eq!(
            decode_config(
                &desired,
                &previous,
                [1; 32],
                &BTreeSet::new(),
                model::AudioProfile::Ethernet
            ),
            Err(Error::Unsupported)
        );
        let consent = net::AudioControl {
            control_version: 1,
            body: Some(net::audio_control::Body::Consent(
                net::AudioConsentChanged {
                    group_id: vec![3; 16],
                    consent_revision: 8,
                    allowed_directions: protocol::limits::AUDIO_DIRECTION_SEND
                        | protocol::limits::AUDIO_DIRECTION_RECEIVE,
                },
            )),
        };
        assert_eq!(
            decode_control(&consent, model::AudioRole::Source, [4; 16]),
            Err(Error::Unsupported)
        );
    }
}

#[cfg(test)]
mod media_consent_tests {
    use super::*;
    #[test]
    fn explicit_media_permission_roundtrips_without_granting_system_or_mic_consent() {
        let peer = cfg::HexId([2; 32]);
        let config = cfg::AudioConfig {
            role: cfg::AudioRole::Source,
            listener_node_id: Some(peer),
            peers: vec![cfg::AudioPeerConfig {
                node_id: peer,
                system_send_override: None,
                system_receive_override: None,
                allow_media_control: true,
                allow_mic_control: false,
                gain_percent: 100,
                muted: false,
            }],
            ..Default::default()
        };
        let encoded = encode_config(&config, model::AudioProfile::Ethernet).unwrap();
        assert!(encoded.peers[0].allow_media_control);
        assert!(!encoded.peers[0].allow_system_send);
        assert!(!encoded.peers[0].allow_mic_control);
        assert_eq!(
            decode_config(
                &encoded,
                &config,
                [1; 32],
                &BTreeSet::from([peer]),
                model::AudioProfile::Ethernet
            )
            .unwrap(),
            config
        );
    }
}

#[cfg(test)]
mod source_request_tests {
    use super::*;
    #[test]
    fn source_request_and_refusal_preserve_exact_correlation_and_profile() {
        for control in [
            session_model::Control::StartRequest(request(Profile::Ethernet)),
            session_model::Control::StartRequest(request(Profile::Wifi)),
            session_model::Control::StartRefusal {
                request_id: [11; 16],
                reason: session_model::Reason::ConsentRequired,
            },
        ] {
            let encoded =
                encode_control(&control, model::AudioRole::Source, 47903, [3; 16]).unwrap();
            let decoded = decode_control(&encoded, model::AudioRole::Source, [3; 16]).unwrap();
            assert!(
                matches!(&decoded, DecodedControl::Control { message, extras: ControlExtras::None } if message == &control)
            );
            assert_eq!(
                encode_decoded_control(&decoded, model::AudioRole::Source, [3; 16]).unwrap(),
                encoded
            );
        }
    }
    fn request(profile: Profile) -> session_model::StartRequest {
        session_model::StartRequest {
            request_id: [11; 16],
            group: [12; 16],
            source: [13; 32],
            consent_revision: 14,
            kind: keys::StreamKind::System,
            profile,
        }
    }
    #[test]
    fn source_request_boundary_rejects_invalid_identifiers_enums_and_refusal_reason() {
        use net::audio_control::Body;
        use protocol::validate::AudioViolation;
        let valid = encode_control(
            &session_model::Control::StartRequest(request(Profile::Wifi)),
            model::AudioRole::Source,
            47903,
            [3; 16],
        )
        .unwrap();
        for field in 0..5 {
            let mut invalid = valid.clone();
            let Some(Body::StartRequest(request)) = invalid.body.as_mut() else {
                panic!("request");
            };
            let expected = match field {
                0 => {
                    request.request_id.pop();
                    AudioViolation::Identifier
                }
                1 => {
                    request.group_id.pop();
                    AudioViolation::Identifier
                }
                2 => {
                    request.source_node_id.pop();
                    AudioViolation::Identifier
                }
                3 => {
                    request.kind = 99;
                    AudioViolation::UnsupportedFormat
                }
                _ => {
                    request.profile = 99;
                    AudioViolation::UnsupportedFormat
                }
            };
            assert_eq!(
                decode_control(&invalid, model::AudioRole::Source, [3; 16]),
                Err(Error::Protocol(expected))
            );
        }
        let unsupported = session_model::StartRequest {
            kind: keys::StreamKind::Microphone,
            ..request(Profile::Wifi)
        };
        assert_eq!(
            encode_control(
                &session_model::Control::StartRequest(unsupported),
                model::AudioRole::Source,
                47903,
                [3; 16]
            ),
            Err(Error::Unsupported)
        );
        for (request_id, reason, expected) in [
            (
                vec![11; 15],
                net::AudioStopCode::ConsentRequired as i32,
                AudioViolation::Identifier,
            ),
            (vec![11; 16], 0, AudioViolation::Reason),
            (vec![11; 16], 99, AudioViolation::Reason),
        ] {
            let invalid = net::AudioControl {
                control_version: 1,
                body: Some(Body::StartRefusal(net::AudioStartRefusal {
                    request_id,
                    reason,
                })),
            };
            assert_eq!(
                decode_control(&invalid, model::AudioRole::Listener, [3; 16]),
                Err(Error::Protocol(expected))
            );
        }
    }
}

#[test]
fn full_config_preserves_explicit_denied_peer_preferences() {
    let local = [1; 32];
    let peer = cfg::HexId([2; 32]);
    let preferences = cfg::AudioConfig {
        role: cfg::AudioRole::Listener,
        peers: vec![cfg::AudioPeerConfig {
            node_id: peer,
            system_send_override: None,
            system_receive_override: None,
            allow_media_control: false,
            allow_mic_control: false,
            gain_percent: 100,
            muted: false,
        }],
        ..Default::default()
    };
    let encoded = encode_config(&preferences, model::AudioProfile::Ethernet).unwrap();
    let decoded = decode_config(
        &encoded,
        &cfg::AudioConfig::default(),
        local,
        &BTreeSet::from([peer]),
        model::AudioProfile::Ethernet,
    )
    .unwrap();
    assert_eq!(decoded.peers, preferences.peers);
    assert!(!decoded.enabled);
    assert!(decoded.allow_system_audio_from.is_empty());
    assert!(decoded.allow_send_to.is_empty());
}

#[cfg(test)]
mod focus_tests {
    use super::*;
    use audio_model::focus::{Owner, Snapshot};

    fn snapshot() -> Snapshot {
        Snapshot {
            owner: Owner {
                node: [1; 32],
                serial: 2,
            },
            revision: 3,
            active: Some([2; 32]),
            lease: Some([4; 16]),
            committed: true,
        }
    }

    #[test]
    fn focus_roundtrip_is_separate_from_session_admission() {
        let value = snapshot();
        let message = encode_focus(value);
        assert!(is_focus(&message));
        assert_eq!(decode_focus(&message), Ok(FocusControl::Snapshot(value)));
        assert_eq!(
            decode_control(&message, model::AudioRole::Source, [9; 16]),
            Err(Error::Unsupported)
        );
        let request = encode_focus_request([5; 16]);
        assert_eq!(decode_focus(&request), Ok(FocusControl::Request([5; 16])));
    }

    #[test]
    fn focus_rejects_malformed_commit_and_identifiers() {
        let mut value = snapshot();
        value.lease = None;
        assert!(decode_focus(&encode_focus(value)).is_err());
        value = snapshot();
        value.committed = false;
        assert!(decode_focus(&encode_focus(value)).is_err());
        value = snapshot();
        value.owner.serial = 0;
        assert!(decode_focus(&encode_focus(value)).is_err());
        value = snapshot();
        value.revision = 0;
        assert!(decode_focus(&encode_focus(value)).is_err());
        let mut request = encode_focus_request([5; 16]);
        if let Some(net::audio_control::Body::FocusRequest(value)) = request.body.as_mut() {
            value.group_id.pop();
        }
        assert!(decode_focus(&request).is_err());
    }

    #[test]
    fn idle_owner_and_unknown_roundtrip() {
        let mut value = snapshot();
        value.active = Some(value.owner.node);
        value.lease = None;
        assert_eq!(
            decode_focus(&encode_focus(value)),
            Ok(FocusControl::Snapshot(value))
        );
        value.active = None;
        value.committed = false;
        assert_eq!(
            decode_focus(&encode_focus(value)),
            Ok(FocusControl::Snapshot(value))
        );
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;

    fn status() -> wire::AudioStatus {
        wire::AudioStatus {
            revision: 10,
            enabled: true,
            role: wire::AudioRole::Source as i32,
            mode: wire::AudioMode::MixAll as i32,
            config: Some(wire::AudioConfig {
                enabled: true,
                role: wire::AudioRole::Source as i32,
                group_id: vec![3; 16],
                listener_node_id: vec![2; 32],
                output_device: "default".into(),
                group_id_override: Some(false),
                ..Default::default()
            }),
            sessions: vec![wire::AudioSessionStatus {
                session_id: vec![1; 16],
                peer: vec![2; 32],
                local_role: wire::AudioRole::Source as i32,
                state: wire::AudioSessionState::Running as i32,
                profile: wire::AudioProfile::Ethernet as i32,
                kind: wire::AudioKind::System as i32,
                consent: Some(wire::AudioSessionConsent {
                    local: Some(wire::AudioConsentFlags {
                        allow_system_send: true,
                        ..Default::default()
                    }),
                    remote: Some(wire::AudioConsentFlags {
                        allow_system_receive: true,
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn telemetry_and_playout_observations_do_not_change_the_control_domain() {
        let old = status();
        let mut new = old.clone();
        new.revision += 1;
        let session = &mut new.sessions[0];
        session.health = Some(wire::AudioHealth {
            received_packets: 100,
            ..Default::default()
        });
        session.remote_quality = Some(wire::AudioRemoteQuality {
            received_packets: 100,
            ..Default::default()
        });
        session.latency_estimate_ms = Some(20);
        session.state = wire::AudioSessionState::Rebuffering as i32;
        session.focus_primary = true;
        session.gain_percent = 25;
        session.media_info = Some(wire::MediaInfo {
            session_token: 1,
            revision: 1,
            ..Default::default()
        });
        assert!(same_control_state(&old, &new));
        assert!(control_revision_matches(
            old.revision,
            new.revision,
            old.revision
        ));
    }

    #[test]
    fn preferences_consent_identity_and_lifecycle_close_the_control_interval() {
        let old = status();
        for change in 0..6 {
            let mut new = old.clone();
            match change {
                0 => new.config.as_mut().unwrap().group_id[0] ^= 1,
                1 => new.config.as_mut().unwrap().output_device = "another-device".into(),
                2 => {
                    new.sessions[0]
                        .consent
                        .as_mut()
                        .unwrap()
                        .local
                        .as_mut()
                        .unwrap()
                        .allow_system_send = false
                }
                3 => new.sessions[0].session_id[0] ^= 1,
                4 => new.sessions[0].state = wire::AudioSessionState::Failed as i32,
                5 => new.sessions[0].muted = true,
                _ => unreachable!(),
            }
            assert!(!same_control_state(&old, &new));
        }
    }

    #[test]
    fn future_stale_and_aba_snapshots_cannot_cross_the_control_barrier() {
        for expected in [0, 9, 21, u64::MAX] {
            assert!(!control_revision_matches(expected, 20, 10));
        }
        for expected in [10, 15, 20] {
            assert!(control_revision_matches(expected, 20, 10));
        }
        assert!(!control_revision_matches(u64::MAX, u64::MAX, u64::MAX));
        let a = status();
        let mut b = a.clone();
        b.config.as_mut().unwrap().group_id[0] ^= 1;
        assert!(!same_control_state(&a, &b));
        let returned_to_a = a.clone();
        assert!(!same_control_state(&b, &returned_to_a));
        assert!(same_control_state(&a, &returned_to_a));
        assert!(!control_revision_matches(a.revision, 12, 12));
    }
}
