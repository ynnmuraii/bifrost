//! Private audio-child IPC, intentionally distinct from GUI `audio` validators.
//! Reuses Framed and same-user transport endpoints. Every receive error is terminal.
use crate::{Framed, IpcError};
use prost::Message;
use protocol::{audio_worker as w, SeqValidator};
use std::{fmt, io::Read, net::IpAddr, path::Path};
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::{Zeroize, Zeroizing};
pub const FRAME_MAJOR: u32 = protocol::limits::PROTOCOL_MAJOR;
pub const FRAME_MINOR: u32 = protocol::limits::PROTOCOL_MINOR;
pub const CORE_SILENCE_MS: u32 = 1000;
pub const PING_INTERVAL_MS: u64 = 250;
pub const MAX_SESSIONS: usize = 3;
pub const DEVICES_PER_PAGE: usize = 16;
pub const MAX_DEVICES: usize = 64;
pub const MAX_COMMAND_BYTES: usize = 8192;
pub const MAX_PTT_RESERVED: usize = 16;
pub const MAX_MEDIA_SESSIONS: usize = 32;
pub const MAX_PTT_GRANT_NS: u64 = 250_000_000;

pub struct LaunchToken(Zeroizing<[u8; 32]>);
impl LaunchToken {
    pub fn new(bytes: Zeroizing<[u8; 32]>) -> Self {
        Self(bytes)
    }
    pub fn expose_secret(&self) -> &[u8; 32] {
        &self.0
    }
}
pub struct DirectionSecrets {
    key: Zeroizing<[u8; 32]>,
    prefix: Zeroizing<[u8; 4]>,
}
impl DirectionSecrets {
    pub fn new(key: Zeroizing<[u8; 32]>, prefix: Zeroizing<[u8; 4]>) -> Self {
        Self { key, prefix }
    }
    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }
    pub fn into_parts(self) -> (Zeroizing<[u8; 32]>, Zeroizing<[u8; 4]>) {
        (self.key, self.prefix)
    }
    fn from_wire(mut material: w::DirectionMaterial) -> Result<Self, IpcError> {
        let mut key = Zeroizing::new([0; 32]);
        let mut prefix = Zeroizing::new([0; 4]);
        if material.key.len() != 32 || material.nonce_prefix.len() != 4 {
            return Err(invalid());
        }
        key.copy_from_slice(&material.key);
        prefix.copy_from_slice(&material.nonce_prefix);
        material.key.zeroize();
        material.nonce_prefix.zeroize();
        Ok(Self { key, prefix })
    }
}
pub struct SessionSecrets {
    pub forward: DirectionSecrets,
    pub reverse: DirectionSecrets,
}
/// The raw message has its key/token fields removed; only opaque non-Debug owners hold secrets.
pub struct Received {
    pub frame: w::AudioFrame,
    pub secrets: Option<SessionSecrets>,
    pub token: Option<LaunchToken>,
}
impl fmt::Debug for Received {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioReceived")
            .field("seq", &self.frame.seq)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

pub struct AudioLink<S> {
    framed: Framed<S>,
    generation: Option<u64>,
    worker: bool,
    inbound: SeqValidator,
    next: u64,
}
impl<S> fmt::Debug for AudioLink<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AudioLink([REDACTED])")
    }
}
impl<S> AudioLink<S> {
    pub fn core(io: S, generation: u64) -> Result<Self, IpcError> {
        if generation == 0 {
            return Err(invalid());
        }
        Ok(Self {
            framed: Framed::sensitive_ipc(io),
            generation: Some(generation),
            worker: false,
            inbound: SeqValidator::new(),
            next: 1,
        })
    }
    pub fn worker(io: S) -> Self {
        Self {
            framed: Framed::sensitive_ipc(io),
            generation: None,
            worker: true,
            inbound: SeqValidator::new(),
            next: 1,
        }
    }
    pub fn generation(&self) -> Option<u64> {
        self.generation
    }
    pub fn inbound_expected(&self) -> u64 {
        self.inbound.expected()
    }
    pub fn outbound_next(&self) -> u64 {
        self.next
    }
}
impl<S> Drop for AudioLink<S> {
    fn drop(&mut self) {
        self.framed.clear_sensitive();
    }
}
impl<S: AsyncRead + AsyncWrite + Unpin> AudioLink<S> {
    pub async fn send(
        &mut self,
        request_id: u64,
        message: w::audio_frame::Msg,
    ) -> Result<(), IpcError> {
        let frame = w::AudioFrame {
            major: FRAME_MAJOR,
            minor: FRAME_MINOR,
            seq: self.next,
            worker_generation: self.generation.ok_or_else(invalid)?,
            request_id,
            msg: Some(message),
        };
        validate_frame(&frame)?;
        let message = frame.msg.as_ref().ok_or_else(invalid)?;
        if is_core(message) == self.worker
            || (self.next == 1
                && !if self.worker {
                    matches!(message, w::audio_frame::Msg::Ready(_))
                } else {
                    matches!(message, w::audio_frame::Msg::Attach(_))
                })
            || (self.next != 1 && matches!(message, w::audio_frame::Msg::Attach(_)))
        {
            return Err(invalid());
        }
        self.framed.send_sensitive(&frame).await?;
        self.next = self.next.checked_add(1).ok_or_else(invalid)?;
        Ok(())
    }
    pub async fn recv(&mut self) -> Result<Received, IpcError> {
        let body = self.framed.recv_sensitive_bytes().await?;
        let mut frame = w::AudioFrame::decode(body.as_slice()).map_err(|_| invalid())?;
        validate_frame(&frame)?;
        let message = frame.msg.as_ref().ok_or_else(invalid)?;
        let first = self.inbound.expected() == 1;
        if is_core(message) != self.worker
            || (first
                && !if self.worker {
                    matches!(message, w::audio_frame::Msg::Attach(_))
                } else {
                    matches!(message, w::audio_frame::Msg::Ready(_))
                })
            || (!first && matches!(message, w::audio_frame::Msg::Attach(_)))
        {
            return Err(invalid());
        }
        if self
            .generation
            .is_some_and(|generation| generation != frame.worker_generation)
        {
            return Err(invalid());
        }
        self.inbound.accept(frame.seq)?;
        self.generation = Some(frame.worker_generation);
        let mut secrets = None;
        let mut token = None;
        match frame.msg.as_mut().ok_or_else(invalid)? {
            w::audio_frame::Msg::Attach(attach) => {
                let mut bytes = Zeroizing::new([0; 32]);
                bytes.copy_from_slice(&attach.launch_token);
                attach.launch_token.zeroize();
                token = Some(LaunchToken::new(bytes));
            }
            w::audio_frame::Msg::StartSession(start) => {
                secrets = Some(SessionSecrets {
                    forward: DirectionSecrets::from_wire(
                        start.forward.take().ok_or_else(invalid)?,
                    )?,
                    reverse: DirectionSecrets::from_wire(
                        start.reverse.take().ok_or_else(invalid)?,
                    )?,
                });
            }
            _ => {}
        }
        Ok(Received {
            frame,
            secrets,
            token,
        })
    }
}
fn invalid() -> IpcError {
    IpcError::protocol("invalid private audio-worker frame")
}
fn text(value: &str, max: usize, empty: bool) -> Result<(), IpcError> {
    if value.len() > max || (!empty && value.is_empty()) || value.chars().any(char::is_control) {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn session_id(id: &[u8]) -> Result<(), IpcError> {
    if id.len() != 16 {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn selection(value: &w::DeviceSelection) -> Result<(), IpcError> {
    match value.selector.as_ref().ok_or_else(invalid)? {
        w::device_selection::Selector::DeviceId(id) => text(id, 512, false),
        w::device_selection::Selector::FollowDefault(1 | 2) => Ok(()),
        _ => Err(invalid()),
    }
}
fn hotkey(value: &w::PttHotkey) -> Result<(), IpcError> {
    if !(0x04..=0x73).contains(&value.key_code) {
        return Err(invalid());
    }
    Ok(())
}
fn port(port: u32, default: bool) -> bool {
    (port == 0 && default) || (port > 0 && port <= 65535 && port != 47902)
}
fn address(ip: &str, scope: u32) -> Result<IpAddr, IpcError> {
    text(ip, 45, false)?;
    let ip: IpAddr = ip.parse().map_err(|_| invalid())?;
    if ip.is_unspecified()
        || ip.is_multicast()
        || ip == IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
        || ip.is_ipv4() && scope != 0
    {
        return Err(invalid());
    }
    if matches!(ip,IpAddr::V6(ip) if ip.is_unicast_link_local()) && scope == 0 {
        return Err(invalid());
    }
    Ok(ip)
}
fn is_core(message: &w::audio_frame::Msg) -> bool {
    matches!(
        message,
        w::audio_frame::Msg::Attach(_)
            | w::audio_frame::Msg::ListDevices(_)
            | w::audio_frame::Msg::StartSession(_)
            | w::audio_frame::Msg::StopSession(_)
            | w::audio_frame::Msg::RenewSession(_)
            | w::audio_frame::Msg::SetGain(_)
            | w::audio_frame::Msg::Ping(_)
            | w::audio_frame::Msg::Shutdown(_)
            | w::audio_frame::Msg::ProbeBackend(_)
            | w::audio_frame::Msg::AcquireMicMute(_)
            | w::audio_frame::Msg::SetMicMute(_)
            | w::audio_frame::Msg::ReleaseMicMute(_)
            | w::audio_frame::Msg::ConfigurePtt(_)
            | w::audio_frame::Msg::PttGrant(_)
            | w::audio_frame::Msg::PttRevoke(_)
            | w::audio_frame::Msg::PttReleaseAll(_)
            | w::audio_frame::Msg::SetMediaWatch(_)
            | w::audio_frame::Msg::MediaCommand(_)
    )
}
/// Semantic validation occurs before sequence state changes or any native/key use.
pub fn validate_frame(frame: &w::AudioFrame) -> Result<(), IpcError> {
    if frame.major != FRAME_MAJOR
        || frame.minor > FRAME_MINOR
        || frame.seq == 0
        || frame.worker_generation == 0
    {
        return Err(invalid());
    }
    let message = frame.msg.as_ref().ok_or_else(invalid)?;
    if is_core(message) && frame.encoded_len() > MAX_COMMAND_BYTES {
        return Err(invalid());
    }
    use w::audio_frame::Msg;
    let correlated = matches!(
        message,
        Msg::Attach(_)
            | Msg::ListDevices(_)
            | Msg::StartSession(_)
            | Msg::StopSession(_)
            | Msg::RenewSession(_)
            | Msg::SetGain(_)
            | Msg::ProbeBackend(_)
            | Msg::Shutdown(_)
            | Msg::Ready(_)
            | Msg::Devices(_)
    );
    let correlated = correlated
        || matches!(
            message,
            Msg::AcquireMicMute(_)
                | Msg::SetMicMute(_)
                | Msg::ReleaseMicMute(_)
                | Msg::ConfigurePtt(_)
                | Msg::PttGrant(_)
                | Msg::PttRevoke(_)
                | Msg::PttReleaseAll(_)
                | Msg::SetMediaWatch(_)
                | Msg::MediaCommand(_)
                | Msg::MediaCommandResult(_)
        );
    let pushed = matches!(
        message,
        Msg::Ping(_)
            | Msg::Pong(_)
            | Msg::SessionEvent(_)
            | Msg::Health(_)
            | Msg::BackendEvent(_)
            | Msg::MediaSessions(_)
            | Msg::PttStatus(_)
    );
    if correlated && frame.request_id == 0 || pushed && frame.request_id != 0 {
        return Err(invalid());
    }
    match message {
        Msg::Attach(attach) => {
            if attach.launch_token.len() != 32
                || !matches!(attach.backend, 1 | 2)
                || attach.core_silence_ms != CORE_SILENCE_MS
            {
                return Err(invalid());
            }
        }
        Msg::Ready(ready) => {
            if !matches!(ready.backend, 2..=4) || ready.capabilities.len() != 6 {
                return Err(invalid());
            }
            let mut seen = 0u8;
            for capability in &ready.capabilities {
                if !(1..=6).contains(&capability.capability)
                    || seen & (1 << capability.capability) != 0
                {
                    return Err(invalid());
                }
                seen |= 1 << capability.capability;
                if !matches!(capability.availability, 1..=3)
                    || capability.availability == 1 && capability.reason != 0
                    || capability.availability != 1 && !(1..=10).contains(&capability.reason)
                {
                    return Err(invalid());
                }
                if !ready.probed && (capability.availability != 3 || capability.reason != 1) {
                    return Err(invalid());
                }
            }
        }
        Msg::Ping(ping) => {
            if ping.serial == 0 {
                return Err(invalid());
            }
        }
        Msg::Pong(pong) => {
            if pong.serial == 0 {
                return Err(invalid());
            }
        }
        Msg::StartSession(start) => {
            session_id(&start.session_id)?;
            if !matches!(start.profile, 1 | 2)
                || !(1..=1000).contains(&start.initial_authorization_ms)
            {
                return Err(invalid());
            }
            let local = address(&start.local_bind_ip, start.local_scope_id)?;
            let peer = address(&start.peer_ip, start.peer_scope_id)?;
            if local.is_ipv4() != peer.is_ipv4()
                || !port(start.local_udp_port, true)
                || !port(start.peer_udp_port, false)
            {
                return Err(invalid());
            }
            match start.role {
                1 => {
                    if start.render.is_some() {
                        return Err(invalid());
                    }
                    let capture = start.capture.as_ref().ok_or_else(invalid)?;
                    match capture.kind {
                        1 if capture.device.is_none() => {}
                        2 | 3 => selection(capture.device.as_ref().ok_or_else(invalid)?)?,
                        _ => return Err(invalid()),
                    }
                }
                2 => {
                    if start.capture.is_some() {
                        return Err(invalid());
                    }
                    selection(start.render.as_ref().ok_or_else(invalid)?)?;
                }
                _ => return Err(invalid()),
            }
            for material in [&start.forward, &start.reverse] {
                let material = material.as_ref().ok_or_else(invalid)?;
                if material.key.len() != 32 || material.nonce_prefix.len() != 4 {
                    return Err(invalid());
                }
            }
        }
        Msg::StopSession(stop) => session_id(&stop.session_id)?,
        Msg::RenewSession(renew) => {
            session_id(&renew.session_id)?;
            if !(1..=1000).contains(&renew.ttl_ms) {
                return Err(invalid());
            }
        }
        Msg::SetGain(gain) => {
            session_id(&gain.session_id)?;
            if gain.gain_percent > 100 {
                return Err(invalid());
            }
        }
        Msg::ListDevices(query) => {
            if !matches!(query.direction, 1 | 2)
                || query.page > 3
                || (query.page == 0) != (query.inventory_revision == 0)
            {
                return Err(invalid());
            }
        }
        Msg::Devices(page) => {
            if !matches!(page.direction, 1 | 2)
                || page.page > 3
                || page.inventory_revision == 0
                || page.devices.len() > DEVICES_PER_PAGE
                || page.page == 3 && !page.complete
            {
                return Err(invalid());
            }
            for (index, device) in page.devices.iter().enumerate() {
                text(&device.id, 512, false)?;
                text(&device.name, 256, true)?;
                if device.direction != page.direction
                    || device.sample_rate == 0
                    || device.sample_rate > 384_000
                    || device.channels == 0
                    || device.channels > 32
                    || page.devices[..index]
                        .iter()
                        .any(|other| other.id == device.id)
                {
                    return Err(invalid());
                }
            }
        }
        Msg::SessionEvent(event) => {
            session_id(&event.session_id)?;
            if !matches!(event.kind, 1..=6)
                || (event.kind == 5 && !matches!(event.reason, 1..=9))
                || (event.kind != 5 && event.reason != 0)
                || (event.kind == 1 && !port(event.local_udp_port, false))
                || (event.kind != 1 && event.local_udp_port != 0)
                || !matches!(event.path_diagnostic, 0..=4)
                || (event.path_diagnostic != 0 && (event.kind != 5 || event.reason != 5))
            {
                return Err(invalid());
            }
        }
        Msg::Health(health) => {
            if health.sessions.len() > MAX_SESSIONS {
                return Err(invalid());
            }
            for (index, session) in health.sessions.iter().enumerate() {
                session_id(&session.session_id)?;
                if session.counters.is_none()
                    || !matches!(session.state, 1..=3)
                    || !matches!(session.clock_source, 0..=4)
                    || session.path.as_ref().is_some_and(|path| {
                        !matches!(path.diagnostic, 0..=4)
                            || path.probes_sent > 3
                            || path.validated && path.diagnostic != 0
                    })
                    || session.latency.as_ref().is_some_and(|latency| {
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
                        .iter()
                        .any(|delay| *delay > 2_000_000)
                    })
                    || session.drift.as_ref().is_some_and(|drift| {
                        !(-5000..=5000).contains(&drift.ratio_ppm)
                            || !drift.filtered_fill_error_frames.is_finite()
                            || drift.filtered_fill_error_frames.abs() > 48_000.0
                    })
                    || health.sessions[..index]
                        .iter()
                        .any(|other| other.session_id == session.session_id)
                {
                    return Err(invalid());
                }
            }
        }
        Msg::BackendEvent(event) => match event.kind {
            3 => {
                if !event.device_id.is_empty()
                    || !matches!(event.direction, 1 | 2)
                    || !matches!(event.default_role, 1 | 2)
                {
                    return Err(invalid());
                }
            }
            6 => {
                if !event.device_id.is_empty() || event.direction != 0 || event.default_role != 0 {
                    return Err(invalid());
                }
            }
            1 | 2 | 4 | 5 => {
                text(&event.device_id, 512, false)?;
                if event.direction != 0 || event.default_role != 0 {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        },
        Msg::Error(error) => {
            if !matches!(error.code, 1..=9)
                || !error.session_id.is_empty() && error.session_id.len() != 16
            {
                return Err(invalid());
            }
        }
        Msg::AcquireMicMute(request) => {
            selection(request.selection.as_ref().ok_or_else(invalid)?)?;
        }
        Msg::SetMicMute(request) => {
            if request.expected_generation == 0 {
                return Err(invalid());
            }
        }
        Msg::ConfigurePtt(request) => {
            if request.reserved.len() > MAX_PTT_RESERVED {
                return Err(invalid());
            }
            if let Some(binding) = &request.binding {
                hotkey(binding)?;
            }
            for (index, reserved) in request.reserved.iter().enumerate() {
                hotkey(reserved)?;
                if request.binding.as_ref() == Some(reserved)
                    || request.reserved[..index].contains(reserved)
                {
                    return Err(invalid());
                }
            }
        }
        Msg::PttGrant(request) => {
            if request.grant_id == 0 || !(1..=250).contains(&request.ttl_ms) {
                return Err(invalid());
            }
        }
        Msg::PttRevoke(request) => {
            if request.grant_id == 0 {
                return Err(invalid());
            }
        }
        Msg::MuteSnapshot(snapshot) => {
            text(&snapshot.endpoint_id, 512, false)?;
            if snapshot.generation == 0 {
                return Err(invalid());
            }
        }
        Msg::PttStatus(status) => {
            if !matches!(status.state, 1..=3) || status.state != 3 && !status.registered {
                return Err(invalid());
            }
            if status.state == 3 && !matches!(status.reason, 1..=3)
                || status.state != 3 && status.reason != 0
            {
                return Err(invalid());
            }
        }
        Msg::PttEvent(event) => {
            let valid = match event.kind {
                1 => {
                    event.expires_at_ns > event.observed_at_ns
                        && event.expires_at_ns - event.observed_at_ns <= MAX_PTT_GRANT_NS
                }
                2 => event.observed_at_ns == 0 && event.expires_at_ns > 0,
                3..=5 => event.observed_at_ns == 0 && event.expires_at_ns == 0,
                _ => false,
            };
            let scope_valid = if event.grant_id == 0 {
                match event.kind {
                    1 | 2 => event.reason == 0,
                    3 => {
                        if frame.request_id == 0 {
                            matches!(event.reason, 1..=4 | 6..=9)
                                && (!matches!(event.reason, 8 | 9) || event.safety_epoch != 0)
                        } else {
                            event.reason == w::PttReleaseReason::Revoked as i32
                        }
                    }
                    _ => false,
                }
            } else {
                match event.kind {
                    1 | 2 => event.reason == 0,
                    3 => matches!(event.reason, 1..=4 | 6),
                    4 => matches!(event.reason, 2 | 5 | 7),
                    5 => matches!(event.reason, 2 | 6),
                    _ => false,
                }
            };
            if !valid || !scope_valid || event.grant_id != 0 && event.safety_epoch != 0 {
                return Err(invalid());
            }
        }
        Msg::MediaCommand(command) => {
            if command.token == 0
                || command.expected_revision == 0
                || !matches!(command.command, 1..=5)
            {
                return Err(invalid());
            }
        }
        Msg::MediaSessions(snapshot) => {
            if snapshot.revision == 0 || snapshot.sessions.len() > MAX_MEDIA_SESSIONS {
                return Err(invalid());
            }
            for (index, session) in snapshot.sessions.iter().enumerate() {
                text(&session.app_id, 128, true)?;
                text(&session.title, 256, true)?;
                text(&session.subtitle, 256, true)?;
                if session.token == 0
                    || session.revision == 0
                    || !matches!(session.state, 0..=3)
                    || session.commands & !31 != 0
                    || snapshot.sessions[..index]
                        .iter()
                        .any(|other| other.token == session.token)
                {
                    return Err(invalid());
                }
            }
            if snapshot.current_token.is_some_and(|token| {
                token == 0
                    || !snapshot
                        .sessions
                        .iter()
                        .any(|session| session.token == token)
            }) {
                return Err(invalid());
            }
        }
        Msg::MediaCommandResult(result) => {
            if !matches!(result.result, 1..=5) {
                return Err(invalid());
            }
        }
        Msg::ReleaseMicMute(_) | Msg::PttReleaseAll(_) | Msg::SetMediaWatch(_) => {}
        Msg::Shutdown(_) | Msg::ProbeBackend(_) => {}
    }
    if frame.encoded_len() > protocol::Channel::Ipc.max_frame_len() {
        return Err(invalid());
    }
    Ok(())
}
/// Same one-use private-file mechanism as the input agent; no token in argv/env/logging.
pub fn take_launch_token(path: &Path) -> Result<LaunchToken, IpcError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| IpcError::io("opening audio launch token", error))?;
    std::fs::remove_file(path)
        .map_err(|error| IpcError::io("consuming audio launch token", error))?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(33));
    file.by_ref()
        .take(33)
        .read_to_end(&mut bytes)
        .map_err(|error| IpcError::io("reading audio launch token", error))?;
    if bytes.len() != 32 {
        return Err(invalid());
    }
    let mut token = Zeroizing::new([0; 32]);
    token.copy_from_slice(&bytes);
    Ok(LaunchToken::new(token))
}
