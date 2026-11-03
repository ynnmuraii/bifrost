//! Audio failures are deliberately separate from fatal input framing violations.
use crate::{limits, wire};
use prost::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioViolation {
    TooLarge,
    Version,
    MissingBody,
    Identifier,
    Nonce,
    Role,
    Directions,
    MediaVersions,
    Port,
    UnsupportedFormat,
    Reason,
    Epoch,
    Ttl,
    Health,
    Lease,
    ParentConnection,
    Capability,
    QueueFull,
    Shape,
    Metadata,
    Artwork,
}

impl AudioViolation {
    pub const fn stop_code(self) -> wire::AudioStopCode {
        match self {
            Self::UnsupportedFormat | Self::MediaVersions | Self::Version => {
                wire::AudioStopCode::UnsupportedFormat
            }
            Self::Capability => wire::AudioStopCode::Disabled,
            Self::QueueFull => wire::AudioStopCode::RateLimited,
            _ => wire::AudioStopCode::ProtocolViolation,
        }
    }
}

/// Check received AudioControl body bytes, including unknown fields and duplicate
/// Envelope audio fields. Protobuf errors remain fatal to the input connection.
pub fn audio_control_wire_size(
    mut envelope: &[u8],
) -> Result<Option<AudioViolation>, prost::DecodeError> {
    use prost::encoding::{decode_key, decode_varint, skip_field, DecodeContext, WireType};
    let mut total = 0usize;
    while !envelope.is_empty() {
        let (tag, wire_type) = decode_key(&mut envelope)?;
        if tag == 59 && wire_type == WireType::LengthDelimited {
            let mut field = envelope;
            let len = decode_varint(&mut envelope)?;
            if len > envelope.len() as u64 {
                // Let prost construct its own public decoding error for the
                // truncated length-delimited field.
                return skip_field(wire_type, tag, &mut field, DecodeContext::default())
                    .map(|()| None);
            }
            // Each body is a disjoint subslice of this bounded Envelope.
            total += len as usize;
            envelope = &envelope[len as usize..];
        } else {
            skip_field(wire_type, tag, &mut envelope, DecodeContext::default())?;
        }
    }
    Ok((total > limits::MAX_AUDIO_CONTROL_BYTES).then_some(AudioViolation::TooLarge))
}

/// Validate the fixed V1 system format, not a negotiable range of codec knobs.
pub fn audio_format(format: &wire::AudioFormat) -> Result<(), AudioViolation> {
    let profile = match wire::AudioLatencyProfile::try_from(format.profile) {
        Ok(wire::AudioLatencyProfile::Ethernet) => {
            format.frame_samples == 480 && format.bitrate_bps == 96_000 && !format.fec
        }
        Ok(wire::AudioLatencyProfile::Wifi) => {
            format.frame_samples == 960 && format.bitrate_bps == 64_000 && format.fec
        }
        _ => false,
    };
    if format.media_version != 1
        || format.codec != wire::AudioCodec::Opus as i32
        || format.sample_rate != 48_000
        || format.channels != 2
        || format.max_datagram_bytes != 1_200
        || format.aead != wire::AudioAead::Chacha20Poly1305 as i32
        || !profile
    {
        return Err(AudioViolation::UnsupportedFormat);
    }
    Ok(())
}

fn id(bytes: &[u8], len: usize) -> Result<(), AudioViolation> {
    if bytes.len() != len {
        return Err(AudioViolation::Identifier);
    }
    Ok(())
}
fn nonce(bytes: &[u8]) -> Result<(), AudioViolation> {
    if bytes.len() != 32 {
        return Err(AudioViolation::Nonce);
    }
    Ok(())
}
fn port(value: u32) -> Result<(), AudioViolation> {
    if !(1..=65_535).contains(&value) {
        return Err(AudioViolation::Port);
    }
    Ok(())
}
fn directions(value: u32) -> Result<(), AudioViolation> {
    if value & !(limits::AUDIO_DIRECTION_SEND | limits::AUDIO_DIRECTION_RECEIVE) != 0 {
        return Err(AudioViolation::Directions);
    }
    Ok(())
}
fn reason(value: i32) -> Result<(), AudioViolation> {
    match wire::AudioStopCode::try_from(value) {
        Ok(wire::AudioStopCode::Unspecified) | Err(_) => Err(AudioViolation::Reason),
        Ok(_) => Ok(()),
    }
}
fn epoch(value: u32) -> Result<(), AudioViolation> {
    if value != 1 {
        return Err(AudioViolation::Epoch);
    }
    Ok(())
}

