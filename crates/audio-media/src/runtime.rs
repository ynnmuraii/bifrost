//! Opt-in std-thread media engine. Construction requires an already authorized peer tuple and
//! fresh per-session material. Callers must destroy this handle on parent/consent/worker loss.
//! Backend/control polling stays on the owner thread; no core/IPC/identity dependency is present.
use crate::{
    control::ReceiverReport,
    crypto::{CryptoError, DirectionMaterial, Opener, Sealer},
    keys::{Endpoint, SessionKeys},
    native_rate::NativeRate,
    pipeline::{MediaError, Receiver, Sender},
    socket::{MediaSocket, Notification, SessionEndpoint},
};
use audio_backend::{
    AudioFormat, Backend, BackendError, CaptureSpec, DeviceEvent, Event as BackendEvent,
    RenderRequest, Stream, StreamSnapshot, StreamStatus,
};
use audio_model::{
    jitter::Profile,
    packet::{HEADER_LEN, MAX_DATAGRAM_LEN},
    status::{AudioHealth, ClockSource, DriftDiagnostics, StreamHealth},
    Header, PacketKind,
};
use ring::rand::{SecureRandom, SystemRandom};
use rtrb::{Consumer, RingBuffer};
use rubato::FixedAsync;
use std::{
    io,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        mpsc::{self, Receiver as EventReceiver, SyncSender},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod listener_group;
pub use listener_group::ListenerGroup;

pub const PCM_RING_SAMPLES: usize = 32_768;
pub const EVENT_CAPACITY: usize = 16;
pub const STOP_DEADLINE: Duration = Duration::from_secs(2);

/// Socket is bound by the caller; peer must match the authenticated parent's IP and accepted port.
/// The engine pins that complete tuple. Authenticated rebinding is intentionally unsupported.
pub struct SessionConfig {
    pub socket: Arc<MediaSocket>,
    pub peer: SocketAddr,
    pub session_id: [u8; 16],
    pub profile: Profile,
    /// Revocation is immediate on callbacks and UDP workers, even while native control blocks.
    pub authorized: Arc<AtomicBool>,
}
pub struct SessionMaterial {
    pub forward: DirectionMaterial,
    pub reverse: DirectionMaterial,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StopReason {
    User = 1,
    DeviceInvalidated = 2,
    NetworkFailure = 3,
    WorkerFailed = 4,
    PathTimeout = 5,
    RotationRequired = 6,
    AuthorizationLost = 7,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEvent {
    Started,
    Degraded,
    DeviceInvalidated,
    RotationRequired,
    Stopped { reason: StopReason },
}
#[derive(Debug, Clone, Copy)]
pub struct HealthSnapshot {
    pub health: AudioHealth,
    pub state: StreamHealth,
    pub sent_datagrams: u64,
    pub sent_media_packets: u64,
    pub capture_active: bool,
    pub sending: bool,
    pub rejected_datagrams: u64,
    pub capture_dropped_frames: u64,
    pub drift: Option<DriftDiagnostics>,
    pub clock_source: ClockSource,
    /// Worker loop entries, including notifications and deadline wakes; not native callbacks.
    pub worker_wakeups: u64,
    pub native: StreamSnapshot,
}
#[derive(Debug)]
pub enum EngineError {
    Backend(BackendError),
    Io(io::Error),
    Media(MediaError),
    InvalidFormat,
    Unauthorized,
    InvalidGain,
    StopTimeout,
    GroupFull,
    WorkerPanicked,
}
impl From<BackendError> for EngineError {
    fn from(e: BackendError) -> Self {
        Self::Backend(e)
    }
}
impl From<io::Error> for EngineError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<MediaError> for EngineError {
    fn from(e: MediaError) -> Self {
        Self::Media(e)
    }
}
impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for EngineError {}
#[derive(Clone, Copy)]
struct Stats {
    health: AudioHealth,
    state: StreamHealth,
    sent: u64,
    rejected: u64,
    drift: Option<DriftDiagnostics>,
}
struct Shared {
    stopped: AtomicBool,
    reason: AtomicU8,
    stopped_event: AtomicBool,
    started: AtomicBool,
    degraded: AtomicBool,
    muted: AtomicBool,
    gain: AtomicU32,
    native_rate: AtomicU32,
    invalidated: AtomicBool,
    callback_invalid: AtomicBool,
    render_frames: AtomicU64,
    render_clock: AtomicU64,
    callback_underruns: AtomicU64,
    callback_limiter: AtomicU64,
    capture_dropped: AtomicU64,
    capture_discontinuities: AtomicU64,
    last_media_ms: AtomicU64,
    authorized: Arc<AtomicBool>,
    clock: Instant,
    capture_frames: AtomicU64,
    capture_clock_ns: AtomicU64,
    clock_source: AtomicU8,
    worker_wakeups: AtomicU64,
    last_capture_ms: AtomicU64,
    sent_media_packets: AtomicU64,
    last_send_ms: AtomicU64,
    notification: Arc<Notification>,
    socket_rejected: Arc<AtomicU64>,
    stats: Mutex<Stats>,
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl Shared {
    fn new(authorized: Arc<AtomicBool>) -> Self {
        Self {
            stopped: AtomicBool::new(false),
            reason: AtomicU8::new(0),
            stopped_event: AtomicBool::new(false),
            started: AtomicBool::new(false),
            degraded: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            gain: AtomicU32::new(1.0f32.to_bits()),
            native_rate: AtomicU32::new(0),
            invalidated: AtomicBool::new(false),
            callback_invalid: AtomicBool::new(false),
            render_frames: AtomicU64::new(0),
            render_clock: AtomicU64::new(0),
            callback_underruns: AtomicU64::new(0),
            callback_limiter: AtomicU64::new(0),
            capture_dropped: AtomicU64::new(0),
            capture_discontinuities: AtomicU64::new(0),
            last_media_ms: AtomicU64::new(0),
            authorized,
            clock: Instant::now(),
            capture_frames: AtomicU64::new(0),
            capture_clock_ns: AtomicU64::new(0),
            clock_source: AtomicU8::new(ClockSource::Unknown as u8),
            worker_wakeups: AtomicU64::new(0),
            last_capture_ms: AtomicU64::new(0),
            sent_media_packets: AtomicU64::new(0),
            last_send_ms: AtomicU64::new(0),
            notification: Arc::new(Notification::new()),
            socket_rejected: Arc::new(AtomicU64::new(0)),
            stats: Mutex::new(Stats {
                health: AudioHealth::default(),
                state: StreamHealth::Buffering,
                sent: 0,
                rejected: 0,
                drift: None,
            }),
        }
    }
    fn is_stopped(&self) -> bool {
        if !self.authorized.load(Ordering::Acquire) {
            self.request_stop(StopReason::AuthorizationLost);
        }
        self.stopped.load(Ordering::Acquire)
    }
    fn request_stop(&self, reason: StopReason) {
        let _ = self
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.notification.notify();
        }
    }
    fn reason(&self) -> StopReason {
        match self.reason.load(Ordering::Acquire) {
            2 => StopReason::DeviceInvalidated,
            3 => StopReason::NetworkFailure,
            4 => StopReason::WorkerFailed,
            5 => StopReason::PathTimeout,
            6 => StopReason::RotationRequired,
            7 => StopReason::AuthorizationLost,
            _ => StopReason::User,
        }
    }
    fn finish(&self, events: &SyncSender<MediaEvent>) {
        if !self.stopped_event.swap(true, Ordering::AcqRel) {
            if self.reason() == StopReason::RotationRequired {
                let _ = events.try_send(MediaEvent::RotationRequired);
            }
            let _ = events.try_send(MediaEvent::Stopped {
                reason: self.reason(),
            });
        }
    }
    fn started(&self, events: &SyncSender<MediaEvent>) {
        if !self.started.swap(true, Ordering::AcqRel) {
            lock(&self.stats).state = StreamHealth::Ready;
            let _ = events.try_send(MediaEvent::Started);
        }
    }
    fn degraded(&self, events: &SyncSender<MediaEvent>) {
        lock(&self.stats).state = StreamHealth::Degraded;
        if !self.degraded.swap(true, Ordering::AcqRel) {
            let _ = events.try_send(MediaEvent::Degraded);
        }
    }
    fn invalidate(&self, events: &SyncSender<MediaEvent>) {
        if !self.invalidated.swap(true, Ordering::AcqRel) {
            let _ = events.try_send(MediaEvent::DeviceInvalidated);
        }
        self.request_stop(StopReason::DeviceInvalidated);
    }
}

/// Owner-thread control handle. `poll_event`/`snapshot` refresh native device status; the role also
/// forwards backend notifications through `on_backend_event`. Only native callbacks touch rings.
/// Stop gives all engine/native-stop threads a shared 2 s deadline. A backend violating its stop
/// contract yields StopTimeout, not an unbounded join; callbacks remain gated silent/discarded.
pub struct MediaSession<S: Stream + 'static> {
    stream: Option<S>,
    shared: Arc<Shared>,
    events: EventReceiver<MediaEvent>,
    event_tx: SyncSender<MediaEvent>,
    workers: Vec<JoinHandle<()>>,
    native_stop: Option<JoinHandle<Result<(), BackendError>>>,
    initial_position: Option<u64>,
    last_native: StreamSnapshot,
    source: bool,
    group_native: Option<Arc<Mutex<StreamSnapshot>>>,
    group_owner: Option<Box<ListenerGroup<S>>>,
    stop_attempted: bool,
}
impl<S: Stream + 'static> MediaSession<S> {
    /// Enables deterministic Dry pumping or owner-thread platform-specific control only.
    pub fn stream_mut(&mut self) -> Option<&mut S> {
        if let Some(group) = self.group_owner.as_mut() {
            return group.stream_mut();
        }
        self.stream.as_mut()
    }
    pub fn set_gain(&self, gain: f32) -> Result<(), EngineError> {
        if self.source || !gain.is_finite() || !(0.0..=2.0).contains(&gain) {
            return Err(EngineError::InvalidGain);
        }
        self.shared.gain.store(gain.to_bits(), Ordering::Release);
        Ok(())
    }
    pub fn set_muted(&self, muted: bool) -> Result<(), EngineError> {
        if self.source {
            return Err(EngineError::InvalidGain);
        }
        self.shared.muted.store(muted, Ordering::Release);
        Ok(())
    }
    fn refresh(&mut self) {
        if let Some(group) = self.group_owner.as_mut() {
            group.refresh();
        }
        if let Some(native) = &self.group_native {
            self.last_native = *lock(native);
        }
        if let Some(stream) = self.stream.as_ref() {
            let snapshot = stream.snapshot();
            self.last_native = snapshot;
            match snapshot.status {
                StreamStatus::DeviceInvalidated | StreamStatus::Failed(_) => {
                    self.shared.invalidate(&self.event_tx)
                }
                StreamStatus::Stopped if !self.shared.stopped.load(Ordering::Acquire) => {
                    self.shared.invalidate(&self.event_tx)
                }
                _ => {}
            }
            if !self.source {
                if let (Some(position), Some(base), Some(frequency)) = (
                    snapshot.device_position,
                    self.initial_position,
                    snapshot.clock_frequency,
                ) {
                    if frequency > 0 {
                        let count = (u128::from(position.saturating_sub(base)) * 48_000
                            / u128::from(frequency))
                        .min(u128::from(u64::MAX)) as u64;
                        let previous = self.shared.render_clock.fetch_max(count, Ordering::AcqRel);
                        if count > previous {
                            self.shared
                                .clock_source
                                .store(ClockSource::RenderDevicePosition as u8, Ordering::Release);
                        }
                    }
                }
            }
        }
    }
    pub fn snapshot(&mut self) -> HealthSnapshot {
        self.refresh();
        let stats = *lock(&self.shared.stats);
        let mut health = stats.health;
        health.underruns = health
            .underruns
            .saturating_add(self.shared.callback_underruns.load(Ordering::Acquire));
        health.limiter_frames = health
            .limiter_frames
            .saturating_add(self.shared.callback_limiter.load(Ordering::Acquire));
        health.discontinuities = health
            .discontinuities
            .saturating_add(self.shared.capture_discontinuities.load(Ordering::Acquire));
        let now = self.shared.clock.elapsed().as_millis() as u64;
        let stopped = self.shared.is_stopped();
        let media = self.shared.sent_media_packets.load(Ordering::Acquire);
        HealthSnapshot {
            health,
            state: stats.state,
            sent_datagrams: stats.sent,
            sent_media_packets: media,
            capture_active: self.source
                && self.last_native.status == StreamStatus::Running
                && self.shared.capture_frames.load(Ordering::Acquire) > 0
                && now.saturating_sub(self.shared.last_capture_ms.load(Ordering::Acquire)) < 100,
            sending: !stopped
                && media > 0
                && now.saturating_sub(self.shared.last_send_ms.load(Ordering::Acquire)) < 100,
            rejected_datagrams: stats
                .rejected
                .saturating_add(self.shared.socket_rejected.load(Ordering::Acquire)),
            capture_dropped_frames: self
                .shared
                .capture_dropped
                .load(Ordering::Acquire)
                .saturating_add(self.last_native.dropped_capture_frames),
            drift: stats.drift,
            clock_source: ClockSource::from_code(self.shared.clock_source.load(Ordering::Acquire)),
            worker_wakeups: self.shared.worker_wakeups.load(Ordering::Relaxed),
            native: self.last_native,
        }
    }
    pub fn poll_event(&mut self) -> Option<MediaEvent> {
        self.refresh();
        self.events.try_recv().ok()
    }
    pub fn on_backend_event(&mut self, event: &BackendEvent) {
        if let Some(group) = self.group_owner.as_mut() {
            group.on_backend_event(event);
        }
        let relevant = match event {
            BackendEvent::ResyncRequired => true,
            BackendEvent::Device(
                DeviceEvent::Removed(id)
                | DeviceEvent::Invalidated(id)
                | DeviceEvent::FormatChanged(id),
            ) => self.stream.as_ref().and_then(Stream::device) == Some(id),
            _ => false,
        };
        if relevant {
            self.shared.invalidate(&self.event_tx);
        }
    }
    pub fn stop(&mut self) -> Result<(), EngineError> {
        self.stop_attempted = true;
        self.shared.request_stop(StopReason::User);
        if let Some(group) = self.group_owner.as_mut() {
            group.stop()?;
        }
        let deadline = Instant::now() + STOP_DEADLINE;
        if let Some(mut stream) = self.stream.take() {
            self.native_stop = Some(
                thread::Builder::new()
                    .name("bfa-stop".into())
                    .spawn(move || stream.stop())?,
            );
        }
        while self.workers.iter().any(|worker| !worker.is_finished())
            || self
                .native_stop
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            if Instant::now() >= deadline {
                return Err(EngineError::StopTimeout);
            }
            thread::sleep(Duration::from_millis(1));
        }
        let mut result = Ok(());
        for worker in self.workers.drain(..) {
            if worker.join().is_err() {
                result = Err(EngineError::WorkerPanicked);
            }
        }
        if let Some(worker) = self.native_stop.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(EngineError::Backend(error)),
                Err(_) => result = Err(EngineError::WorkerPanicked),
            }
        }
        self.last_native.status = StreamStatus::Stopped;
        self.shared.finish(&self.event_tx);
        result
    }
}
impl<S: Stream + 'static> Drop for MediaSession<S> {
    fn drop(&mut self) {
        if !self.stop_attempted {
            let _ = self.stop();
        }
    }
}

