//! Opt-in std-thread media engine. Construction requires an already authorized peer tuple and
//! fresh per-session material. Callers must destroy this handle on parent/consent/worker loss.
//! Backend/control polling stays on the owner thread; no core/IPC/identity dependency is present.
use crate::{
    control::ReceiverReport,
    crypto::{CryptoError, DirectionMaterial, Opener, Sealer},
    keys::{Endpoint, SessionKeys},
    mixer::Mixer,
    native_rate::NativeRate,
    pipeline::{MediaError, Receiver, Sender},
};
use audio_backend::{
    AudioFormat, Backend, BackendError, CaptureSpec, DeviceEvent, Event as BackendEvent,
    RenderRequest, Stream, StreamSnapshot, StreamStatus,
};
use audio_model::{
    jitter::Profile,
    packet::{HEADER_LEN, MAX_DATAGRAM_LEN},
    status::{AudioHealth, StreamHealth},
    Header, PacketKind,
};
use ring::rand::{SecureRandom, SystemRandom};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::FixedAsync;
use std::{
    io,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering},
        mpsc::{self, Receiver as EventReceiver, SyncSender},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const PCM_RING_SAMPLES: usize = 32_768;
pub const EVENT_CAPACITY: usize = 16;
pub const STOP_DEADLINE: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_millis(2);

/// Socket is bound by the caller; peer must match the authenticated parent's IP and accepted port.
/// The engine pins that complete tuple. Authenticated rebinding is intentionally unsupported.
pub struct SessionConfig {
    pub socket: UdpSocket,
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
    capture_dropped: AtomicU64,
    capture_discontinuities: AtomicU64,
    last_media_ms: AtomicU64,
    authorized: Arc<AtomicBool>,
    clock: Instant,
    capture_frames: AtomicU64,
    last_capture_ms: AtomicU64,
    sent_media_packets: AtomicU64,
    last_send_ms: AtomicU64,
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
            capture_dropped: AtomicU64::new(0),
            capture_discontinuities: AtomicU64::new(0),
            last_media_ms: AtomicU64::new(0),
            authorized,
            clock: Instant::now(),
            capture_frames: AtomicU64::new(0),
            last_capture_ms: AtomicU64::new(0),
            sent_media_packets: AtomicU64::new(0),
            last_send_ms: AtomicU64::new(0),
            stats: Mutex::new(Stats {
                health: AudioHealth::default(),
                state: StreamHealth::Buffering,
                sent: 0,
                rejected: 0,
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
        self.stopped.store(true, Ordering::Release);
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
}
impl<S: Stream + 'static> MediaSession<S> {
    /// Enables deterministic Dry pumping or owner-thread platform-specific control only.
    pub fn stream_mut(&mut self) -> Option<&mut S> {
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
                        self.shared.render_clock.fetch_max(count, Ordering::AcqRel);
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
            rejected_datagrams: stats.rejected,
            capture_dropped_frames: self
                .shared
                .capture_dropped
                .load(Ordering::Acquire)
                .saturating_add(self.last_native.dropped_capture_frames),
            native: self.last_native,
        }
    }
    pub fn poll_event(&mut self) -> Option<MediaEvent> {
        self.refresh();
        self.events.try_recv().ok()
    }
    pub fn on_backend_event(&mut self, event: &BackendEvent) {
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
        self.shared.request_stop(StopReason::User);
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
        if self.stream.is_some() || !self.shared.stopped.load(Ordering::Acquire) {
            let _ = self.stop();
        }
    }
}

