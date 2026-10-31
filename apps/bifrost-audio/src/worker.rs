use super::*;
use audio_media::{
    crypto::DirectionMaterial,
    runtime::{self as media, MediaSession},
    socket::MediaSocket,
};
use audio_model::{jitter::Profile, status::StreamHealth};
use ring::digest::{digest, SHA256};
use std::{
    net::{IpAddr, SocketAddr, SocketAddrV6, UdpSocket},
    sync::mpsc::{Receiver, RecvTimeoutError},
};

pub(crate) type Pump<S> = Box<dyn FnMut(&mut S) -> Result<(), BackendError> + Send>;
const MAX_STARTS: usize = 256;
struct Running<S: Stream + 'static> {
    id: [u8; 16],
    role: i32,
    authorized: Arc<AtomicBool>,
    media: MediaSession<S>,
}
struct Inventory {
    request_id: u64,
    revision: u64,
    direction: i32,
    devices: Vec<w::Device>,
}
struct Worker<B: Backend + 'static>
where
    B::Stream: 'static,
{
    backend: Option<B>,
    factory: fn() -> Result<B, BackendError>,
    pump: Option<Pump<B::Stream>>,
    kind: i32,
    shared: Arc<Shared>,
    responses: mpsc::Sender<Response>,
    health: watch::Sender<w::Health>,
    sessions: Vec<Running<B::Stream>>,
    listener: Option<media::ListenerGroup<B::Stream>>,
    socket: Option<Arc<MediaSocket>>,
    used_ids: Vec<[u8; 16]>,
    used_keys: Vec<[u8; 32]>,
    inventory: Option<Inventory>,
    revision: u64,
}
pub(crate) fn run<B: Backend + 'static>(
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    responses: mpsc::Sender<Response>,
    health: watch::Sender<w::Health>,
    kind: i32,
    factory: fn() -> Result<B, BackendError>,
    pump: Option<Pump<B::Stream>>,
) where
    B::Stream: 'static,
{
    let mut worker = Worker {
        backend: None,
        factory,
        pump,
        kind,
        shared,
        responses,
        health,
        sessions: Vec::with_capacity(contract::MAX_SESSIONS),
        listener: None,
        socket: None,
        used_ids: Vec::with_capacity(MAX_STARTS),
        used_keys: Vec::with_capacity(MAX_STARTS * 2),
        inventory: None,
        revision: 0,
    };
    let (mut next_pump, mut next_health) = (Instant::now(), Instant::now());
    while !worker.shared.stopping.load(Ordering::Acquire) {
        match commands.recv_timeout(Duration::from_millis(5)) {
            Ok(command) => worker.command(command),
            Err(RecvTimeoutError::Disconnected) => {
                worker.shared.stop_all();
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next_pump {
            if let Some(pump) = worker.pump.as_mut() {
                for session in &mut worker.sessions {
                    if let Some(stream) = session.media.stream_mut() {
                        if pump(stream).is_err() {
                            session.media.on_backend_event(&Event::ResyncRequired);
                        }
                    }
                }
                if let Some(group) = worker.listener.as_mut() {
                    if let Some(stream) = group.stream_mut() {
                        if pump(stream).is_err() {
                            group.on_backend_event(&Event::ResyncRequired);
                        }
                    }
                }
            }
            next_pump += Duration::from_millis(10);
            if Instant::now().saturating_duration_since(next_pump) > Duration::from_millis(20) {
                next_pump = Instant::now() + Duration::from_millis(10);
            }
        }
        worker.events();
        if Instant::now() >= next_health {
            worker.push_health();
            // The IPC task still sends at 1 Hz; a fresher watch slot avoids a second
            // of phase lag and stale session membership when its timer fires first.
            next_health = Instant::now() + Duration::from_millis(200);
        }
    }
    worker.shared.stop_all();
    for mut session in worker.sessions.drain(..) {
        let _ = session.media.stop();
        worker.shared.remove(&session.id);
    }
    if let Some(group) = worker.listener.as_mut() {
        let _ = group.stop();
    }
}
impl<B: Backend + 'static> Worker<B>
where
    B::Stream: 'static,
{
    fn backend(&mut self) -> Result<&mut B, BackendError> {
        if self.backend.is_none() {
            self.backend = Some((self.factory)()?);
        }
        Ok(self.backend.as_mut().expect("initialized above"))
    }
    fn response(&self, request_id: u64, message: Msg) {
        if self
            .responses
            .try_send(Response {
                request_id,
                message,
            })
            .is_err()
        {
            self.shared.stop_all();
        }
    }
    fn command(&mut self, command: Command) {
        let Command {
            request_id,
            message,
            secrets,
            permit,
        } = command;
        let start_id = if let Msg::StartSession(start) = &message {
            Some(start.session_id.clone())
        } else {
            None
        };
        let result = match message {
            Msg::ProbeBackend(_) => {
                self.probe(request_id);
                Ok(())
            }
            Msg::ListDevices(query) => self.devices(request_id, query),
            Msg::StartSession(start) => self.start(start, secrets.ok_or(6), permit.ok_or(6)),
            Msg::StopSession(stop) => {
                self.stop(&stop.session_id, self.shared.stop_reason(&stop.session_id));
                Ok(())
            }
            Msg::SetGain(gain) => {
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|session| gain.session_id == session.id && session.role == 2)
                {
                    session
                        .media
                        .set_gain(gain.gain_percent as f32 / 100.0)
                        .map_err(|_| 6)
                        .and_then(|()| session.media.set_muted(gain.muted).map_err(|_| 6))
                } else {
                    Err(6)
                }
            }
            _ => Err(6),
        };
        if let Err(code) = result {
            let authorization_lost = if let Some(id) = &start_id {
                self.shared.revoke(id);
                let expired = self.shared.stop_reason(id) == 9;
                self.shared.remove(id);
                expired
            } else {
                false
            };
            if self.sessions.is_empty() {
                if let Some(mut group) = self.listener.take() {
                    if group.stop().is_err() {
                        self.shared.stop_all();
                    }
                }
                self.socket = None;
            }
            if authorization_lost {
                self.event(start_id.as_ref().expect("expired start"), 5, 9, 0);
                return;
            }
            self.response(
                request_id,
                Msg::Error(w::Error {
                    code,
                    session_id: start_id.unwrap_or_default(),
                }),
            );
        }
    }
    fn probe(&mut self, request_id: u64) {
        let kind = self.kind;
        let ready = match self.backend() {
            Err(error) => unavailable(kind, true, reason(error)),
            Ok(backend) => {
                let caps = backend.capabilities();
                let probe = backend.probe_capabilities();
                let render = backend.devices().is_ok_and(|devices| {
                    devices
                        .iter()
                        .any(|device| device.direction == Direction::Render)
                });
                let (mut capabilities, add) =
                    (Vec::with_capacity(6), |capability, availability, reason| {
                        w::CapabilityAvailability {
                            capability,
                            availability,
                            reason,
                        }
                    });
                capabilities.push(add(
                    1,
                    if caps.system_capture { 1 } else { 2 },
                    if caps.system_capture { 0 } else { 2 },
                ));
                capabilities.push(add(
                    2,
                    if render { 1 } else { 3 },
                    if render { 0 } else { 5 },
                ));
                let (availability, why) = match probe.process_exclude {
                    Availability::Available => (1, 0),
                    Availability::Unavailable(BackendError::Unsupported) => (2, 6),
                    Availability::Unavailable(error) => (3, reason(error)),
                };
                capabilities.push(add(3, availability, why));
                capabilities.push(add(4, 2, if kind == 3 { 7 } else { 8 }));
                capabilities.push(add(5, 2, 8));
                capabilities.push(add(6, 2, 9));
                w::Ready {
                    backend: kind,
                    probed: true,
                    capabilities,
                }
            }
        };
        self.response(request_id, Msg::Ready(ready));
    }
    fn devices(&mut self, request_id: u64, query: w::ListDevices) -> Result<(), i32> {
        if query.page == 0 {
            let backend = self.backend().map_err(code)?;
            let devices = backend.devices().map_err(code)?;
            let defaults = backend.defaults().map_err(code)?;
            let direction = if query.direction == 1 {
                Direction::Render
            } else {
                Direction::Capture
            };
            let mut inventory = Vec::new();
            for device in devices
                .into_iter()
                .filter(|device| device.direction == direction)
            {
                if inventory.len() == contract::MAX_DEVICES {
                    return Err(3);
                }
                let is_default=[&defaults.multimedia,&defaults.communications].iter().any(|defaults|if direction==Direction::Render{defaults.render.as_ref()}else{defaults.capture.as_ref()}==Some(&device.id));
                inventory.push(w::Device {
                    id: device.id.as_str().to_owned(),
                    name: device.label,
                    direction: query.direction,
                    sample_rate: device.format.sample_rate,
                    channels: u32::from(device.format.channels),
                    is_default,
                });
            }
            self.revision = self.revision.checked_add(1).ok_or(3)?;
            self.inventory = Some(Inventory {
                request_id,
                revision: self.revision,
                direction: query.direction,
                devices: inventory,
            });
        }
        let inventory = self.inventory.as_ref().ok_or(6)?;
        if inventory.request_id != request_id
            || inventory.direction != query.direction
            || query.page != 0 && inventory.revision != query.inventory_revision
        {
            return Err(6);
        }
        let start = query.page as usize * contract::DEVICES_PER_PAGE;
        if start > inventory.devices.len() {
            return Err(6);
        }
        let end = (start + contract::DEVICES_PER_PAGE).min(inventory.devices.len());
        self.response(
            request_id,
            Msg::Devices(w::Devices {
                direction: query.direction,
                page: query.page,
                inventory_revision: inventory.revision,
                devices: inventory.devices[start..end].to_vec(),
                complete: end == inventory.devices.len(),
            }),
        );
        Ok(())
    }
    fn start(
        &mut self,
        start: w::StartSession,
        secrets: Result<contract::SessionSecrets, i32>,
        authorized: Result<Arc<AtomicBool>, i32>,
    ) -> Result<(), i32> {
        let id: [u8; 16] = start.session_id.as_slice().try_into().map_err(|_| 6)?;
        let secrets = secrets?;
        let authorized = authorized?;
        if self.used_ids.contains(&id) {
            return Err(4);
        }
        if self.used_ids.len() >= MAX_STARTS || self.sessions.len() >= contract::MAX_SESSIONS {
            return Err(3);
        }
        let fingerprint = |key: &[u8]| -> [u8; 32] {
            digest(&SHA256, key)
                .as_ref()
                .try_into()
                .expect("SHA256 length")
        };
        let forward = fingerprint(secrets.forward.key());
        let reverse = fingerprint(secrets.reverse.key());
        if forward == reverse
            || self.used_keys.contains(&forward)
            || self.used_keys.contains(&reverse)
        {
            return Err(5);
        }
        self.used_ids.push(id);
        self.used_keys.extend([forward, reverse]);
        if !authorized.load(Ordering::Acquire) || self.shared.stopping.load(Ordering::Acquire) {
            return Err(6);
        }
        if self
            .sessions
            .iter()
            .any(|session| session.role != start.role)
        {
            return Err(6);
        }
        let bind = socket_address(
            &start.local_bind_ip,
            if start.local_udp_port == 0 {
                47903
            } else {
                start.local_udp_port
            },
            start.local_scope_id,
        )?;
        let peer = socket_address(&start.peer_ip, start.peer_udp_port, start.peer_scope_id)?;
        let socket = if let Some(socket) = &self.socket {
            if socket.local_addr().map_err(|_| 7)? != bind {
                return Err(6);
            }
            socket.clone()
        } else {
            let socket = MediaSocket::new(UdpSocket::bind(bind).map_err(|_| 7)?).map_err(|_| 7)?;
            self.socket = Some(socket.clone());
            socket
        };
        let port = u32::from(socket.local_addr().map_err(|_| 7)?.port());
        let profile = if start.profile == 1 {
            Profile::Ethernet
        } else {
            Profile::Wifi
        };
        let (forward_key, forward_prefix) = secrets.forward.into_parts();
        let (reverse_key, reverse_prefix) = secrets.reverse.into_parts();
        let material = media::SessionMaterial {
            forward: DirectionMaterial::new(forward_key, forward_prefix),
            reverse: DirectionMaterial::new(reverse_key, reverse_prefix),
        };
        let config = media::SessionConfig {
            socket,
            peer,
            session_id: id,
            profile,
            authorized: authorized.clone(),
        };
        if !authorized.load(Ordering::Acquire) {
            return Err(6);
        }
        let mut media = if start.role == 1 {
            if let Some(mut group) = self.listener.take() {
                group.stop().map_err(|_| 8)?;
            }
            let capture = start.capture.as_ref().ok_or(6)?;
            let spec = match capture.kind {
                1 => CaptureSpec::SystemExcludingOwnProcessTree { format: format() },
                2 => CaptureSpec::Monitor {
                    sink: selection(capture.device.as_ref().ok_or(6)?)?,
                    format: format(),
                },
                3 => CaptureSpec::EndpointLoopback {
                    endpoint: selection(capture.device.as_ref().ok_or(6)?)?,
                    format: format(),
                },
                _ => return Err(1),
            };
            media::start_source(self.backend().map_err(code)?, spec, config, material)
        } else {
            let request = RenderRequest {
                device: selection(start.render.as_ref().ok_or(6)?)?,
                requested: format(),
            };
            if let Some(group) = &self.listener {
                if !group.accepts(&request) {
                    return Err(6);
                }
            } else {
                let group = media::ListenerGroup::new(self.backend().map_err(code)?, request)
                    .map_err(media_error)?;
                self.listener = Some(group);
            }
            self.listener
                .as_mut()
                .expect("created above")
                .join(config, material)
        }
        .map_err(media_error)?;
        if !authorized.load(Ordering::Acquire) || self.shared.stopping.load(Ordering::Acquire) {
            let _ = media.stop();
            return Err(6);
        }
        self.sessions.push(Running {
            id,
            role: start.role,
            authorized,
            media,
        });
        self.event(&id, 1, 0, port);
        Ok(())
    }
    fn stop(&mut self, id: &[u8], reason: i32) {
        if let Some(index) = self.sessions.iter().position(|session| session.id == id) {
            let mut session = self.sessions.remove(index);
            session.authorized.store(false, Ordering::Release);
            if session.media.stop().is_err() {
                self.shared.stop_all();
                return;
            }
            if self.sessions.is_empty() {
                if let Some(mut group) = self.listener.take() {
                    if group.stop().is_err() {
                        self.shared.stop_all();
                        return;
                    }
                }
                self.socket = None;
            }
            self.shared.remove(id);
            self.event(id, 5, reason, 0);
        }
    }
    fn event(&self, id: &[u8], kind: i32, reason: i32, port: u32) {
        self.response(
            0,
            Msg::SessionEvent(w::SessionEvent {
                session_id: id.to_vec(),
                kind,
                reason,
                local_udp_port: port,
            }),
        );
    }
    fn events(&mut self) {
        for _ in 0..8 {
            let event = self.backend.as_mut().and_then(Backend::poll_event);
            let Some(event) = event else {
                break;
            };
            for session in &mut self.sessions {
                session.media.on_backend_event(&event);
            }
            if let Some(group) = self.listener.as_mut() {
                group.on_backend_event(&event);
            }
            let event = match event {
                Event::Device(event) => Some(match event {
                    DeviceEvent::Added(id) => backend_event(1, Some(id)),
                    DeviceEvent::Removed(id) => backend_event(2, Some(id)),
                    DeviceEvent::FormatChanged(id) => backend_event(4, Some(id)),
                    DeviceEvent::Invalidated(id) => backend_event(5, Some(id)),
                    DeviceEvent::DefaultChanged { direction, role } => w::BackendEvent {
                        kind: 3,
                        device_id: String::new(),
                        direction: match direction {
                            Direction::Render => 1,
                            Direction::Capture => 2,
                        },
                        default_role: match role {
                            DefaultRole::Multimedia => 1,
                            DefaultRole::Communications => 2,
                        },
                    },
                }),
                Event::ResyncRequired => Some(backend_event(6, None)),
                _ => None,
            };
            if let Some(event) = event {
                self.inventory = None;
                self.response(0, Msg::BackendEvent(event));
            }
        }
        if let Some(group) = self.listener.as_mut() {
            group.refresh();
        }
        let mut index = 0;
        while index < self.sessions.len() {
            if !self.sessions[index].authorized.load(Ordering::Acquire) {
                let id = self.sessions[index].id;
                self.stop(&id, self.shared.stop_reason(&id));
                continue;
            }
            let mut ended = None;
            while let Some(event) = self.sessions[index].media.poll_event() {
                let session = &self.sessions[index];
                match event {
                    media::MediaEvent::Started => self.event(&session.id, 6, 0, 0),
                    media::MediaEvent::Degraded => self.event(&session.id, 2, 0, 0),
                    media::MediaEvent::DeviceInvalidated => self.event(&session.id, 3, 0, 0),
                    media::MediaEvent::RotationRequired => self.event(&session.id, 4, 0, 0),
                    media::MediaEvent::Stopped { reason } => {
                        ended = Some(stop_reason(reason));
                        break;
                    }
                }
            }
            if let Some(reason) = ended {
                let id = self.sessions[index].id;
                self.stop(&id, reason);
            } else {
                index += 1;
            }
        }
    }
    fn push_health(&mut self) {
        let sessions = self
            .sessions
            .iter_mut()
            .map(|session| {
                let snapshot = session.media.snapshot();
                let health = snapshot.health;
                w::SessionHealth {
                    session_id: session.id.to_vec(),
                    counters: Some(w::AudioHealth {
                        received_packets: health.received_packets,
                        lost_frames: health.lost_frames,
                        late_frames: health.late_frames,
                        concealed_frames: health.concealed_frames,
                        underruns: health.underruns,
                        discontinuities: health.discontinuities,
                        limiter_frames: health.limiter_frames,
                    }),
                    state: match snapshot.state {
                        StreamHealth::Ready => 1,
                        StreamHealth::Buffering => 2,
                        StreamHealth::Degraded => 3,
                    },
                    capture_active: Some(snapshot.capture_active),
                    sending: Some(snapshot.sending),
                    sent_media_packets: snapshot.sent_media_packets,
                }
            })
            .collect();
        self.health.send_replace(w::Health { sessions });
    }
}
fn media_error(error: media::EngineError) -> i32 {
    match error {
        media::EngineError::Backend(error) => code(error),
        media::EngineError::Io(_) => 7,
        _ => 8,
    }
}
fn format() -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels: 2,
    }
}
fn selection(selection: &w::DeviceSelection) -> Result<DeviceSelection, i32> {
    match selection.selector.as_ref().ok_or(6)? {
        w::device_selection::Selector::DeviceId(id) => {
            Ok(DeviceSelection::Explicit(DeviceId::new(id).map_err(|_| 6)?))
        }
        w::device_selection::Selector::FollowDefault(role) => {
            Ok(DeviceSelection::FollowDefault(if *role == 1 {
                DefaultRole::Multimedia
            } else {
                DefaultRole::Communications
            }))
        }
    }
}
fn socket_address(ip: &str, port: u32, scope: u32) -> Result<SocketAddr, i32> {
    match ip.parse::<IpAddr>().map_err(|_| 6)? {
        IpAddr::V4(ip) => Ok(SocketAddr::new(IpAddr::V4(ip), port as u16)),
        IpAddr::V6(ip) => Ok(SocketAddr::V6(SocketAddrV6::new(ip, port as u16, 0, scope))),
    }
}
fn backend_event(kind: i32, id: Option<DeviceId>) -> w::BackendEvent {
    w::BackendEvent {
        kind,
        device_id: id.map(|id| id.as_str().to_owned()).unwrap_or_default(),
        direction: 0,
        default_role: 0,
    }
}
fn reason(error: BackendError) -> i32 {
    match error {
        BackendError::Unsupported => 2,
        BackendError::PermissionDenied => 4,
        BackendError::DeviceGone => 5,
        BackendError::ServerUnavailable => 3,
        _ => 10,
    }
}
fn code(error: BackendError) -> i32 {
    match error {
        BackendError::Unsupported => 1,
        BackendError::DeviceGone => 2,
        _ => 8,
    }
}
fn stop_reason(reason: media::StopReason) -> i32 {
    match reason {
        media::StopReason::User => 1,
        media::StopReason::DeviceInvalidated => 2,
        media::StopReason::NetworkFailure => 3,
        media::StopReason::WorkerFailed => 4,
        media::StopReason::PathTimeout => 5,
        media::StopReason::RotationRequired => 6,
        media::StopReason::AuthorizationLost => 9,
    }
}