fn valid_format(format: AudioFormat) -> bool {
    format.channels == 2 && (8_000..=audio_backend::MAX_SAMPLE_RATE).contains(&format.sample_rate)
}
fn make_session<S: Stream + 'static>(
    stream: S,
    shared: Arc<Shared>,
    events: EventReceiver<MediaEvent>,
    event_tx: SyncSender<MediaEvent>,
    source: bool,
) -> MediaSession<S> {
    let snapshot = stream.snapshot();
    MediaSession {
        stream: Some(stream),
        shared,
        events,
        event_tx,
        workers: Vec::with_capacity(2),
        native_stop: None,
        initial_position: if source {
            None
        } else {
            snapshot.device_position
        },
        last_native: snapshot,
        source,
        group_native: None,
        group_owner: None,
        stop_attempted: false,
    }
}

#[derive(Default)]
struct CaptureClock {
    previous_host: Option<u64>,
    previous_duration: u64,
    elapsed_ns: u64,
}

impl CaptureClock {
    fn advance(
        &mut self,
        timestamp: u64,
        valid: bool,
        frames: usize,
        rate: u32,
    ) -> (u64, ClockSource) {
        let duration = (frames as u64).saturating_mul(1_000_000_000) / u64::from(rate.max(1));
        let host_delta = self
            .previous_host
            .filter(|previous| valid && timestamp > *previous)
            .map(|previous| timestamp - previous);
        let source = if valid && (self.previous_host.is_none() || host_delta.is_some()) {
            ClockSource::CaptureHostTimestamp
        } else {
            ClockSource::CaptureCallbackFrames
        };
        self.elapsed_ns = self
            .elapsed_ns
            .saturating_add(host_delta.unwrap_or(self.previous_duration))
            .saturating_add(duration)
            .saturating_sub(self.previous_duration);
        self.previous_host = valid.then_some(timestamp);
        self.previous_duration = duration;
        (self.elapsed_ns, source)
    }
}