fn socket_setup(socket: &UdpSocket) -> Result<(), EngineError> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(IO_TIMEOUT))?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(())
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
        initial_position: snapshot.device_position,
        last_native: snapshot,
        source,
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
    socket_setup(&config.socket)?;
    // Control reception must never gate capture/send deadlines. In particular,
    // Winsock's SO_RCVTIMEO rounds a 2 ms wait up to approximately 16 ms.
    config.socket.set_nonblocking(true)?;
    let start = Instant::now();
    let shared = Arc::new(Shared::new(config.authorized.clone()));
    let (events_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
    let (mut producer, consumer) = RingBuffer::<f32>::new(PCM_RING_SAMPLES);
    let callback_shared = shared.clone();
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
                return;
            }
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
        socket: config.socket,
        peer: config.peer,
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
    socket_setup(&config.socket)?;
    let start = Instant::now();
    let shared = Arc::new(Shared::new(config.authorized.clone()));
    let (events_tx, events) = mpsc::sync_channel(EVENT_CAPACITY);
    let (producer, mut consumer) = RingBuffer::<f32>::new(PCM_RING_SAMPLES);
    let callback_shared = shared.clone();
    let mut stream = backend.start_render(
        request,
        Box::new(move |output| {
            if callback_shared.is_stopped() {
                output.fill(0.0);
                return;
            }
            if !output.len().is_multiple_of(2) {
                output.fill(0.0);
                callback_shared
                    .callback_invalid
                    .store(true, Ordering::Release);
                return;
            }
            let mut underrun = false;
            for frame in output.as_chunks_mut::<2>().0 {
                // Never consume half a stereo frame if a producer is interrupted between pushes.
                if consumer.slots() >= 2 {
                    frame[0] = consumer.pop().unwrap_or(0.0);
                    frame[1] = consumer.pop().unwrap_or(0.0);
                } else {
                    frame.fill(0.0);
                    underrun = true;
                }
            }
            if underrun {
                callback_shared
                    .callback_underruns
                    .fetch_add(1, Ordering::Relaxed);
            }
            let total = callback_shared
                .render_frames
                .fetch_add((output.len() / 2) as u64, Ordering::AcqRel)
                + (output.len() / 2) as u64;
            let rate = callback_shared.native_rate.load(Ordering::Acquire);
            if rate > 0 {
                let clock = (u128::from(total) * 48_000 / u128::from(rate))
                    .min(u128::from(u64::MAX)) as u64;
                callback_shared
                    .render_clock
                    .fetch_max(clock, Ordering::AcqRel);
            }
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
            48_000,
            format.sample_rate,
            240,
            FixedAsync::Input,
        )?)
    } else {
        None
    };
    let keys = SessionKeys::new(
        Endpoint::MediaReceiver,
        config.session_id,
        0,
        material.forward,
        material.reverse,
    )
    .map_err(MediaError::Crypto)?;
    let receiver = Arc::new(Mutex::new(Receiver::new(
        config.profile,
        keys.receive,
        frame as u32,
    )?));
    let path = PathState::new(start)?;
    let mut session = make_session(stream, shared.clone(), events, events_tx.clone(), false);
    let feeder = PlayoutWorker {
        shared: shared.clone(),
        events: events_tx.clone(),
        receiver: receiver.clone(),
        output: producer,
        converter,
        rate: format.sample_rate,
        start,
    };
    let context = Context {
        socket: config.socket,
        peer: config.peer,
        shared,
        events: events_tx,
        start,
    };
    session.workers.push(
        thread::Builder::new()
            .name("bfa-playout".into())
            .spawn(move || feeder.run())?,
    );
    let worker = ReceiveWorker {
        context,
        receiver,
        sealer: keys.send,
        path,
    };
    session.workers.push(
        thread::Builder::new()
            .name("bfa-receive".into())
            .spawn(move || worker.run())?,
    );
    Ok(session)
}