fn result(value: i32) -> Result<(), AudioViolation> {
    wire::AudioControlResult::try_from(value)
        .map(|_| ())
        .map_err(|_| AudioViolation::Reason)
}

/// Bound the complete message before any membership/duplicate scans. Envelope and
/// sequence validation remain the caller's fatal KVM boundary; this error is audio-only.
pub fn audio_control(message: &wire::AudioControl) -> Result<(), AudioViolation> {
    use wire::audio_control::Body;
    if message.encoded_len() > limits::MAX_AUDIO_CONTROL_BYTES {
        return Err(AudioViolation::TooLarge);
    }
    if message.control_version != limits::AUDIO_CONTROL_VERSION {
        return Err(AudioViolation::Version);
    }
    match message.body.as_ref().ok_or(AudioViolation::MissingBody)? {
        Body::Hello(hello) => {
            id(&hello.listener_node_id, 32)?;
            match wire::AudioNodeRole::try_from(hello.role) {
                Ok(wire::AudioNodeRole::Listener) => id(&hello.group_id, 16)?,
                Ok(wire::AudioNodeRole::Source) => {
                    if !hello.group_id.is_empty() {
                        id(&hello.group_id, 16)?;
                    }
                }
                _ => return Err(AudioViolation::Role),
            }
            port(hello.udp_port)?;
            directions(hello.directions)?;
            if hello.media_versions.is_empty()
                || hello.media_versions.len() > limits::MAX_AUDIO_MEDIA_VERSIONS
            {
                return Err(AudioViolation::MediaVersions);
            }
            for (index, version) in hello.media_versions.iter().enumerate() {
                if *version == 0 || hello.media_versions[..index].contains(version) {
                    return Err(AudioViolation::MediaVersions);
                }
            }
            if !hello.media_versions.contains(&1) {
                return Err(AudioViolation::MediaVersions);
            }
        }
        Body::Offer(offer) => {
            id(&offer.session_id, 16)?;
            if !offer.request_id.is_empty() {
                id(&offer.request_id, 16)?;
            }
            nonce(&offer.session_nonce)?;
            id(&offer.parent_connection_id, 16)?;
            id(&offer.group_id, 16)?;
            id(&offer.listener_node_id, 32)?;
            id(&offer.source_node_id, 32)?;
            if offer.listener_node_id == offer.source_node_id {
                return Err(AudioViolation::Role);
            }
            if offer.kind != wire::AudioStreamKind::System as i32 {
                return Err(AudioViolation::UnsupportedFormat);
            }
            audio_format(
                offer
                    .format
                    .as_ref()
                    .ok_or(AudioViolation::UnsupportedFormat)?,
            )?;
            port(offer.udp_port)?;
            epoch(offer.key_epoch)?;
            if offer.control_ttl_ms != limits::AUDIO_CONTROL_TTL_MS {
                return Err(AudioViolation::Ttl);
            }
        }
        Body::Answer(answer) => {
            id(&answer.session_id, 16)?;
            if answer.accepted {
                if answer.reason != wire::AudioStopCode::Unspecified as i32 {
                    return Err(AudioViolation::Reason);
                }
                audio_format(
                    answer
                        .accepted_format
                        .as_ref()
                        .ok_or(AudioViolation::UnsupportedFormat)?,
                )?;
                port(answer.udp_port)?;
                nonce(&answer.source_nonce)?;
            } else {
                reason(answer.reason)?;
                if answer.accepted_format.is_some()
                    || !answer.source_nonce.is_empty()
                    || answer.udp_port != 0
                {
                    return Err(AudioViolation::Reason);
                }
            }
        }
        Body::Commit(commit) => {
            id(&commit.session_id, 16)?;
            nonce(&commit.transcript_sha256)?;
            epoch(commit.key_epoch)?;
        }
        Body::Stop(stop) => {
            id(&stop.session_id, 16)?;
            reason(stop.reason)?;
        }
        Body::Keepalive(keepalive) => {
            id(&keepalive.session_id, 16)?;
            if !(1..=limits::AUDIO_CONTROL_TTL_MS).contains(&keepalive.authorized_for_ms) {
                return Err(AudioViolation::Ttl);
            }
        }
        Body::Report(report) => {
            id(&report.session_id, 16)?;
            if report.buffer_ms > 1_000
                || report.suggested_loss_pct > 100
                || !matches!(
                    wire::AudioDeviceState::try_from(report.device_state),
                    Ok(wire::AudioDeviceState::Ready
                        | wire::AudioDeviceState::Unavailable
                        | wire::AudioDeviceState::Rebuffering)
                )
            {
                return Err(AudioViolation::Health);
            }
        }
        Body::Focus(focus) => {
            id(&focus.owner_node_id, 32)?;
            if focus.owner_serial == 0 || focus.focus_revision == 0 {
                return Err(AudioViolation::Epoch);
            }
            if focus.committed {
                id(&focus.active_node_id, 32)?;
                if focus.active_node_id == focus.owner_node_id {
                    if !focus.lease_id.is_empty() {
                        return Err(AudioViolation::Lease);
                    }
                } else {
                    id(&focus.lease_id, 16)?;
                }
            } else if !focus.active_node_id.is_empty() || !focus.lease_id.is_empty() {
                return Err(AudioViolation::Lease);
            }
        }
        Body::FocusRequest(request) => id(&request.group_id, 16)?,
        Body::Rekey(rekey) => {
            id(&rekey.session_id, 16)?;
            if rekey.epoch < 2 || !(1..=4).contains(&rekey.stage) {
                return Err(AudioViolation::Epoch);
            }
            if rekey.stage == 1 {
                id(&rekey.nonce, 32)?;
            } else if !rekey.nonce.is_empty() {
                return Err(AudioViolation::Shape);
            }
        }
        Body::MicControl(control) => {
            id(&control.group_id, 16)?;
            id(&control.request_id, 16)?;
            if control.expected_revision == 0 {
                return Err(AudioViolation::Epoch);
            }
            match wire::MicControlOp::try_from(control.op) {
                Ok(wire::MicControlOp::SetMute) => {
                    if !control.press_id.is_empty()
                        || control.challenge != 0
                        || control.expires_ns != 0
                    {
                        return Err(AudioViolation::Shape);
                    }
                }
                Ok(wire::MicControlOp::Press | wire::MicControlOp::Renew) => {
                    id(&control.press_id, 16)?;
                    if control.muted || control.challenge == 0 || control.expires_ns == 0 {
                        return Err(AudioViolation::Shape);
                    }
                }
                Ok(wire::MicControlOp::Release) => {
                    id(&control.press_id, 16)?;
                    if control.muted || control.challenge != 0 || control.expires_ns != 0 {
                        return Err(AudioViolation::Shape);
                    }
                }
                Ok(wire::MicControlOp::ReleaseAll) => {
                    if control.muted
                        || !control.press_id.is_empty()
                        || control.challenge != 0
                        || control.expires_ns != 0
                    {
                        return Err(AudioViolation::Shape);
                    }
                }
                _ => return Err(AudioViolation::Shape),
            }
        }
        Body::MicState(state) => {
            id(&state.group_id, 16)?;
            if state.revision == 0 {
                return Err(AudioViolation::Epoch);
            }
            result(state.reason)?;
            if !state.controller_node_id.is_empty() {
                id(&state.controller_node_id, 32)?;
            }
            if !state.press_id.is_empty() {
                id(&state.press_id, 16)?;
            }
            if (state.challenge == 0) != (state.expires_ns == 0)
                || (state.ptt_active
                    && (!state.ptt_enabled || state.muted || state.press_id.is_empty()))
                || (!state.ptt_active && !state.press_id.is_empty())
            {
                return Err(AudioViolation::Shape);
            }
        }
        Body::MicAck(ack) => {
            id(&ack.group_id, 16)?;
            id(&ack.request_id, 16)?;
            if ack.revision == 0 {
                return Err(AudioViolation::Epoch);
            }
            result(ack.result)?;
        }
        Body::ArtworkRequest(request) => {
            let valid = (|| {
                id(&request.session_id, 16)?;
                id(&request.artwork_id, 32)?;
                if request.token == 0 {
                    return Err(AudioViolation::Epoch);
                }
                Ok(())
            })();
            valid.map_err(|_| AudioViolation::Artwork)?;
        }
        Body::Artwork(artwork) => {
            let valid = (|| {
                id(&artwork.session_id, 16)?;
                id(&artwork.artwork_id, 32)?;
                if artwork.token == 0
                    || artwork.revision == 0
                    || artwork.total_bytes == 0
                    || artwork.total_bytes as usize > limits::MAX_AUDIO_ARTWORK_BYTES
                    || artwork.data.is_empty()
                    || artwork.data.len() > limits::MAX_AUDIO_ARTWORK_CHUNK
                    || (artwork.offset as usize)
                        .checked_add(artwork.data.len())
                        .is_none_or(|end| end > artwork.total_bytes as usize)
                    || !(1..=192).contains(&artwork.width)
                    || !(1..=192).contains(&artwork.height)
                {
                    return Err(AudioViolation::Metadata);
                }
                Ok(())
            })();
            valid.map_err(|_| AudioViolation::Artwork)?;
        }
        Body::MediaInfo(info) => {
            id(&info.session_id, 16)?;
            if !info.artwork_id.is_empty() {
                id(&info.artwork_id, 32).map_err(|_| AudioViolation::Artwork)?;
            }
            if info.app.len() > 128
                || info.title.len() > 256
                || info.subtitle.len() > 256
                || info
                    .app
                    .chars()
                    .chain(info.title.chars())
                    .chain(info.subtitle.chars())
                    .any(char::is_control)
                || wire::MediaPlayback::try_from(info.playback).is_err()
                || info.commands & !31 != 0
                || matches!((info.position_ms, info.duration_ms), (Some(p), Some(d)) if p > d)
                || if info.can_seek == Some(true) {
                    !matches!(
                        (info.position_ms, info.duration_ms, info.seek_min_ms, info.seek_max_ms),
                        (Some(_), Some(duration), Some(min), Some(max))
                            if duration > 0 && min <= max && max <= duration
                    )
                } else {
                    info.seek_min_ms.is_some() || info.seek_max_ms.is_some()
                }
            {
                return Err(AudioViolation::Metadata);
            }
            if info.present {
                if info.token == 0 || info.revision == 0 {
                    return Err(AudioViolation::Epoch);
                }
            } else if !info.artwork_id.is_empty()
                || info.token != 0
                || !info.app.is_empty()
                || !info.title.is_empty()
                || !info.subtitle.is_empty()
                || info.position_ms.is_some()
                || info.duration_ms.is_some()
                || info.playback != wire::MediaPlayback::Stopped as i32
                || info.commands != 0
                || info.allow_control
                || info.can_seek == Some(true)
                || info.seek_min_ms.is_some()
                || info.seek_max_ms.is_some()
            {
                return Err(AudioViolation::Shape);
            }
        }
        Body::MediaCommand(command) => {
            id(&command.request_id, 16)?;
            id(&command.session_id, 16)?;
            if command.token == 0 || command.revision == 0 {
                return Err(AudioViolation::Epoch);
            }
            match wire::MediaCommandKind::try_from(command.command) {
                Ok(wire::MediaCommandKind::Unspecified) | Err(_) => {
                    return Err(AudioViolation::Shape)
                }
                Ok(_) => {}
            }
            if (command.command == wire::MediaCommandKind::Seek as i32)
                != command.position_ms.is_some()
            {
                return Err(AudioViolation::Shape);
            }
        }
        Body::MediaAck(ack) => {
            id(&ack.request_id, 16)?;
            id(&ack.session_id, 16)?;
            result(ack.result)?;
        }
        Body::Consent(consent) => {
            id(&consent.group_id, 16)?;
            directions(consent.allowed_directions)?;
        }
        Body::StartRequest(request) => {
            id(&request.request_id, 16)?;
            id(&request.group_id, 16)?;
            id(&request.source_node_id, 32)?;
            if request.kind != wire::AudioStreamKind::System as i32
                || !matches!(
                    wire::AudioLatencyProfile::try_from(request.profile),
                    Ok(wire::AudioLatencyProfile::Ethernet | wire::AudioLatencyProfile::Wifi)
                )
            {
                return Err(AudioViolation::UnsupportedFormat);
            }
        }
        Body::StartRefusal(refusal) => {
            id(&refusal.request_id, 16)?;
            reason(refusal.reason)?;
        }
    }
    Ok(())
}