pub fn start_source<B: Backend>(
    backend: &mut B,
    spec: CaptureSpec,
    config: SessionConfig,
    material: SessionMaterial,
) -> Result<MediaSession<B::Stream>, EngineError>
where
    B::Stream: 'static,
{
    if !config.authorized.load(Ordering::Acquire) {
        return Err(EngineError::Unauthorized);
    }
    // MediaSocket already provides nonblocking session queues and one event-driven
    // reader; no per-session socket timeout can gate the encoder cadence.
    let start = Instant::now();
    let shared = Arc::new(Shared::new(config.authorized.clone()));
    let endpoint = config.socket.register(
        config.session_id,
        config.peer,
        shared.socket_rejected.clone(),
        shared.notification.clone(),
    )?;
    let (events_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
    let (mut producer, consumer) = RingBuffer::<f32>::new(PCM_RING_SAMPLES);
    let callback_shared = shared.clone();
    let mut capture_clock = CaptureClock::default();
    let mut stream = backend.start_capture(
        spec,
        Box::new(move |block| {
            if block.frames > 0 {
                callback_shared
                    .capture_frames
                    .fetch_add(block.frames as u64, Ordering::Relaxed);
                callback_shared.last_capture_ms.store(
                    callback_shared.clock.elapsed().as_millis() as u64,
                    Ordering::Release,
                );
            }
            if callback_shared.is_stopped() {
                return;
            }
            let expected = callback_shared.native_rate.load(Ordering::Acquire);
            if block.format.channels != 2
                || block.samples.len() != block.frames * 2
                || (expected != 0 && block.format.sample_rate != expected)
            {
                callback_shared
                    .callback_invalid
                    .store(true, Ordering::Release);
                callback_shared.notification.notify();
                return;
            }
            // Process-loopback position may stay constant: only host time or callback frames
            // advance the capture clock. Device position is deliberately never consulted.
            let (clock_ns, source) = capture_clock.advance(
                block.timestamp_ns,
                block.timestamp_valid,
                block.frames,
                block.format.sample_rate,
            );
            if block.discontinuity {
                callback_shared
                    .capture_discontinuities
                    .fetch_add(1, Ordering::Relaxed);
            }
            if producer.slots() < block.samples.len() {
                callback_shared
                    .capture_dropped
                    .fetch_add(block.frames as u64, Ordering::Relaxed);
                callback_shared
                    .capture_discontinuities
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
            for &sample in block.samples {
                let _ = producer.push(sample);
            }
            callback_shared
                .capture_clock_ns
                .store(clock_ns, Ordering::Release);
            callback_shared
                .clock_source
                .store(source as u8, Ordering::Release);
            callback_shared.notification.notify();
        }),
    )?;
    let format = stream.format();
    if !config.authorized.load(Ordering::Acquire) {
        stream.stop()?;
        return Err(EngineError::Unauthorized);
    }
    if !valid_format(format) {
        stream.stop()?;
        return Err(EngineError::InvalidFormat);
    }
    shared
        .native_rate
        .store(format.sample_rate, Ordering::Release);
    let frame = usize::from(config.profile.frame_samples());
    let converter = if format.sample_rate != 48_000 {
        Some(NativeRate::new(
            format.sample_rate,
            48_000,
            frame,
            FixedAsync::Output,
        )?)
    } else {
        None
    };
    let keys = SessionKeys::new(
        Endpoint::MediaSender,
        config.session_id,
        0,
        material.forward,
        material.reverse,
    )
    .map_err(MediaError::Crypto)?;
    let sender = Sender::new(config.profile, keys.send, 0)?;
    let path = PathState::new(start)?;
    let mut session = make_session(stream, shared.clone(), events, events_tx.clone(), true);
    let context = Context {
        endpoint,
        shared,
        events: events_tx,
        start,
    };
    let worker = SourceWorker {
        context,
        sender,
        opener: keys.receive,
        input: consumer,
        converter,
        frame,
        rate: format.sample_rate,
        path,
    };
    session.workers.push(
        thread::Builder::new()
            .name("bfa-encode".into())
            .spawn(move || worker.run())?,
    );
    Ok(session)
}

/// Attempts the Listener's authenticated challenge before returning. Commit/start the
/// Listener before the Source to establish outbound firewall state first; both sides probe.
pub fn start_listener<B: Backend>(
    backend: &mut B,
    request: RenderRequest,
    config: SessionConfig,
    material: SessionMaterial,
) -> Result<MediaSession<B::Stream>, EngineError>
where
    B::Stream: 'static,
{
    if !config.authorized.load(Ordering::Acquire) {
        return Err(EngineError::Unauthorized);
    }
    let mut group = ListenerGroup::new(backend, request)?;
    let mut session = group.join(config, material)?;
    session.group_owner = Some(Box::new(group));
    Ok(session)
}

struct Context {
    endpoint: SessionEndpoint,
    shared: Arc<Shared>,
    events: SyncSender<MediaEvent>,
    start: Instant,
}
impl Context {
    // The peer may not have bound UDP yet when the Listener commits first. ICMP
    // refusal is not authentication evidence; only the existing probe deadline decides.
    fn startup_refusal(&self, error: &io::Error) -> bool {
        !self.shared.started.load(Ordering::Acquire)
            && (matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
            ) || cfg!(windows) && error.raw_os_error() == Some(10054))
    }
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn send(&self, packet: &[u8]) -> Result<(), StopReason> {
        if self.shared.is_stopped() {
            return Err(self.shared.reason());
        }
        match self.endpoint.send(packet) {
            Ok(n) if n == packet.len() => {
                lock(&self.shared.stats).sent += 1;
                if packet.get(5) == Some(&(PacketKind::Opus as u8)) {
                    self.shared
                        .sent_media_packets
                        .fetch_add(1, Ordering::AcqRel);
                    self.shared.last_send_ms.store(
                        self.shared.clock.elapsed().as_millis() as u64,
                        Ordering::Release,
                    );
                }
                Ok(())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                self.shared.degraded(&self.events);
                Ok(())
            }
            Err(error) if self.startup_refusal(&error) => Ok(()),
            _ => Err(StopReason::NetworkFailure),
        }
    }
    fn receive(&mut self, packet: &mut [u8]) -> Result<Option<usize>, StopReason> {
        match self.endpoint.try_recv(packet) {
            Ok(Some(n)) => Ok(Some(n)),
            Ok(None) => Ok(None),
            Err(_) => Err(StopReason::NetworkFailure),
        }
    }
    fn wait_until(&self, deadline: Instant) {
        self.shared
            .notification
            .wait_timeout(deadline.saturating_duration_since(Instant::now()));
    }
    fn finish(&self, result: Result<(), StopReason>) {
        if let Err(reason) = result {
            self.shared.request_stop(reason);
        }
        self.shared.finish(&self.events);
    }
}
fn crypto_reason(error: CryptoError) -> StopReason {
    if error == CryptoError::RotationRequired {
        StopReason::RotationRequired
    } else {
        StopReason::WorkerFailed
    }
}
fn media_reason(error: MediaError) -> StopReason {
    match error {
        MediaError::Crypto(error) => crypto_reason(error),
        _ => StopReason::WorkerFailed,
    }
}
struct PathState {
    token: [u8; 16],
    start: Instant,
    next: Instant,
    attempts: u8,
    validated: bool,
    last_reply: Option<Instant>,
}
impl PathState {
    fn new(start: Instant) -> Result<Self, EngineError> {
        let mut token = [0; 16];
        SystemRandom::new()
            .fill(&mut token)
            .map_err(|_| EngineError::Media(MediaError::Crypto(CryptoError::Key)))?;
        Ok(Self {
            token,
            start,
            next: start,
            attempts: 0,
            validated: false,
            last_reply: None,
        })
    }
    fn deadline(&self) -> Option<Instant> {
        if self.validated {
            None
        } else if self.attempts < 3 {
            Some(self.next.min(self.start + Duration::from_secs(2)))
        } else {
            Some(self.start + Duration::from_secs(2))
        }
    }
    fn probe(&mut self) -> Result<Option<[u8; 16]>, StopReason> {
        if self.validated {
            return Ok(None);
        }
        let now = Instant::now();
        if now.duration_since(self.start) >= Duration::from_secs(2) {
            return Err(StopReason::PathTimeout);
        }
        if self.attempts < 3 && now >= self.next {
            self.attempts += 1;
            self.next = now + Duration::from_millis(500);
            Ok(Some(self.token))
        } else {
            Ok(None)
        }
    }
    fn control(&mut self, header: Header, payload: &[u8]) -> bool {
        if payload.len() != 16 {
            return false;
        }
        if header.kind == PacketKind::PathResponse
            && payload == self.token
            && self.attempts > 0
            && Instant::now().duration_since(self.next - Duration::from_millis(500))
                <= Duration::from_millis(500)
        {
            self.validated = true;
        }
        if header.kind == PacketKind::PathChallenge
            && self
                .last_reply
                .is_none_or(|last| last.elapsed() >= Duration::from_millis(100))
        {
            self.last_reply = Some(Instant::now());
            return true;
        }
        false
    }
}

struct SourceWorker {
    context: Context,
    sender: Sender,
    opener: Opener,
    input: Consumer<f32>,
    converter: Option<NativeRate>,
    frame: usize,
    rate: u32,
    path: PathState,
}
impl SourceWorker {
    fn run(mut self) {
        let result = self.process();
        self.context.finish(result);
    }
    fn process(&mut self) -> Result<(), StopReason> {
        let mut packet = [0; MAX_DATAGRAM_LEN + 1];
        let mut outgoing = [0; MAX_DATAGRAM_LEN];
        let input_capacity = self
            .converter
            .as_ref()
            .map_or(self.frame, NativeRate::max_input_frames);
        let mut input = vec![0.0; input_capacity * 2];
        let mut pcm = vec![0.0; self.frame * 2];
        let frame_duration = Duration::from_secs_f64(self.frame as f64 / 48_000.0);
        let mut next_frame = Instant::now();
        let mut next_keepalive = Instant::now();
        let mut dropped = 0;
        let mut capture_epoch = None;
        let mut discontinuities = 0;
        while !self.context.shared.is_stopped() {
            self.context
                .shared
                .worker_wakeups
                .fetch_add(1, Ordering::Relaxed);
            if self.context.shared.callback_invalid.load(Ordering::Acquire) {
                self.context.shared.invalidate(&self.context.events);
                return Err(StopReason::DeviceInvalidated);
            }
            let current = self
                .context
                .shared
                .capture_discontinuities
                .load(Ordering::Acquire);
            if current != discontinuities {
                discontinuities = current;
                self.context.shared.degraded(&self.context.events);
                if let Some(converter) = self.converter.as_mut() {
                    converter.reset();
                }
            }
            if let Some(token) = self.path.probe()? {
                let n = self
                    .sender
                    .control(
                        self.context.now_ms(),
                        PacketKind::PathChallenge,
                        &token,
                        &mut outgoing,
                    )
                    .map_err(crypto_reason)?;
                self.context.send(&outgoing[..n])?;
            }
            let mut drained = 0;
            while drained < 64 && !self.context.shared.is_stopped() {
                let Some(n) = self.context.receive(&mut packet)? else {
                    break;
                };
                drained += 1;
                match self.opener.open(&mut packet[..n], |_| true) {
                    Ok((header, payload)) => {
                        if self.path.control(header, payload) {
                            let n = self
                                .sender
                                .control(
                                    self.context.now_ms(),
                                    PacketKind::PathResponse,
                                    payload,
                                    &mut outgoing,
                                )
                                .map_err(crypto_reason)?;
                            self.context.send(&outgoing[..n])?;
                        }
                        if self.path.validated && header.kind == PacketKind::ReceiverReport {
                            if let Some(report) = ReceiverReport::decode(payload) {
                                let mut stats = lock(&self.context.shared.stats);
                                stats.health.received_packets =
                                    stats.health.received_packets.max(report.received);
                                stats.health.lost_frames =
                                    stats.health.lost_frames.max(report.lost);
                                stats.health.late_frames =
                                    stats.health.late_frames.max(report.late);
                            }
                        }
                    }
                    Err(_) => lock(&self.context.shared.stats).rejected += 1,
                }
            }
            if self.context.shared.is_stopped() {
                break;
            }
            if !self.path.validated {
                if drained < 64 {
                    self.context
                        .wait_until(self.path.deadline().expect("unvalidated path deadline"));
                }
                continue;
            }
            self.context.shared.started(&self.context.events);
            if Instant::now() >= next_keepalive {
                let n = self
                    .sender
                    .control(
                        self.context.now_ms(),
                        PacketKind::Keepalive,
                        &[],
                        &mut outgoing,
                    )
                    .map_err(crypto_reason)?;
                self.context.send(&outgoing[..n])?;
                next_keepalive = Instant::now() + Duration::from_secs(1);
            }
            let needed = self
                .converter
                .as_ref()
                .map_or(self.frame, NativeRate::input_frames);
            while self.input.slots() > needed * 4 {
                for _ in 0..needed * 2 {
                    let _ = self.input.pop();
                }
                self.sender.skip_frame();
                self.context.shared.degraded(&self.context.events);
                if let Some(converter) = self.converter.as_mut() {
                    converter.reset();
                }
            }
            let current_dropped = self.context.shared.capture_dropped.load(Ordering::Acquire);
            if current_dropped > dropped {
                let missing = ((current_dropped - dropped) * 48_000 / u64::from(self.rate))
                    .div_ceil(self.frame as u64);
                self.sender.skip_frames(missing);
                dropped = current_dropped;
                self.context.shared.degraded(&self.context.events);
            }
            let needed = self
                .converter
                .as_ref()
                .map_or(self.frame, NativeRate::input_frames);
            if self.input.slots() >= needed * 2 {
                let clock = Duration::from_nanos(
                    self.context.shared.capture_clock_ns.load(Ordering::Acquire),
                );
                let epoch = *capture_epoch.get_or_insert_with(|| {
                    Instant::now()
                        .checked_sub(clock)
                        .unwrap_or_else(Instant::now)
                });
                let queued = Duration::from_nanos(
                    (self.input.slots() / 2) as u64 * 1_000_000_000 / u64::from(self.rate),
                );
                // Host/QPC progression (or callback-frame fallback) bounds the oldest PCM's
                // send time, while the absolute media grid still prevents catch-up bursts.
                let capture_due = epoch + clock.saturating_sub(queued);
                next_frame = next_frame.max(capture_due);
            }
            if self.input.slots() >= needed * 2 && Instant::now() >= next_frame {
                for sample in &mut input[..needed * 2] {
                    *sample = self.input.pop().map_err(|_| StopReason::WorkerFailed)?;
                }
                if let Some(converter) = self.converter.as_mut() {
                    let n = converter
                        .process(&input[..needed * 2], &mut pcm)
                        .map_err(media_reason)?;
                    if n != self.frame {
                        return Err(StopReason::WorkerFailed);
                    }
                } else {
                    pcm.copy_from_slice(&input[..needed * 2]);
                }
                let n = self
                    .sender
                    .encode(self.context.now_ms(), &pcm, &mut outgoing)
                    .map_err(media_reason)?;
                self.context.send(&outgoing[..n])?;
                next_frame += frame_duration;
                if Instant::now() >= next_frame {
                    // Never send a catch-up burst after a stalled callback or scheduler.
                    next_frame = Instant::now() + frame_duration;
                }
            }
            let needed = self
                .converter
                .as_ref()
                .map_or(self.frame, NativeRate::input_frames);
            // An expired media deadline cannot wake us when capture has no complete frame.
            let deadline = if self.input.slots() >= needed * 2 {
                next_keepalive.min(next_frame)
            } else {
                next_keepalive
            };
            if drained < 64 {
                self.context.wait_until(deadline);
            }
        }
        Ok(())
    }
}

struct ReceiveWorker {
    context: Context,
    receiver: Arc<Mutex<Receiver>>,
    sealer: Sealer,
    path: PathState,
}
impl ReceiveWorker {
    fn send_probe(&mut self, outgoing: &mut [u8]) -> Result<(), StopReason> {
        if let Some(token) = self.path.probe()? {
            let n = self
                .sealer
                .seal(
                    self.context.now_ms(),
                    PacketKind::PathChallenge,
                    0,
                    0,
                    &token,
                    outgoing,
                )
                .map_err(crypto_reason)?;
            self.context.send(&outgoing[..n])?;
        }
        Ok(())
    }
    fn run(mut self) {
        let result = self.process();
        self.context.finish(result);
    }
    fn process(&mut self) -> Result<(), StopReason> {
        let mut packet = [0; MAX_DATAGRAM_LEN + 1];
        let mut outgoing = [0; MAX_DATAGRAM_LEN];
        let mut next_report = Instant::now();
        while !self.context.shared.is_stopped() {
            self.context
                .shared
                .worker_wakeups
                .fetch_add(1, Ordering::Relaxed);
            self.send_probe(&mut outgoing)?;
            let mut drained = 0;
            while drained < 64 && !self.context.shared.is_stopped() {
                let Some(n) = self.context.receive(&mut packet)? else {
                    break;
                };
                drained += 1;
                let now = self.context.shared.render_clock.load(Ordering::Acquire);
                let result = lock(&self.receiver).receive(now, &mut packet[..n], |header| {
                    header.kind != PacketKind::Opus || self.path.validated
                });
                match result {
                    Ok(header) => {
                        let payload =
                            &packet[HEADER_LEN..HEADER_LEN + usize::from(header.payload_len)];
                        if self.path.control(header, payload) {
                            let n = self
                                .sealer
                                .seal(
                                    self.context.now_ms(),
                                    PacketKind::PathResponse,
                                    0,
                                    0,
                                    payload,
                                    &mut outgoing,
                                )
                                .map_err(crypto_reason)?;
                            self.context.send(&outgoing[..n])?;
                        }
                        if header.kind == PacketKind::Opus {
                            self.context
                                .shared
                                .last_media_ms
                                .store(self.context.now_ms(), Ordering::Release);
                        }
                    }
                    Err(_) => lock(&self.context.shared.stats).rejected += 1,
                }
            }
            if self.path.validated {
                self.context.shared.started(&self.context.events);
                if Instant::now() >= next_report {
                    let receiver = lock(&self.receiver);
                    let health = receiver.health();
                    let report = ReceiverReport {
                        received: health.received_packets,
                        lost: health.lost_frames,
                        late: health.late_frames,
                        jitter_samples: receiver.arrival_variation_samples().min(u32::MAX as f64)
                            as u32,
                        buffer_samples: receiver.target_delay_samples(),
                    };
                    drop(receiver);
                    let n = self
                        .sealer
                        .seal(
                            self.context.now_ms(),
                            PacketKind::ReceiverReport,
                            0,
                            0,
                            &report.encode(),
                            &mut outgoing,
                        )
                        .map_err(crypto_reason)?;
                    self.context.send(&outgoing[..n])?;
                    next_report = Instant::now() + Duration::from_secs(1);
                }
            }
            // Revisit stop/path/report deadlines even under a continuously full queue.
            // A full batch can leave queued packets after their notification was consumed.
            if drained < 64 {
                self.context
                    .wait_until(self.path.deadline().unwrap_or(next_report));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    #[test]
    fn exhausted_path_attempts_wait_for_timeout_not_expired_probe() {
        let start = Instant::now();
        let mut path = PathState::new(start).unwrap();
        path.attempts = 3;
        path.next = start - Duration::from_secs(1);
        assert_eq!(path.deadline(), Some(start + Duration::from_secs(2)));
    }

    #[test]
    fn constant_capture_position_host_clock_and_fallback_keep_media_flowing() {
        use audio_backend::CaptureBlock;
        use zeroize::Zeroizing;
        for valid in [true, false] {
            let socket =
                MediaSocket::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
            let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            peer.set_nonblocking(true).unwrap();
            let shared = Arc::new(Shared::new(Arc::new(AtomicBool::new(true))));
            let endpoint = socket
                .register(
                    [8; 16],
                    peer.local_addr().unwrap(),
                    shared.socket_rejected.clone(),
                    shared.notification.clone(),
                )
                .unwrap();
            let (events, _events_rx) = mpsc::sync_channel(EVENT_CAPACITY);
            let (mut producer, input) = RingBuffer::new(PCM_RING_SAMPLES);
            let keys = SessionKeys::new(
                Endpoint::MediaSender,
                [8; 16],
                0,
                DirectionMaterial::new(Zeroizing::new([1; 32]), Zeroizing::new([2; 4])),
                DirectionMaterial::new(Zeroizing::new([3; 32]), Zeroizing::new([4; 4])),
            )
            .unwrap();
            let start = Instant::now();
            let mut path = PathState::new(start).unwrap();
            path.validated = true;
            let worker = SourceWorker {
                context: Context {
                    endpoint,
                    shared: shared.clone(),
                    events,
                    start,
                },
                sender: Sender::new(Profile::Ethernet, keys.send, 0).unwrap(),
                opener: keys.receive,
                input,
                converter: None,
                frame: 480,
                rate: 48_000,
                path,
            };
            let worker = thread::spawn(move || worker.run());
            let samples = [0.0; 960];
            let mut clock = CaptureClock::default();
            let mut previous = 0;
            for index in 0..30 {
                let block = CaptureBlock {
                    samples: &samples,
                    frames: 480,
                    format: AudioFormat {
                        sample_rate: 48_000,
                        channels: 2,
                    },
                    timestamp_ns: 9_000_000_000 + index * 10_000_000,
                    timestamp_valid: valid,
                    device_position: Some(0),
                    discontinuity: false,
                };
                let (time, source) = clock.advance(
                    block.timestamp_ns,
                    block.timestamp_valid,
                    block.frames,
                    block.format.sample_rate,
                );
                assert!(time > previous);
                previous = time;
                assert_eq!(
                    source,
                    if valid {
                        ClockSource::CaptureHostTimestamp
                    } else {
                        ClockSource::CaptureCallbackFrames
                    }
                );
                shared.capture_clock_ns.store(time, Ordering::Release);
                for &sample in block.samples {
                    producer.push(sample).unwrap();
                }
                shared.notification.notify();
                thread::sleep(Duration::from_millis(10));
            }
            assert!(shared.sent_media_packets.load(Ordering::Acquire) >= 28);
            let before_idle = shared.worker_wakeups.load(Ordering::Relaxed);
            thread::sleep(Duration::from_millis(200));
            assert!(shared.worker_wakeups.load(Ordering::Relaxed) - before_idle <= 4);
            assert!(shared.worker_wakeups.load(Ordering::Relaxed) < 160);
            let stopped = Instant::now();
            shared.request_stop(StopReason::User);
            worker.join().unwrap();
            assert!(stopped.elapsed() < Duration::from_millis(200));
        }
    }

    #[test]
    fn capture_host_clock_authority_uses_host_delta_and_invalid_time_uses_frames() {
        let mut clock = CaptureClock::default();
        assert_eq!(clock.advance(0, true, 480, 48_000).0, 10_000_000);
        assert_eq!(clock.advance(15_000_000, true, 480, 48_000).0, 25_000_000);
        assert_eq!(
            clock.advance(u64::MAX, false, 480, 48_000),
            (35_000_000, ClockSource::CaptureCallbackFrames),
        );
        assert_eq!(clock.advance(0, false, 480, 48_000).0, 45_000_000);
    }

    #[test]
    fn startup_icmp_refusal_never_validates_and_stops_being_transient_after_validation() {
        let socket = MediaSocket::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let peer = socket.local_addr().unwrap();
        let (events, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let shared = Arc::new(Shared::new(Arc::new(AtomicBool::new(true))));
        let context = Context {
            endpoint: socket
                .register(
                    [1; 16],
                    peer,
                    shared.socket_rejected.clone(),
                    shared.notification.clone(),
                )
                .unwrap(),
            shared,
            events,
            start: Instant::now(),
        };
        for kind in [
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(context.startup_refusal(&io::Error::from(kind)));
        }
        assert!(!context.shared.started.load(Ordering::Acquire));
        assert!(!context.startup_refusal(&io::Error::from(io::ErrorKind::PermissionDenied)));
        context.shared.started.store(true, Ordering::Release);
        assert!(!context.startup_refusal(&io::Error::from(io::ErrorKind::ConnectionRefused)));
    }
}