struct Context {
    socket: UdpSocket,
    peer: SocketAddr,
    shared: Arc<Shared>,
    events: SyncSender<MediaEvent>,
    start: Instant,
}
impl Context {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn send(&self, packet: &[u8]) -> Result<(), StopReason> {
        if self.shared.is_stopped() {
            return Err(self.shared.reason());
        }
        match self.socket.send_to(packet, self.peer) {
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
            _ => Err(StopReason::NetworkFailure),
        }
    }
    fn receive(&self, packet: &mut [u8]) -> Result<Option<usize>, StopReason> {
        match self.socket.recv_from(packet) {
            Ok((n, peer)) if peer == self.peer && n <= MAX_DATAGRAM_LEN => Ok(Some(n)),
            Ok(_) => {
                lock(&self.shared.stats).rejected += 1;
                Ok(None)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            // Winsock reports truncation as WSAEMSGSIZE instead of returning the filled buffer.
            Err(error) if cfg!(windows) && error.raw_os_error() == Some(10040) => {
                lock(&self.shared.stats).rejected += 1;
                Ok(None)
            }
            Err(_) => Err(StopReason::NetworkFailure),
        }
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
        let mut discontinuities = 0;
        while !self.context.shared.is_stopped() {
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
            for _ in 0..8 {
                let Some(n) = self.context.receive(&mut packet)? else {
                    break;
                };
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
            if !self.path.validated {
                thread::sleep(Duration::from_millis(1));
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
                if Instant::now().saturating_duration_since(next_frame) > frame_duration * 2 {
                    next_frame = Instant::now() + frame_duration;
                }
            }
            // The absolute media grid, not receive timeouts or sleep durations, owns pacing.
            // Poll the capture ring at most every millisecond without busy-spinning.
            let wait = next_frame
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(1));
            thread::sleep(if wait.is_zero() {
                Duration::from_millis(1)
            } else {
                wait
            });
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
    fn run(mut self) {
        let result = self.process();
        self.context.finish(result);
    }
    fn process(&mut self) -> Result<(), StopReason> {
        let mut packet = [0; MAX_DATAGRAM_LEN + 1];
        let mut outgoing = [0; MAX_DATAGRAM_LEN];
        let mut next_report = Instant::now();
        while !self.context.shared.is_stopped() {
            if let Some(token) = self.path.probe()? {
                let n = self
                    .sealer
                    .seal(
                        self.context.now_ms(),
                        PacketKind::PathChallenge,
                        0,
                        0,
                        &token,
                        &mut outgoing,
                    )
                    .map_err(crypto_reason)?;
                self.context.send(&outgoing[..n])?;
            }
            if let Some(n) = self.context.receive(&mut packet)? {
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
        }
        Ok(())
    }
}

struct PlayoutWorker {
    shared: Arc<Shared>,
    events: SyncSender<MediaEvent>,
    receiver: Arc<Mutex<Receiver>>,
    output: Producer<f32>,
    converter: Option<NativeRate>,
    rate: u32,
    start: Instant,
}
impl PlayoutWorker {
    fn run(mut self) {
        let result = self.process();
        if let Err(reason) = result {
            self.shared.request_stop(reason);
        }
        self.shared.finish(&self.events);
    }
    fn process(&mut self) -> Result<(), StopReason> {
        let capacity = lock(&self.receiver).output_capacity_samples();
        let mut decoded = vec![0.0; capacity];
        let mut mixed = vec![0.0; capacity];
        let mut mixer = Mixer::new(480);
        let mut staging = [0.0; 480];
        let mut staged = 0;
        let converted_capacity = self
            .converter
            .as_ref()
            .map_or(0, NativeRate::max_output_frames)
            * 2;
        let mut converted = vec![0.0; converted_capacity];
        while !self.shared.is_stopped() {
            if self.shared.callback_invalid.load(Ordering::Acquire) {
                self.shared.invalidate(&self.events);
                return Err(StopReason::DeviceInvalidated);
            }
            let now = self.shared.render_clock.load(Ordering::Acquire);
            let fill = (PCM_RING_SAMPLES - self.output.slots()) as f64 / 2.0 * 48_000.0
                / f64::from(self.rate);
            let (n, mut health) = {
                let mut receiver = lock(&self.receiver);
                let n = receiver
                    .playout(now, fill, &mut decoded)
                    .map_err(media_reason)?;
                (n, receiver.health())
            };
            if n > 0 {
                let gain = if self.shared.muted.load(Ordering::Acquire) {
                    0.0
                } else {
                    f32::from_bits(self.shared.gain.load(Ordering::Acquire))
                };
                mixer
                    .set_gain(0, gain)
                    .map_err(|_| StopReason::WorkerFailed)?;
                mixer
                    .process(&[&decoded[..n * 2]], &mut mixed[..n * 2])
                    .map_err(|_| StopReason::WorkerFailed)?;
                if let Some(converter) = self.converter.as_mut() {
                    for &sample in &mixed[..n * 2] {
                        staging[staged] = sample;
                        staged += 1;
                        if staged == staging.len() {
                            let frames = converter
                                .process(&staging, &mut converted)
                                .map_err(media_reason)?;
                            push_pcm(
                                &mut self.output,
                                &converted[..frames * 2],
                                &self.shared,
                                &self.events,
                            );
                            staged = 0;
                        }
                    }
                } else {
                    push_pcm(
                        &mut self.output,
                        &mixed[..n * 2],
                        &self.shared,
                        &self.events,
                    );
                }
            }
            health.limiter_frames = mixer.limited_frames();
            let increased_loss = {
                let mut stats = lock(&self.shared.stats);
                let increased = health.lost_frames > stats.health.lost_frames;
                stats.health = health;
                increased
            };
            if increased_loss {
                self.shared.degraded(&self.events);
            }
            if self.shared.started.load(Ordering::Acquire)
                && self.start.elapsed().as_millis() as u64
                    > self.shared.last_media_ms.load(Ordering::Acquire) + 1000
            {
                self.shared.degraded(&self.events);
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}
fn push_pcm(
    output: &mut Producer<f32>,
    pcm: &[f32],
    shared: &Shared,
    events: &SyncSender<MediaEvent>,
) {
    if output.slots() < pcm.len() {
        shared.degraded(events);
        return;
    }
    for &sample in pcm {
        let _ = output.push(sample);
    }
}
