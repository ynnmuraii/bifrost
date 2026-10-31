use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use ipc::audio_worker::{self as contract, AudioLink, LaunchToken};
use ipc::transport::{self, ClientStream, Endpoint};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{timeout, Instant};

const QUEUE: usize = 64;
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn backend_matches(selected: w::BackendKind, reported: i32) -> bool {
    match selected {
        w::BackendKind::Dry => reported == w::BackendKind::Dry as i32,
        w::BackendKind::Native if cfg!(windows) => reported == w::BackendKind::WindowsWasapi as i32,
        w::BackendKind::Native => reported == w::BackendKind::LinuxPulse as i32,
        _ => false,
    }
}

pub struct AudioSupervisorConfig {
    /// Production callers use `std::env::current_exe()`.
    pub program: PathBuf,
    pub private_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub backend: w::BackendKind,
}

/// Privacy reductions must never be treated as a harmless dropped UI update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPrivacyOperation {
    DisableMediaWatch,
    MuteMicrophone,
    RevokePtt,
    ReleaseAllPtt,
    ReleaseMicMute,
    StopSession,
}
impl AudioPrivacyOperation {
    fn log_label(self) -> &'static str {
        match self {
            Self::DisableMediaWatch => "disable_media_watch",
            Self::MuteMicrophone => "mute_microphone",
            Self::RevokePtt => "revoke_ptt",
            Self::ReleaseAllPtt => "release_all_ptt",
            Self::ReleaseMicMute => "release_mic_mute",
            Self::StopSession => "stop_session",
        }
    }

    fn of(message: &Msg) -> Option<Self> {
        match message {
            Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }) => {
                Some(Self::DisableMediaWatch)
            }
            Msg::SetMicMute(w::SetMicMute { muted: true, .. }) => Some(Self::MuteMicrophone),
            Msg::PttRevoke(_) => Some(Self::RevokePtt),
            Msg::PttReleaseAll(_) => Some(Self::ReleaseAllPtt),
            Msg::ReleaseMicMute(_) => Some(Self::ReleaseMicMute),
            Msg::StopSession(_) => Some(Self::StopSession),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioRestartReason {
    SessionBudget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSupervisorError {
    Unavailable,
    QueueFull,
    PrivacyControlRejected,
    RestartRequired(AudioRestartReason),
    Timeout,
    InvalidFrame,
    WorkerRejected(i32),
    Launch(String),
    ChildLost,
    EventOverflow,
}
impl fmt::Display for AudioSupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "audio supervisor: {self:?}")
    }
}
impl std::error::Error for AudioSupervisorError {}
impl AudioSupervisorError {
    fn log_reason(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::QueueFull => "queue_full",
            Self::PrivacyControlRejected => "privacy_control_rejected",
            Self::RestartRequired(AudioRestartReason::SessionBudget) => "session_budget",
            Self::Timeout => "timeout",
            Self::InvalidFrame => "invalid_frame",
            Self::WorkerRejected(_) => "worker_rejected",
            Self::Launch(_) => "launch_failed",
            Self::ChildLost => "child_lost",
            Self::EventOverflow => "event_overflow",
        }
    }
}
type Result<T, E = AudioSupervisorError> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum AudioSupervisorEvent {
    Ready {
        generation: u64,
        pid: u32,
        ready: w::Ready,
    },
    Probed {
        generation: u64,
        request_id: u64,
        ready: w::Ready,
    },
    SessionEvent {
        generation: u64,
        event: w::SessionEvent,
    },
    Health {
        generation: u64,
        health: w::Health,
    },
    BackendEvent {
        generation: u64,
        event: w::BackendEvent,
    },
    MuteSnapshot {
        generation: u64,
        request_id: u64,
        snapshot: w::MuteSnapshot,
    },
    PttEvent {
        generation: u64,
        request_id: u64,
        event: w::PttEvent,
    },
    MediaSessions {
        generation: u64,
        sessions: w::MediaSessions,
    },
    MediaCommandResult {
        generation: u64,
        request_id: u64,
        result: w::MediaCommandResult,
    },
    CommandRejected {
        generation: u64,
        request_id: u64,
        error: w::Error,
    },
    /// Fatal: core must invalidate native state rather than assume the reduction happened.
    PrivacyControlRejected {
        generation: u64,
        request_id: u64,
        operation: AudioPrivacyOperation,
        error: w::Error,
    },
    /// The child retains key/ID tombstones until it is recycled, never evicts them.
    RestartRequired {
        generation: u64,
        reason: AudioRestartReason,
    },
    ChildExited {
        generation: u64,
        pid: u32,
        restartable: bool,
    },
    Failed {
        generation: u64,
        session_id: Option<[u8; 16]>,
        reason: AudioSupervisorError,
    },
}

#[derive(Clone)]
pub struct AudioHandle {
    commands: mpsc::Sender<Command>,
    priority: mpsc::Sender<Command>,
    talk_epoch: Arc<AtomicU64>,
    generation: Arc<AtomicU64>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
}
enum Reply {
    Written(u64),
    Ready(w::Ready),
    Devices(Vec<w::Device>),
    Shutdown,
}
struct Command {
    generation: u64,
    talk_epoch: u64,
    message: Msg,
    admitted_at: Instant,
    reply: oneshot::Sender<Result<Reply>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlReply {
    Mute,
    Ptt,
    Media,
}
struct PendingControl {
    kind: ControlReply,
    deadline: Instant,
}
struct PendingPrivacy {
    operation: AudioPrivacyOperation,
    // No native reply exists for StopSession/watch-off. A later Pong proves that
    // their synchronous queue-admission decision (including Error) has passed.
    after_ping: Option<u64>,
}

impl AudioHandle {
    async fn command(&self, message: Msg) -> Result<Reply> {
        let admitted_at = Instant::now();
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 {
            return Err(AudioSupervisorError::Unavailable);
        }
        let mut frame = w::AudioFrame {
            major: contract::FRAME_MAJOR,
            minor: contract::FRAME_MINOR,
            seq: 1,
            worker_generation: generation,
            request_id: 1,
            msg: Some(message),
        };
        contract::validate_frame(&frame).map_err(|_| AudioSupervisorError::InvalidFrame)?;
        let message = frame.msg.take().ok_or(AudioSupervisorError::InvalidFrame)?;
        let privacy = AudioPrivacyOperation::of(&message);
        let (reply, rx) = oneshot::channel();
        let priority = is_talk_fence(&message);
        let queue = if priority {
            &self.priority
        } else {
            &self.commands
        };
        let permit = queue
            .try_reserve()
            .map_err(|error| match (error, privacy) {
                (mpsc::error::TrySendError::Full(_), Some(operation)) => {
                    tracing::info!(
                        generation,
                        operation = operation.log_label(),
                        reason = "queue_full",
                        "audio supervisor privacy control rejected"
                    );
                    AudioSupervisorError::PrivacyControlRejected
                }
                (mpsc::error::TrySendError::Full(_), None) => AudioSupervisorError::QueueFull,
                (mpsc::error::TrySendError::Closed(_), _) => AudioSupervisorError::Unavailable,
            })?;
        // Reserving first keeps a rejected safety command from fencing live grants.
        let talk_epoch = if priority {
            self.talk_epoch
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1)
        } else {
            self.talk_epoch.load(Ordering::Acquire)
        };
        permit.send(Command {
            generation,
            talk_epoch,
            message,
            admitted_at,
            reply,
        });
        timeout(COMMAND_TIMEOUT, rx)
            .await
            .map_err(|_| AudioSupervisorError::Timeout)?
            .map_err(|_| AudioSupervisorError::Unavailable)?
    }
    pub async fn probe(&self) -> Result<w::Ready> {
        match self.command(Msg::ProbeBackend(w::ProbeBackend {})).await? {
            Reply::Ready(ready) => Ok(ready),
            _ => Err(AudioSupervisorError::InvalidFrame),
        }
    }
    pub async fn list_devices(&self, direction: w::Direction) -> Result<Vec<w::Device>> {
        match self
            .command(Msg::ListDevices(w::ListDevices {
                direction: direction as i32,
                page: 0,
                inventory_revision: 0,
            }))
            .await?
        {
            Reply::Devices(devices) => Ok(devices),
            _ => Err(AudioSupervisorError::InvalidFrame),
        }
    }
    async fn written(&self, message: Msg) -> Result<u64> {
        match self.command(message).await? {
            Reply::Written(id) => Ok(id),
            _ => Err(AudioSupervisorError::InvalidFrame),
        }
    }
    /// Acknowledges the validated wire write, not native session startup.
    pub async fn start_session(&self, start: w::StartSession) -> Result<u64> {
        self.written(Msg::StartSession(start)).await
    }
    pub async fn stop_session(&self, session_id: [u8; 16]) -> Result<u64> {
        self.written(Msg::StopSession(w::StopSession {
            session_id: session_id.to_vec(),
        }))
        .await
    }
    /// Acknowledges only the wire write; rejection arrives as `CommandRejected`.
    /// The caller alone authorizes renewals; the supervisor never renews automatically.
    pub async fn renew_session(&self, session_id: [u8; 16], ttl_ms: u32) -> Result<u64> {
        self.written(Msg::RenewSession(w::RenewSession {
            session_id: session_id.to_vec(),
            ttl_ms,
        }))
        .await
    }
    pub async fn set_gain(&self, session_id: [u8; 16], percent: u32, muted: bool) -> Result<u64> {
        self.written(Msg::SetGain(w::SetGain {
            session_id: session_id.to_vec(),
            gain_percent: percent,
            muted,
        }))
        .await
    }
    pub async fn acquire_mic_mute(&self, selection: w::DeviceSelection) -> Result<u64> {
        self.written(Msg::AcquireMicMute(w::AcquireMicMute {
            selection: Some(selection),
        }))
        .await
    }
    pub async fn set_mic_mute(&self, expected_generation: u64, muted: bool) -> Result<u64> {
        self.written(Msg::SetMicMute(w::SetMicMute {
            expected_generation,
            muted,
        }))
        .await
    }
    pub async fn release_mic_mute(&self) -> Result<u64> {
        self.written(Msg::ReleaseMicMute(w::ReleaseMicMute {}))
            .await
    }
    pub async fn configure_ptt(
        &self,
        binding: Option<w::PttHotkey>,
        reserved: Vec<w::PttHotkey>,
    ) -> Result<u64> {
        self.written(Msg::ConfigurePtt(w::ConfigurePtt { binding, reserved }))
            .await
    }
    /// Opens a remote talk grant for at most 250 ms; Ping never renews it.
    pub async fn ptt_grant(&self, grant_id: u64, ttl_ms: u32) -> Result<u64> {
        self.written(Msg::PttGrant(w::PttGrant { grant_id, ttl_ms }))
            .await
    }
    pub async fn ptt_revoke(&self, grant_id: u64) -> Result<u64> {
        self.written(Msg::PttRevoke(w::PttRevoke { grant_id }))
            .await
    }
    /// Clears all transient native and remote talk, preserving persistent unmute.
    pub async fn ptt_release_all(&self) -> Result<u64> {
        self.written(Msg::PttReleaseAll(w::PttReleaseAll {})).await
    }
    pub async fn set_media_watch(&self, enabled: bool) -> Result<u64> {
        self.written(Msg::SetMediaWatch(w::SetMediaWatch { enabled }))
            .await
    }
    pub async fn media_command(
        &self,
        token: u64,
        expected_revision: u64,
        command: w::MediaCommandKind,
    ) -> Result<u64> {
        self.written(Msg::MediaCommand(w::MediaCommand {
            token,
            expected_revision,
            command: command as i32,
        }))
        .await
    }
    pub async fn shutdown(&self) -> Result<()> {
        // Reserved stop admission: disabling audio also interrupts backoff/launch
        // and cannot be blocked by the ordinary command queue or generation zero.
        self.shutdown.send_replace(true);
        let mut stopped = self.stopped.clone();
        timeout(LAUNCH_TIMEOUT + IO_TIMEOUT * 2, async {
            loop {
                if *stopped.borrow_and_update() {
                    return Ok(());
                }
                stopped
                    .changed()
                    .await
                    .map_err(|_| AudioSupervisorError::Unavailable)?;
            }
        })
        .await
        .map_err(|_| AudioSupervisorError::Timeout)?
    }
}

pub struct AudioSupervisor;
impl AudioSupervisor {
    pub async fn start(
        config: AudioSupervisorConfig,
    ) -> Result<(AudioHandle, mpsc::Receiver<AudioSupervisorEvent>)> {
        if !matches!(config.backend, w::BackendKind::Native | w::BackendKind::Dry) {
            return Err(AudioSupervisorError::InvalidFrame);
        }
        let (commands, rx) = mpsc::channel(QUEUE);
        let (priority, priority_rx) = mpsc::channel(QUEUE);
        let talk_epoch = Arc::new(AtomicU64::new(0));
        let (events, event_rx) = mpsc::channel(QUEUE);
        let generation = Arc::new(AtomicU64::new(0));
        let (started, ready) = oneshot::channel();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (stopped_tx, stopped) = watch::channel(false);
        let actor_generation = generation.clone();
        let actor_talk_epoch = talk_epoch.clone();
        tokio::spawn(async move {
            run(
                config,
                rx,
                (priority_rx, actor_talk_epoch),
                events,
                actor_generation,
                started,
                shutdown_rx,
            )
            .await;
            stopped_tx.send_replace(true);
        });
        // Dropping this receiver on timeout also makes the actor reap a late child.
        timeout(LAUNCH_TIMEOUT, ready)
            .await
            .map_err(|_| AudioSupervisorError::Timeout)?
            .map_err(|_| AudioSupervisorError::Unavailable)??;
        Ok((
            AudioHandle {
                commands,
                priority,
                talk_epoch,
                generation,
                shutdown,
                stopped,
            },
            event_rx,
        ))
    }
}

struct TokenFile(PathBuf);
impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
struct Child {
    process: tokio::process::Child,
    _job: crate::job::KillOnCoreExit,
    _listener: transport::GuiListener,
    _token_file: TokenFile,
    link: AudioLink<ClientStream>,
    pid: u32,
    ready: w::Ready,
}
impl Child {
    async fn reap(&mut self) {
        let _ = self.process.start_kill();
        let _ = timeout(IO_TIMEOUT, self.process.wait()).await;
    }
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        // Volatile stores cannot be optimized out; no secret-bearing copies survive.
        // SAFETY: each pointer comes from an exclusive reference to a live byte.
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(Ordering::SeqCst);
}
fn token() -> Result<LaunchToken> {
    let mut bytes = [0; 32];
    for chunk in bytes.chunks_mut(16) {
        let mut part = match identity::random16() {
            Ok(part) => part,
            Err(error) => {
                wipe(&mut bytes);
                return Err(AudioSupervisorError::Launch(error.to_string()));
            }
        };
        chunk.copy_from_slice(&part);
        wipe(&mut part);
    }
    let secret = LaunchToken::new(bytes.into());
    wipe(&mut bytes);
    Ok(secret)
}

fn configure_lifetime(command: &mut tokio::process::Command) {
    command.kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        command.process_group(0);
        let parent = std::process::id() as i32;
        // SAFETY: the child closure calls only async-signal-safe OS functions, with no locks
        // or allocation. Checking getppid closes the parent-death-before-prctl race.
        unsafe {
            command.pre_exec(move || {
                unsafe extern "C" {
                    fn prctl(option: i32, ...) -> i32;
                    fn getppid() -> i32;
                }
                if prctl(1, 9usize, 0usize, 0usize, 0usize) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(10));
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

async fn launch(
    config: &AudioSupervisorConfig,
    generation: u64,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Child> {
    tracing::info!(
        generation,
        backend = config.backend as i32,
        "audio supervisor launching child"
    );
    use std::io::Write;
    // Names are process-global, so independent supervisors cannot steal each other's files.
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("audio-{}-{serial}", std::process::id());
    #[cfg(unix)]
    let endpoint = Endpoint::socket(config.private_dir.join(format!("{name}.sock")));
    #[cfg(windows)]
    let endpoint = Endpoint::pipe(format!(
        "{}{}{name}",
        transport::PIPE_PATH_PREFIX,
        transport::PIPE_NAME_PREFIX
    ));
    #[cfg(windows)]
    std::fs::create_dir_all(&config.private_dir)
        .map_err(|error| AudioSupervisorError::Launch(error.to_string()))?;
    let mut listener = transport::listen(&endpoint)
        .map_err(|error| AudioSupervisorError::Launch(error.to_string()))?;
    let secret = token()?;
    let token_path = config.private_dir.join(format!("{name}.token"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&token_path)
        .map_err(|error| AudioSupervisorError::Launch(error.to_string()))?;
    let token_file = TokenFile(token_path);
    file.write_all(secret.expose_secret())
        .and_then(|()| file.sync_all())
        .map_err(|error| AudioSupervisorError::Launch(error.to_string()))?;
    drop(file);
    let mut command = tokio::process::Command::new(&config.program);
    command
        .arg("audio")
        .arg("--ipc-endpoint")
        .arg(endpoint.describe())
        .arg("--token-file")
        .arg(&token_file.0)
        .arg("--logs-dir")
        .arg(&config.logs_dir)
        .arg("--worker-generation")
        .arg(generation.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    configure_lifetime(&mut command);
    let mut process = command
        .spawn()
        .map_err(|error| AudioSupervisorError::Launch(error.to_string()))?;
    let job = match crate::job::KillOnCoreExit::attach(&process) {
        Ok(job) => job,
        Err(error) => {
            let _ = process.start_kill();
            let _ = timeout(IO_TIMEOUT, process.wait()).await;
            return Err(AudioSupervisorError::Launch(error.to_string()));
        }
    };
    let attaching = timeout(LAUNCH_TIMEOUT, async {
        let stream = listener
            .accept(&ipc::peer::SameUser::current())
            .await
            .map_err(|_| AudioSupervisorError::ChildLost)?;
        let mut link =
            AudioLink::core(stream, generation).map_err(|_| AudioSupervisorError::InvalidFrame)?;
        link.send(
            1,
            Msg::Attach(w::Attach {
                launch_token: secret.expose_secret().to_vec(),
                backend: config.backend as i32,
                core_silence_ms: contract::CORE_SILENCE_MS,
            }),
        )
        .await
        .map_err(|_| AudioSupervisorError::ChildLost)?;
        let mut received = link
            .recv()
            .await
            .map_err(|_| AudioSupervisorError::ChildLost)?;
        if received.frame.request_id != 1 {
            return Err(AudioSupervisorError::InvalidFrame);
        }
        match received.frame.msg.take() {
            Some(Msg::Ready(ready))
                if !ready.probed && backend_matches(config.backend, ready.backend) =>
            {
                Ok((link, ready))
            }
            _ => Err(AudioSupervisorError::InvalidFrame),
        }
    });
    let attached = tokio::select! {
        biased;
        _ = shutdown_requested(shutdown) => Err(AudioSupervisorError::Unavailable),
        result = attaching => result.unwrap_or(Err(AudioSupervisorError::Timeout)),
    };
    match attached {
        Ok((link, ready)) => Ok(Child {
            pid: process.id().ok_or(AudioSupervisorError::ChildLost)?,
            process,
            _job: job,
            _listener: listener,
            _token_file: token_file,
            link,
            ready,
        }),
        Err(error) => {
            let _ = process.start_kill();
            let _ = timeout(IO_TIMEOUT, process.wait()).await;
            Err(error)
        }
    }
}

fn emit(events: &mpsc::Sender<AudioSupervisorEvent>, event: AudioSupervisorEvent) -> Result<()> {
    let health = matches!(event, AudioSupervisorEvent::Health { .. });
    match events.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) if health => Ok(()),
        Err(_) => Err(AudioSupervisorError::EventOverflow),
    }
}
struct Pending {
    reply: oneshot::Sender<Result<Reply>>,
    deadline: Instant,
    inventory: Option<Inventory>,
}
struct Inventory {
    direction: i32,
    page: u32,
    revision: u64,
    devices: Vec<w::Device>,
}

async fn send(child: &mut Child, id: u64, message: Msg) -> Result<()> {
    timeout(IO_TIMEOUT, child.link.send(id, message))
        .await
        .map_err(|_| AudioSupervisorError::Timeout)?
        .map_err(|_| AudioSupervisorError::InvalidFrame)
}
fn is_talk_fence(message: &Msg) -> bool {
    matches!(
        message,
        Msg::PttReleaseAll(_) | Msg::SetMicMute(w::SetMicMute { muted: true, .. })
    )
}

fn stale_talk(command: &Command, talk_epoch: &AtomicU64) -> bool {
    matches!(command.message, Msg::PttGrant(_))
        && command.talk_epoch != talk_epoch.load(Ordering::Acquire)
}

async fn next_command(
    priority: &mut mpsc::Receiver<Command>,
    commands: &mut mpsc::Receiver<Command>,
) -> Option<Command> {
    tokio::select! {
        biased;
        command = priority.recv() => command,
        command = commands.recv() => command,
    }
}

async fn serve(
    child: &mut Child,
    generation: u64,
    commands: &mut mpsc::Receiver<Command>,
    safety: (&mut mpsc::Receiver<Command>, &AtomicU64),
    events: &mpsc::Sender<AudioSupervisorEvent>,
    available: &AtomicU64,
    sessions: &mut BTreeSet<[u8; 16]>,
) -> Result<()> {
    let (priority, talk_epoch) = safety;
    let mut pending = BTreeMap::<u64, Pending>::new();
    let mut pending_starts = BTreeMap::<u64, [u8; 16]>::new();
    let mut pending_controls = BTreeMap::<u64, PendingControl>::new();
    let mut pending_privacy = BTreeMap::<u64, PendingPrivacy>::new();
    let mut request = 1u64;
    let mut ping = 0u64;
    let mut pong = 0u64;
    let mut last_frame = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(contract::PING_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async {
        loop {
            if commands.is_closed() { return Ok(()); }
            tokio::select! {
                biased;
                _ = tick.tick() => {
                    if last_frame.elapsed() >= IO_TIMEOUT { return Err(AudioSupervisorError::ChildLost); }
                    // A timed-out RPC invalidates this child, so late replies can never
                    // be reused by a later request or a replacement generation.
                    if pending.values().any(|response| Instant::now() >= response.deadline) { return Err(AudioSupervisorError::Timeout); }
                    if pending_controls.values().any(|control| Instant::now() >= control.deadline) { return Err(AudioSupervisorError::Timeout); }
                    if child.process.try_wait().map_err(|_| AudioSupervisorError::ChildLost)?.is_some() { return Err(AudioSupervisorError::ChildLost); }
                    ping = ping.checked_add(1).ok_or(AudioSupervisorError::InvalidFrame)?;
                    send(child, 0, Msg::Ping(w::Ping { serial: ping })).await?;
                }
                _ = events.closed() => return Ok(()),
                received = child.link.recv() => {
                    let mut received = received.map_err(|_| AudioSupervisorError::ChildLost)?;
                    last_frame = Instant::now();
                    let id = received.frame.request_id;
                    match received.frame.msg.take().ok_or(AudioSupervisorError::InvalidFrame)? {
                        Msg::Pong(value) if value.serial > pong && value.serial <= ping => {
                            pong = value.serial;
                            pending_privacy.retain(|_, pending| pending.after_ping.is_none_or(|sent| pong <= sent));
                        }
                        Msg::Ready(ready) => {
                            let response = pending.remove(&id).ok_or(AudioSupervisorError::InvalidFrame)?;
                            if response.inventory.is_some() || !ready.probed || ready.backend != child.ready.backend { return Err(AudioSupervisorError::InvalidFrame); }
                            emit(events, AudioSupervisorEvent::Probed { generation, request_id: id, ready: ready.clone() })?;
                            let _ = response.reply.send(Ok(Reply::Ready(ready)));
                        }
                        Msg::Devices(page) => {
                            let mut response = pending.remove(&id).ok_or(AudioSupervisorError::InvalidFrame)?;
                            let inventory = response.inventory.as_mut().ok_or(AudioSupervisorError::InvalidFrame)?;
                            if page.direction != inventory.direction || page.page != inventory.page || (inventory.page != 0 && page.inventory_revision != inventory.revision) || inventory.devices.iter().any(|old| page.devices.iter().any(|new| new.id == old.id)) { return Err(AudioSupervisorError::InvalidFrame); }
                            inventory.revision = page.inventory_revision;
                            inventory.devices.extend(page.devices);
                            if inventory.devices.len() > contract::MAX_DEVICES { return Err(AudioSupervisorError::InvalidFrame); }
                            if page.complete { let _ = response.reply.send(Ok(Reply::Devices(std::mem::take(&mut inventory.devices)))); }
                            else {
                                inventory.page += 1;
                                send(child, id, Msg::ListDevices(w::ListDevices { direction: inventory.direction, page: inventory.page, inventory_revision: inventory.revision })).await?;
                                pending.insert(id, response);
                            }
                        }
                        Msg::SessionEvent(event) => {
                            let session: [u8; 16] = event.session_id.as_slice().try_into().map_err(|_| AudioSupervisorError::InvalidFrame)?;
                            if !sessions.contains(&session) {
                                if event.kind == 5 { continue; }
                                return Err(AudioSupervisorError::InvalidFrame);
                            }
                            if matches!(event.kind, 1 | 5) {
                                pending_starts.retain(|_, id| *id != session);
                            }
                            if event.kind == 5 { sessions.remove(&session); }
                            emit(events, AudioSupervisorEvent::SessionEvent { generation, event })?;
                        }
                        Msg::Health(mut health) => {
                            // Health is a coalesced snapshot and may predate the last Stop event.
                            health.sessions.retain(|session| sessions.iter().any(|id| id.as_slice() == session.session_id));
                            emit(events, AudioSupervisorEvent::Health { generation, health })?;
                        }
                        Msg::BackendEvent(event) => emit(events, AudioSupervisorEvent::BackendEvent { generation, event })?,
                        Msg::MuteSnapshot(snapshot) => {
                            if id != 0 && pending_controls.remove(&id).map(|control| control.kind) != Some(ControlReply::Mute) {return Err(AudioSupervisorError::InvalidFrame);}
                            pending_privacy.remove(&id);
                            emit(events, AudioSupervisorEvent::MuteSnapshot {generation,request_id:id,snapshot})?;
                        }
                        Msg::PttEvent(event) => {
                            if id != 0 && pending_controls.remove(&id).map(|control| control.kind) != Some(ControlReply::Ptt) {return Err(AudioSupervisorError::InvalidFrame);}
                            pending_privacy.remove(&id);
                            emit(events, AudioSupervisorEvent::PttEvent {generation,request_id:id,event})?;
                        }
                        Msg::MediaSessions(sessions) => emit(events, AudioSupervisorEvent::MediaSessions {generation,sessions})?,
                        Msg::MediaCommandResult(result) => {
                            if pending_controls.remove(&id).map(|control| control.kind) != Some(ControlReply::Media) {return Err(AudioSupervisorError::InvalidFrame);}
                            emit(events, AudioSupervisorEvent::MediaCommandResult {generation,request_id:id,result})?;
                        }
                        Msg::Error(error) => {
                            if id <= 1 || id > request { return Err(AudioSupervisorError::InvalidFrame); }
                            let error = worker_rejection(events, generation, id, pending_privacy.remove(&id).map(|pending| pending.operation), pending_starts.contains_key(&id), error)?;
                            if let Some(response) = pending.remove(&id) { let _ = response.reply.send(Err(AudioSupervisorError::WorkerRejected(error.code))); }
                            // A rejected renewal/gain never deletes another live session.
                            if let Some(session) = pending_starts.remove(&id) { sessions.remove(&session); }
                            pending_controls.remove(&id);
                            emit(events, AudioSupervisorEvent::CommandRejected { generation, request_id: id, error })?;
                        }
                        _ => return Err(AudioSupervisorError::InvalidFrame),
                    }
                }
                command = next_command(priority, commands) => {
                    let Some(command) = command else { return Ok(()); };
                    if command.reply.is_closed() && !is_talk_fence(&command.message) { continue; }
                    if command.generation != generation {
                        let _ = command.reply.send(Err(AudioSupervisorError::Unavailable)); continue;
                    }
                    if stale_talk(&command, talk_epoch) {
                        let _ = command.reply.send(Err(AudioSupervisorError::Unavailable)); continue;
                    }
                    // The child holds one paged inventory snapshot at a time.
                    let privacy = AudioPrivacyOperation::of(&command.message);
                    let no_reply_privacy = pending_privacy.values().filter(|pending| pending.after_ping.is_some()).count();
                    let limit = if is_talk_fence(&command.message) { QUEUE * 2 } else { QUEUE };
                    if pending.len() + pending_controls.len() + no_reply_privacy >= limit || (matches!(&command.message, Msg::ListDevices(_)) && pending.values().any(|response| response.inventory.is_some())) {
                        if let Some(operation) = privacy {
                            let _ = command.reply.send(Err(AudioSupervisorError::PrivacyControlRejected));
                            worker_rejection(events, generation, 0, Some(operation), false, w::Error {code: 3, session_id: Vec::new()})?;
                        } else {
                            let _ = command.reply.send(Err(AudioSupervisorError::QueueFull));
                        }
                        continue;
                    }
                    let next_request = request.checked_add(1).ok_or(AudioSupervisorError::InvalidFrame)?;
                    // Handle validation bounds the queue; AudioLink validates the actual wire frame.
                    let mut message = command.message;
                    if let Err(error) = bound_grant_queue(&mut message, command.admitted_at, Instant::now()) {
                        let _ = command.reply.send(Err(error)); continue;
                    }
                    let shutdown = matches!(message, Msg::Shutdown(_));
                    let inventory = match &message { Msg::ListDevices(query) => Some(Inventory { direction: query.direction, page: 0, revision: 0, devices: Vec::new() }), _ => None };
                    let response = matches!(message, Msg::ProbeBackend(_) | Msg::ListDevices(_));
                    let control = match &message {
                        Msg::AcquireMicMute(_) | Msg::SetMicMute(_) | Msg::ReleaseMicMute(_) => Some(ControlReply::Mute),
                        Msg::ConfigurePtt(_) | Msg::PttGrant(_) | Msg::PttRevoke(_) | Msg::PttReleaseAll(_) => Some(ControlReply::Ptt),
                        Msg::MediaCommand(_) => Some(ControlReply::Media),
                        _ => None,
                    };
                    let session = match &message { Msg::StartSession(start) => Some(start.session_id.as_slice().try_into().map_err(|_| AudioSupervisorError::InvalidFrame)?), _ => None };
                    if let Some(id) = session {
                        if sessions.len() >= contract::MAX_SESSIONS || sessions.contains(&id) {
                            let _ = command.reply.send(Err(AudioSupervisorError::WorkerRejected(3))); continue;
                        }
                    }
                    request = next_request;
                    if let Err(error) = send(child, request, message).await { let _ = command.reply.send(Err(error.clone())); return Err(error); }
                    if let Some(id) = session { sessions.insert(id); pending_starts.insert(request, id); }
                    if let Some(kind) = control { pending_controls.insert(request, PendingControl {kind, deadline: Instant::now() + COMMAND_TIMEOUT}); }
                    if let Some(operation) = privacy {
                        pending_privacy.insert(request, PendingPrivacy { operation, after_ping: control.is_none().then_some(ping) });
                    }
                    if shutdown {
                        available.store(0, Ordering::Release);
                        if timeout(IO_TIMEOUT, child.process.wait()).await.is_err() { child.reap().await; }
                        let _ = command.reply.send(Ok(Reply::Shutdown)); return Ok(());
                    }
                    if response { pending.insert(request, Pending { reply: command.reply, deadline: Instant::now() + COMMAND_TIMEOUT, inventory }); }
                    else { let _ = command.reply.send(Ok(Reply::Written(request))); }
                }
            }
        }
    }.await;
    for (_, response) in pending {
        let _ = response.reply.send(Err(AudioSupervisorError::ChildLost));
    }
    result
}

fn worker_rejection(
    events: &mpsc::Sender<AudioSupervisorEvent>,
    generation: u64,
    request_id: u64,
    privacy: Option<AudioPrivacyOperation>,
    is_start: bool,
    error: w::Error,
) -> Result<w::Error> {
    if error.code == w::ErrorCode::ResourceLimit as i32 {
        if let Some(operation) = privacy {
            tracing::info!(
                generation,
                request_id,
                operation = operation.log_label(),
                reason = "resource_limit",
                "audio supervisor privacy control rejected"
            );
            emit(
                events,
                AudioSupervisorEvent::PrivacyControlRejected {
                    generation,
                    request_id,
                    operation,
                    error,
                },
            )?;
            return Err(AudioSupervisorError::PrivacyControlRejected);
        }
    }
    if error.code == w::ErrorCode::RestartRequired as i32 {
        if !is_start {
            return Err(AudioSupervisorError::InvalidFrame);
        }
        let reason = AudioRestartReason::SessionBudget;
        emit(
            events,
            AudioSupervisorEvent::RestartRequired { generation, reason },
        )?;
        return Err(AudioSupervisorError::RestartRequired(reason));
    }
    Ok(error)
}

// Whole milliseconds round down: time in the supervisor queue never extends a grant.
fn bound_grant_queue(message: &mut Msg, admitted_at: Instant, now: Instant) -> Result<()> {
    if let Msg::PttGrant(grant) = message {
        let deadline = admitted_at + Duration::from_millis(u64::from(grant.ttl_ms));
        let remaining = deadline
            .checked_duration_since(now)
            .ok_or(AudioSupervisorError::Timeout)?
            .as_millis();
        if remaining == 0 {
            return Err(AudioSupervisorError::Timeout);
        }
        grant.ttl_ms = u32::try_from(remaining).map_err(|_| AudioSupervisorError::InvalidFrame)?;
    }
    Ok(())
}

async fn run(
    config: AudioSupervisorConfig,
    mut commands: mpsc::Receiver<Command>,
    safety: (mpsc::Receiver<Command>, Arc<AtomicU64>),
    events: mpsc::Sender<AudioSupervisorEvent>,
    available: Arc<AtomicU64>,
    started: oneshot::Sender<Result<()>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (mut priority, talk_epoch) = safety;
    let mut first = Some(started);
    let mut generation = 1u64;
    let mut restarts = VecDeque::<Instant>::new();
    loop {
        if *shutdown.borrow() || commands.is_closed() || events.is_closed() {
            return;
        }
        let mut child = match launch(&config, generation, &mut shutdown).await {
            Ok(child) => child,
            Err(error) => {
                if *shutdown.borrow() {
                    return;
                }
                if let Some(first) = first.take() {
                    tracing::info!(
                        generation,
                        reason = error.log_reason(),
                        "audio supervisor terminal child failure"
                    );
                    let _ = first.send(Err(error));
                    return;
                }
                if emit(
                    &events,
                    AudioSupervisorEvent::Failed {
                        generation,
                        session_id: None,
                        reason: error,
                    },
                )
                .is_err()
                {
                    tracing::info!(
                        generation,
                        reason = "event_overflow",
                        "audio supervisor terminal child failure"
                    );
                    return;
                }
                // A failed restart consumes the same bounded restart budget as a lost child.
                while restarts
                    .front()
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(60))
                {
                    restarts.pop_front();
                }
                if restarts.len() >= 3 {
                    tracing::info!(
                        generation,
                        reason = "restart_budget_exhausted",
                        "audio supervisor terminal child failure"
                    );
                    return;
                }
                restarts.push_back(Instant::now());
                tracing::info!(
                    generation,
                    attempt = restarts.len(),
                    reason = "launch_failed",
                    "audio supervisor restarting child"
                );
                let Some(next_generation) = generation.checked_add(1) else {
                    tracing::info!(
                        generation,
                        reason = "generation_exhausted",
                        "audio supervisor terminal child failure"
                    );
                    return;
                };
                generation = next_generation;
                if !backoff(
                    &mut commands,
                    &mut priority,
                    &events,
                    restarts.len(),
                    &mut shutdown,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };
        tracing::info!(
            generation,
            pid = child.pid,
            "audio supervisor child attached"
        );
        if emit(
            &events,
            AudioSupervisorEvent::Ready {
                generation,
                pid: child.pid,
                ready: child.ready.clone(),
            },
        )
        .is_err()
        {
            tracing::info!(
                generation,
                reason = "event_overflow",
                "audio supervisor terminal child failure"
            );
            child.reap().await;
            return;
        }
        available.store(generation, Ordering::Release);
        if let Some(first) = first.take() {
            if first.send(Ok(())).is_err() {
                available.store(0, Ordering::Release);
                child.reap().await;
                return;
            }
        }
        let mut sessions = BTreeSet::new();
        let outcome = tokio::select! {
            biased;
            _ = shutdown_requested(&mut shutdown) => Ok(()),
            outcome = serve(&mut child, generation, &mut commands, (&mut priority, &talk_epoch), &events, &available, &mut sessions) => outcome,
        };
        available.store(0, Ordering::Release);
        if outcome.is_ok() {
            let _ = timeout(
                Duration::from_millis(250),
                child.link.send(u64::MAX, Msg::Shutdown(w::Shutdown {})),
            )
            .await;
            let _ = timeout(IO_TIMEOUT, child.process.wait()).await;
        }
        child.reap().await;
        while let Ok(command) = commands.try_recv() {
            let _ = command.reply.send(Err(AudioSupervisorError::Unavailable));
        }
        while let Ok(command) = priority.try_recv() {
            let _ = command.reply.send(Err(AudioSupervisorError::Unavailable));
        }
        let Err(reason) = outcome else {
            return;
        };
        while restarts
            .front()
            .is_some_and(|at| at.elapsed() >= Duration::from_secs(60))
        {
            restarts.pop_front();
        }
        let restartable = restarts.len() < 3 && reason != AudioSupervisorError::EventOverflow;
        let mut overflow = false;
        for session_id in sessions {
            overflow |= emit(
                &events,
                AudioSupervisorEvent::Failed {
                    generation,
                    session_id: Some(session_id),
                    reason: reason.clone(),
                },
            )
            .is_err();
        }
        overflow |= emit(
            &events,
            AudioSupervisorEvent::ChildExited {
                generation,
                pid: child.pid,
                restartable: restartable && !overflow,
            },
        )
        .is_err();
        if overflow || !restartable {
            tracing::info!(
                generation,
                reason = if overflow {
                    "event_overflow"
                } else {
                    reason.log_reason()
                },
                attempts = restarts.len(),
                "audio supervisor terminal child failure"
            );
            return;
        }
        restarts.push_back(Instant::now());
        tracing::info!(
            generation,
            attempt = restarts.len(),
            reason = reason.log_reason(),
            "audio supervisor restarting child"
        );
        let Some(next_generation) = generation.checked_add(1) else {
            tracing::info!(
                generation,
                reason = "generation_exhausted",
                "audio supervisor terminal child failure"
            );
            return;
        };
        generation = next_generation;
        if !backoff(
            &mut commands,
            &mut priority,
            &events,
            restarts.len(),
            &mut shutdown,
        )
        .await
        {
            return;
        }
    }
}
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn backoff(
    commands: &mut mpsc::Receiver<Command>,
    priority: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<AudioSupervisorEvent>,
    attempt: usize,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let delay = tokio::time::sleep(Duration::from_secs(1 << (attempt - 1)));
    tokio::pin!(delay);
    loop {
        tokio::select! {
            biased;
            _ = shutdown_requested(shutdown) => return false,
            _ = &mut delay => return true,
            _ = events.closed() => return false,
            command = next_command(priority, commands) => match command {
                Some(command) => { let _ = command.reply.send(Err(AudioSupervisorError::Unavailable)); }
                None => return false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> PathBuf {
        if let Some(path) = std::env::var_os("BIFROST_BIN") {
            return path.into();
        }
        if let Some(path) = option_env!("CARGO_BIN_EXE_bifrost") {
            return path.into();
        }
        let exe = std::env::current_exe().expect("test executable");
        let debug = exe
            .parent()
            .expect("deps directory")
            .parent()
            .expect("target profile");
        let path = debug.join(if cfg!(windows) {
            "bifrost.exe"
        } else {
            "bifrost"
        });
        assert!(
            path.is_file(),
            "build the real bifrost binary first or set BIFROST_BIN"
        );
        path
    }

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "bf-aud-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            Self(root)
        }
        fn config(&self, name: &str) -> AudioSupervisorConfig {
            AudioSupervisorConfig {
                program: binary(),
                private_dir: self.0.join(name),
                logs_dir: self.0.join(name).join("logs"),
                backend: w::BackendKind::Dry,
            }
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn next(events: &mut mpsc::Receiver<AudioSupervisorEvent>) -> AudioSupervisorEvent {
        timeout(Duration::from_secs(8), events.recv())
            .await
            .expect("supervisor event deadline")
            .expect("supervisor event channel")
    }
    async fn ready(events: &mut mpsc::Receiver<AudioSupervisorEvent>) -> (u64, u32) {
        loop {
            if let AudioSupervisorEvent::Ready {
                generation,
                pid,
                ready,
            } = next(events).await
            {
                assert!(!ready.probed);
                assert_eq!(ready.backend, w::BackendKind::Dry as i32);
                return (generation, pid);
            }
        }
    }
    fn port() -> u16 {
        loop {
            let socket =
                std::net::UdpSocket::bind("127.0.0.1:0").expect("loopback UDP reservation");
            let port = socket.local_addr().unwrap().port();
            if port != 47902 {
                return port;
            }
        }
    }
    fn start(role: i32, local: u16, peer: u16) -> w::StartSession {
        w::StartSession {
            session_id: vec![13; 16],
            role,
            profile: 1,
            capture: (role == 1).then_some(w::CaptureSpec {
                kind: 1,
                device: None,
            }),
            render: (role == 2).then_some(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(1)),
            }),
            local_bind_ip: "127.0.0.1".into(),
            local_udp_port: u32::from(local),
            peer_ip: "127.0.0.1".into(),
            peer_udp_port: u32::from(peer),
            forward: Some(w::DirectionMaterial {
                key: vec![21; 32],
                nonce_prefix: vec![22; 4],
            }),
            reverse: Some(w::DirectionMaterial {
                key: vec![23; 32],
                nonce_prefix: vec![24; 4],
            }),
            local_scope_id: 0,
            peer_scope_id: 0,
            initial_authorization_ms: 1000,
        }
    }

    #[test]
    fn health_overflow_drops_but_critical_overflow_fails_closed() {
        let (events, _rx) = mpsc::channel(1);
        emit(
            &events,
            AudioSupervisorEvent::Health {
                generation: 1,
                health: w::Health {
                    sessions: Vec::new(),
                },
            },
        )
        .unwrap();
        emit(
            &events,
            AudioSupervisorEvent::Health {
                generation: 1,
                health: w::Health {
                    sessions: Vec::new(),
                },
            },
        )
        .unwrap();
        assert_eq!(
            emit(
                &events,
                AudioSupervisorEvent::ChildExited {
                    generation: 1,
                    pid: 1,
                    restartable: false
                }
            ),
            Err(AudioSupervisorError::EventOverflow)
        );
    }

    #[tokio::test]
    async fn invalid_commands_never_enter_secret_queue() {
        let (commands, mut rx) = mpsc::channel(1);
        let handle = AudioHandle {
            commands,
            priority: mpsc::channel(1).0,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(1)),
            shutdown: watch::channel(false).0,
            stopped: watch::channel(true).1,
        };
        let mut invalid = start(1, 40001, 40002);
        invalid.forward.as_mut().unwrap().key.push(0);
        assert_eq!(
            handle.start_session(invalid).await,
            Err(AudioSupervisorError::InvalidFrame)
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            handle.set_gain([1; 16], 101, false).await,
            Err(AudioSupervisorError::InvalidFrame)
        );
        assert!(rx.try_recv().is_err());
        for ttl_ms in [0, 1001] {
            assert_eq!(
                handle.renew_session([1; 16], ttl_ms).await,
                Err(AudioSupervisorError::InvalidFrame)
            );
            let mut invalid = start(1, 40001, 40002);
            invalid.initial_authorization_ms = ttl_ms;
            assert_eq!(
                handle.start_session(invalid).await,
                Err(AudioSupervisorError::InvalidFrame)
            );
            assert!(rx.try_recv().is_err());
            for invalid_ttl in [0, 251] {
                assert_eq!(
                    handle.ptt_grant(1, invalid_ttl).await,
                    Err(AudioSupervisorError::InvalidFrame)
                );
            }
            assert_eq!(
                handle.ptt_grant(0, 250).await,
                Err(AudioSupervisorError::InvalidFrame)
            );
            assert_eq!(
                handle.ptt_revoke(0).await,
                Err(AudioSupervisorError::InvalidFrame)
            );
        }
    }

    #[tokio::test]
    async fn renew_accepts_boundary_ttls_and_acknowledges_only_writes() {
        let (commands, mut rx) = mpsc::channel(1);
        let handle = AudioHandle {
            commands,
            priority: mpsc::channel(1).0,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(7)),
            shutdown: watch::channel(false).0,
            stopped: watch::channel(true).1,
        };
        for ttl_ms in [1, 1000] {
            let (result, ()) = tokio::join!(handle.renew_session([3; 16], ttl_ms), async {
                let command = rx.recv().await.expect("explicit renewal");
                assert_eq!(command.generation, 7);
                assert!(
                    matches!(command.message, Msg::RenewSession(renew) if renew.session_id == vec![3; 16] && renew.ttl_ms == ttl_ms)
                );
                assert!(command.reply.send(Ok(Reply::Written(42))).is_ok());
            });
            assert_eq!(result, Ok(42));
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn native_controls_are_correlated_generation_fenced_write_requests() {
        let (commands, mut rx) = mpsc::channel(1);
        let (priority, mut priority_rx) = mpsc::channel(1);
        let handle = AudioHandle {
            commands,
            priority,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(7)),
            shutdown: watch::channel(false).0,
            stopped: watch::channel(true).1,
        };
        macro_rules! written {
            ($call:expr, $pattern:pat) => {{
                let (result, ()) = tokio::join!($call, async {
                    let command = next_command(&mut priority_rx, &mut rx)
                        .await
                        .expect("explicit native control");
                    assert_eq!(command.generation, 7);
                    assert!(matches!(command.message, $pattern));
                    assert!(command.reply.send(Ok(Reply::Written(42))).is_ok());
                });
                assert_eq!(result, Ok(42));
            }};
        }
        written!(
            handle.acquire_mic_mute(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(2))
            }),
            Msg::AcquireMicMute(_)
        );
        written!(handle.set_mic_mute(1, false), Msg::SetMicMute(_));
        written!(handle.release_mic_mute(), Msg::ReleaseMicMute(_));
        written!(handle.configure_ptt(None, Vec::new()), Msg::ConfigurePtt(_));
        written!(handle.ptt_grant(7, 250), Msg::PttGrant(_));
        written!(handle.ptt_revoke(7), Msg::PttRevoke(_));
        written!(handle.ptt_release_all(), Msg::PttReleaseAll(_));
        written!(handle.set_mic_mute(1, true), Msg::SetMicMute(_));
        written!(handle.set_media_watch(false), Msg::SetMediaWatch(_));
        written!(
            handle.media_command(7, 1, w::MediaCommandKind::Play),
            Msg::MediaCommand(_)
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn backoff_rejects_queued_previous_generation() {
        let (tx, mut rx) = mpsc::channel(1);
        let (priority, mut priority_rx) = mpsc::channel(1);
        let (events, _events_rx) = mpsc::channel(1);
        let (_shutdown, mut shutdown) = watch::channel(false);
        for message in [
            Msg::StartSession(start(1, 40001, 40002)),
            Msg::RenewSession(w::RenewSession {
                session_id: vec![13; 16],
                ttl_ms: 1000,
            }),
        ] {
            let (reply, response) = oneshot::channel();
            tx.send(Command {
                generation: 1,
                talk_epoch: 0,
                message,
                admitted_at: Instant::now(),
                reply,
            })
            .await
            .unwrap_or_else(|_| panic!("command queue"));
            let (reply, safety_response) = oneshot::channel();
            priority
                .try_send(Command {
                    generation: 1,
                    talk_epoch: 1,
                    message: Msg::PttReleaseAll(w::PttReleaseAll {}),
                    admitted_at: Instant::now(),
                    reply,
                })
                .unwrap_or_else(|_| panic!("priority queue"));
            assert!(backoff(&mut rx, &mut priority_rx, &events, 1, &mut shutdown).await);
            assert!(matches!(
                response.await.unwrap(),
                Err(AudioSupervisorError::Unavailable)
            ));
            assert!(rx.try_recv().is_err());
            assert!(matches!(
                safety_response.await.unwrap(),
                Err(AudioSupervisorError::Unavailable)
            ));
            assert!(priority_rx.try_recv().is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_child_dry_loopback_restart_and_reap() {
        let root = Root::new();
        let (source, mut source_events) = AudioSupervisor::start(root.config("s")).await.unwrap();
        let (listener, mut listener_events) =
            AudioSupervisor::start(root.config("l")).await.unwrap();
        let (generation, source_pid) = ready(&mut source_events).await;
        let (_, listener_pid) = ready(&mut listener_events).await;
        assert_ne!(source_pid, listener_pid);
        assert!(source.probe().await.unwrap().probed);
        assert!(listener.probe().await.unwrap().probed);
        assert!(!source
            .list_devices(w::Direction::Render)
            .await
            .unwrap()
            .is_empty());
        let a = port();
        let mut b = port();
        while a == b {
            b = port();
        }
        // Listener startup is synchronous in the child. Its wire-write acknowledgment
        // must not block Source from supplying the authenticated startup probes.
        listener.start_session(start(2, b, a)).await.unwrap();
        source.start_session(start(1, a, b)).await.unwrap();
        let mut sending = false;
        let mut receiving = false;
        let mut metadata_seen = false;
        let mut watch_requested = false;
        let mut renew = tokio::time::interval(Duration::from_millis(100));
        timeout(Duration::from_secs(10), async {
            while !sending || !receiving || !metadata_seen {
                tokio::select! {
                    _ = renew.tick() => {
                        source.renew_session([13; 16], 1000).await.unwrap();
                        listener.renew_session([13; 16], 1000).await.unwrap();
                    },
                    event = source_events.recv() => match event.expect("source events") {
                        AudioSupervisorEvent::MediaSessions { sessions, .. } => {
                            metadata_seen |= sessions.sessions.iter().any(|session| session.title == "Dry media");
                        },
                        AudioSupervisorEvent::Health { health, .. } => {
                            if let Some(session) = health.sessions.iter().find(|session| session.sending == Some(true) && session.capture_active == Some(true) && session.sent_media_packets > 0) {
                                assert_eq!(session.clock_source, 1, "Dry capture uses valid host timestamps");
                                assert!(session.drift.is_none(), "Source does not run a drift PI");
                                sending = true;
                            }
                            if sending && !watch_requested {
                                source.set_media_watch(true).await.unwrap();
                                watch_requested = true;
                            }
                        },
                        AudioSupervisorEvent::Failed { reason, .. } => panic!("source failed: {reason}"),
                        AudioSupervisorEvent::CommandRejected { error, .. } => panic!("source rejected: {error:?}"),
                        _ => {}
                    },
                    event = listener_events.recv() => match event.expect("listener events") {
                        AudioSupervisorEvent::Health { health, .. } => {
                            if let Some(session) = health.sessions.iter().find(|session| session.counters.as_ref().is_some_and(|counters| counters.received_packets > 0) && session.drift.is_some()) {
                                let drift = session.drift.as_ref().unwrap();
                                assert!((-1000..=1000).contains(&drift.ratio_ppm));
                                assert!(drift.filtered_fill_error_frames.is_finite());
                                assert!(drift.filtered_fill_error_frames.abs() <= 48_000.0);
                                assert!([3, 4].contains(&session.clock_source), "Listener identifies its actual render clock");
                                receiving = true;
                            }
                        },
                        AudioSupervisorEvent::Failed { reason, .. } => panic!("listener failed: {reason}"),
                        AudioSupervisorEvent::CommandRejected { error, .. } => panic!("listener rejected: {error:?}"),
                        _ => {}
                    },
                }
            }
        }).await.expect("real Dry media sending and receiving");
        listener.set_gain([13; 16], 75, false).await.unwrap();
        listener.stop_session([13; 16]).await.unwrap();
        loop {
            if let AudioSupervisorEvent::SessionEvent { event, .. } =
                next(&mut listener_events).await
            {
                if event.kind == 5 {
                    assert_eq!(event.reason, 1);
                    break;
                }
            }
        }
        let stopped_renewal = listener.renew_session([13; 16], 1000).await.unwrap();
        loop {
            if let AudioSupervisorEvent::CommandRejected {
                request_id, error, ..
            } = next(&mut listener_events).await
            {
                assert_eq!(error.code, 6);
                assert_eq!(error.session_id, vec![13; 16]);
                // Automatic lease renewals queued before StopSession can also
                // be rejected; only this explicit request completes the check.
                if request_id == stopped_renewal {
                    break;
                }
            }
        }
        #[cfg(unix)]
        assert!(std::process::Command::new("kill")
            .arg("-KILL")
            .arg(source_pid.to_string())
            .status()
            .unwrap()
            .success());
        #[cfg(windows)]
        assert!(std::process::Command::new("taskkill")
            .args(["/F", "/PID", &source_pid.to_string()])
            .status()
            .unwrap()
            .success());
        let mut failed = false;
        let mut exited = false;
        let replacement_pid;
        loop {
            match next(&mut source_events).await {
                AudioSupervisorEvent::Failed {
                    generation: old,
                    session_id: Some(id),
                    ..
                } => {
                    assert_eq!(old, generation);
                    assert_eq!(id, [13; 16]);
                    failed = true;
                }
                AudioSupervisorEvent::ChildExited {
                    generation: old,
                    pid,
                    restartable,
                } => {
                    assert_eq!(old, generation);
                    assert_eq!(pid, source_pid);
                    assert!(restartable);
                    exited = true;
                }
                AudioSupervisorEvent::Ready {
                    generation: fresh,
                    pid,
                    ready,
                } => {
                    assert!(fresh > generation);
                    assert!(!ready.probed);
                    replacement_pid = pid;
                    break;
                }
                _ => {}
            }
        }
        assert!(failed && exited);
        assert_ne!(source_pid, replacement_pid);
        // No StartSession replay: the fresh child reports no old sessions or sending.
        loop {
            if let AudioSupervisorEvent::Health {
                generation: fresh,
                health,
            } = next(&mut source_events).await
            {
                assert!(fresh > generation);
                assert!(health.sessions.is_empty());
                break;
            }
        }
        let rejected = source.renew_session([13; 16], 1000).await.unwrap();
        loop {
            if let AudioSupervisorEvent::CommandRejected {
                request_id, error, ..
            } = next(&mut source_events).await
            {
                assert_eq!(request_id, rejected);
                assert_eq!(error.code, 6);
                assert_eq!(error.session_id, vec![13; 16]);
                break;
            }
        }
        source.shutdown().await.unwrap();
        listener.shutdown().await.unwrap();
        for name in ["s", "l"] {
            let path = root.config(name).logs_dir.join("bifrost-audio.log");
            let bytes = std::fs::read(&path).expect("audio child must create its rotating log");
            let log = std::str::from_utf8(&bytes).expect("UTF-8 child log");
            assert!(
                log.contains("audio child attached"),
                "missing attach lifecycle"
            );
            assert!(
                log.contains("audio child stopped"),
                "missing graceful shutdown"
            );
            assert!(
                log.contains("audio backend probe result"),
                "missing probe lifecycle"
            );
            assert!(
                log.contains("audio session lifecycle"),
                "missing session lifecycle"
            );
            assert!(
                log.contains("event_kind=1"),
                "missing started session event"
            );
            if name == "l" {
                assert!(
                    log.contains("event_kind=5"),
                    "missing stopped session event"
                );
            }
            if name == "s" {
                assert!(
                    log.contains("audio child restarted"),
                    "missing restart lifecycle"
                );
                assert!(
                    log.lines().any(|line| line.contains("audio child attached")
                        && line.contains("worker_generation=2")),
                    "replacement attach must identify its worker generation"
                );
            }
            assert!(
                !log.contains("Dry media"),
                "private media title reached disk"
            );
            let session = start(1, a, b);
            for secret in [
                session.forward.as_ref().unwrap().key.as_slice(),
                session.reverse.as_ref().unwrap().key.as_slice(),
            ] {
                assert!(
                    !bytes.windows(secret.len()).any(|window| window == secret),
                    "raw secret reached child log"
                );
                let hex: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
                assert!(!log.contains(&hex), "hex secret reached child log");
                assert!(
                    !log.contains(&hex.to_ascii_uppercase()),
                    "uppercase hex secret reached child log"
                );
            }
        }
        #[cfg(target_os = "linux")]
        for pid in [source_pid, replacement_pid, listener_pid] {
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "child must be reaped"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_child_last_handle_drop_reaps() {
        let root = Root::new();
        let (handle, mut events) = AudioSupervisor::start(root.config("d")).await.unwrap();
        let (_, pid) = ready(&mut events).await;
        let clone = handle.clone();
        drop(handle);
        assert!(clone.probe().await.unwrap().probed);
        drop(clone);
        timeout(Duration::from_secs(3), async {
            while events.recv().await.is_some() {}
        })
        .await
        .expect("last handle closes supervisor");
        #[cfg(target_os = "linux")]
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "last handle drop reaps child"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
    }

    #[test]
    fn backend_policy_never_falls_back_between_native_and_dry() {
        assert!(backend_matches(
            w::BackendKind::Dry,
            w::BackendKind::Dry as i32
        ));
        assert!(!backend_matches(
            w::BackendKind::Native,
            w::BackendKind::Dry as i32
        ));
        for backend in [w::BackendKind::LinuxPulse, w::BackendKind::WindowsWasapi] {
            assert!(!backend_matches(w::BackendKind::Dry, backend as i32));
            assert_eq!(
                backend_matches(w::BackendKind::Native, backend as i32),
                backend
                    == if cfg!(windows) {
                        w::BackendKind::WindowsWasapi
                    } else {
                        w::BackendKind::LinuxPulse
                    }
            );
        }
        assert!(!backend_matches(
            w::BackendKind::Native,
            w::BackendKind::Native as i32
        ));
    }

    #[tokio::test]
    async fn shutdown_has_reserved_admission_without_a_live_generation() {
        let (commands, mut queue) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        commands
            .try_send(Command {
                generation: 1,
                talk_epoch: 0,
                message: Msg::ProbeBackend(w::ProbeBackend {}),
                admitted_at: Instant::now(),
                reply,
            })
            .unwrap_or_else(|_| panic!("fixture queue"));
        let (shutdown, mut requested) = watch::channel(false);
        let (finished, stopped) = watch::channel(false);
        let handle = AudioHandle {
            commands,
            priority: mpsc::channel(1).0,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(0)),
            shutdown,
            stopped,
        };
        let (result, ()) = tokio::join!(handle.shutdown(), async {
            shutdown_requested(&mut requested).await;
            assert_eq!(queue.len(), 1, "shutdown bypasses the saturated data queue");
            assert!(queue.try_recv().is_ok());
            finished.send_replace(true);
        });
        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn real_child_shutdown_during_backoff_prevents_restart() {
        let root = Root::new();
        let (handle, mut events) = AudioSupervisor::start(root.config("shutdown-backoff"))
            .await
            .unwrap();
        let (_, pid) = ready(&mut events).await;
        #[cfg(unix)]
        assert!(std::process::Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success());
        #[cfg(windows)]
        assert!(std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .unwrap()
            .success());
        loop {
            if let AudioSupervisorEvent::ChildExited { restartable, .. } = next(&mut events).await {
                assert!(restartable);
                break;
            }
        }
        handle.shutdown().await.unwrap();
        while let Some(event) = events.recv().await {
            assert!(
                !matches!(event, AudioSupervisorEvent::Ready { .. }),
                "shutdown must not restart a child"
            );
        }
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    #[test]
    fn queued_grants_keep_only_the_original_remaining_lifetime() {
        let now = Instant::now();
        let mut grant = Msg::PttGrant(w::PttGrant {
            grant_id: 7,
            ttl_ms: 250,
        });
        bound_grant_queue(&mut grant, now, now + Duration::from_millis(100)).unwrap();
        assert!(matches!(
            grant,
            Msg::PttGrant(w::PttGrant { ttl_ms: 150, .. })
        ));
        assert_eq!(
            bound_grant_queue(&mut grant, now, now + Duration::from_millis(251)),
            Err(AudioSupervisorError::Timeout)
        );
        let mut toggle = Msg::SetMicMute(w::SetMicMute {
            expected_generation: 1,
            muted: false,
        });
        assert!(bound_grant_queue(&mut toggle, now, now + Duration::from_secs(60)).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_child_mic_grant_expiry_and_restart_drop_native_ownership() {
        async fn mute(
            events: &mut mpsc::Receiver<AudioSupervisorEvent>,
            id: u64,
        ) -> w::MuteSnapshot {
            loop {
                match next(events).await {
                    AudioSupervisorEvent::MuteSnapshot {
                        request_id,
                        snapshot,
                        ..
                    } if request_id == id => return snapshot,
                    AudioSupervisorEvent::Failed { reason, .. } => {
                        panic!("native control failed: {reason}")
                    }
                    AudioSupervisorEvent::CommandRejected { error, .. } => {
                        panic!("native control rejected: {error:?}")
                    }
                    _ => {}
                }
            }
        }
        let root = Root::new();
        let (handle, mut events) = AudioSupervisor::start(root.config("m5")).await.unwrap();
        let (generation, pid) = ready(&mut events).await;
        assert!(handle.probe().await.unwrap().probed);
        let id = handle
            .acquire_mic_mute(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(2)),
            })
            .await
            .unwrap();
        let original = mute(&mut events, id).await;
        assert!(original.guard_active && !original.muted);
        let id = handle
            .set_mic_mute(original.generation, true)
            .await
            .unwrap();
        assert!(mute(&mut events, id).await.muted);
        let grant = handle.ptt_grant(73, 200).await.unwrap();
        let (mut opened, mut admitted, mut expired, mut remuted) = (false, false, false, false);
        timeout(Duration::from_secs(3), async {
            while !expired || !remuted {
                match next(&mut events).await {
                    AudioSupervisorEvent::MuteSnapshot { snapshot, .. } => {
                        assert!(snapshot.guard_active);
                        if !snapshot.muted {
                            opened = true;
                        }
                        if expired && snapshot.muted {
                            remuted = true;
                        }
                    }
                    AudioSupervisorEvent::PttEvent {
                        generation: observed_generation,
                        request_id,
                        event,
                    } => {
                        assert_eq!(observed_generation, generation);
                        if request_id == grant {
                            assert_eq!(event.kind, 1);
                            admitted = true;
                        }
                        if event.grant_id == 73 && event.kind == 3 && event.reason == 1 {
                            assert_eq!(request_id, 0);
                            expired = true;
                        }
                    }
                    AudioSupervisorEvent::Failed { reason, .. } => panic!("grant failed: {reason}"),
                    AudioSupervisorEvent::CommandRejected { error, .. } => {
                        panic!("grant rejected: {error:?}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("independent talk grant expiry");
        assert!(opened && admitted);
        #[cfg(unix)]
        assert!(std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success());
        #[cfg(windows)]
        assert!(std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .status()
            .unwrap()
            .success());
        let (next_generation, next_pid) = ready(&mut events).await;
        assert!(next_generation > generation);
        let release = handle.release_mic_mute().await.unwrap();
        loop {
            if let AudioSupervisorEvent::CommandRejected {
                generation,
                request_id,
                error,
            } = next(&mut events).await
            {
                if request_id == release {
                    assert_eq!(generation, next_generation);
                    assert_eq!(error.code, 6);
                    break;
                }
            }
        }
        handle.shutdown().await.unwrap();
        #[cfg(target_os = "linux")]
        for pid in [pid, next_pid] {
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }
        #[cfg(not(target_os = "linux"))]
        let _ = next_pid;
    }

    #[tokio::test]
    async fn privacy_resource_limits_are_typed_fatal_not_generic_rejections() {
        let (events, mut received) = mpsc::channel(8);
        for (message, operation) in [
            (
                Msg::SetMediaWatch(w::SetMediaWatch { enabled: false }),
                AudioPrivacyOperation::DisableMediaWatch,
            ),
            (
                Msg::SetMicMute(w::SetMicMute {
                    expected_generation: 1,
                    muted: true,
                }),
                AudioPrivacyOperation::MuteMicrophone,
            ),
            (
                Msg::PttRevoke(w::PttRevoke { grant_id: 1 }),
                AudioPrivacyOperation::RevokePtt,
            ),
            (
                Msg::PttReleaseAll(w::PttReleaseAll {}),
                AudioPrivacyOperation::ReleaseAllPtt,
            ),
            (
                Msg::ReleaseMicMute(w::ReleaseMicMute {}),
                AudioPrivacyOperation::ReleaseMicMute,
            ),
            (
                Msg::StopSession(w::StopSession {
                    session_id: vec![1; 16],
                }),
                AudioPrivacyOperation::StopSession,
            ),
        ] {
            assert_eq!(AudioPrivacyOperation::of(&message), Some(operation));
            assert_eq!(
                worker_rejection(
                    &events,
                    7,
                    12,
                    Some(operation),
                    false,
                    w::Error {
                        code: 3,
                        session_id: Vec::new()
                    }
                ),
                Err(AudioSupervisorError::PrivacyControlRejected)
            );
            assert!(
                matches!(received.recv().await, Some(AudioSupervisorEvent::PrivacyControlRejected { generation: 7, request_id: 12, operation: actual, error }) if actual == operation && error.code == 3)
            );
        }
        assert!(
            AudioPrivacyOperation::of(&Msg::SetMediaWatch(w::SetMediaWatch { enabled: true }))
                .is_none()
        );
        assert!(AudioPrivacyOperation::of(&Msg::SetMicMute(w::SetMicMute {
            expected_generation: 1,
            muted: false
        }))
        .is_none());
        assert!(worker_rejection(
            &events,
            7,
            13,
            None,
            false,
            w::Error {
                code: 3,
                session_id: Vec::new()
            }
        )
        .is_ok());
        assert!(received.try_recv().is_err());
        assert_eq!(
            worker_rejection(
                &events,
                7,
                14,
                None,
                true,
                w::Error {
                    code: 9,
                    session_id: vec![1; 16]
                }
            ),
            Err(AudioSupervisorError::RestartRequired(
                AudioRestartReason::SessionBudget
            ))
        );
        assert!(matches!(
            received.recv().await,
            Some(AudioSupervisorEvent::RestartRequired {
                generation: 7,
                reason: AudioRestartReason::SessionBudget
            })
        ));
        assert_eq!(
            worker_rejection(
                &events,
                7,
                15,
                None,
                false,
                w::Error {
                    code: 9,
                    session_id: Vec::new()
                }
            ),
            Err(AudioSupervisorError::InvalidFrame)
        );
    }

    #[tokio::test]
    async fn full_supervisor_admission_distinguishes_privacy_reductions() {
        let (commands, mut queue) = mpsc::channel(1);
        let (priority, mut priority_rx) = mpsc::channel(1);
        let (reply, _receive) = oneshot::channel();
        commands
            .try_send(Command {
                generation: 7,
                talk_epoch: 0,
                message: Msg::ProbeBackend(w::ProbeBackend {}),
                admitted_at: Instant::now(),
                reply,
            })
            .unwrap_or_else(|_| panic!("fill admission"));
        let handle = AudioHandle {
            commands,
            priority,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(7)),
            shutdown: watch::channel(false).0,
            stopped: watch::channel(true).1,
        };
        assert_eq!(
            handle.set_media_watch(false).await,
            Err(AudioSupervisorError::PrivacyControlRejected)
        );
        for message in [
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: 1,
                muted: true,
            }),
            Msg::PttReleaseAll(w::PttReleaseAll {}),
        ] {
            let (result, ()) = tokio::join!(handle.written(message), async {
                let command = priority_rx.recv().await.unwrap();
                assert_eq!(queue.len(), 1);
                assert!(command.reply.send(Ok(Reply::Written(42))).is_ok());
            });
            assert_eq!(result, Ok(42));
        }
        assert_eq!(
            handle.ptt_revoke(1).await,
            Err(AudioSupervisorError::PrivacyControlRejected)
        );
        assert_eq!(
            handle.release_mic_mute().await,
            Err(AudioSupervisorError::PrivacyControlRejected)
        );
        assert_eq!(
            handle.stop_session([1; 16]).await,
            Err(AudioSupervisorError::PrivacyControlRejected)
        );
        assert_eq!(
            handle.set_media_watch(true).await,
            Err(AudioSupervisorError::QueueFull)
        );
        assert_eq!(
            handle.set_gain([1; 16], 50, false).await,
            Err(AudioSupervisorError::QueueFull)
        );
        assert!(queue.try_recv().is_ok());
        assert!(queue.try_recv().is_err());
    }

    #[tokio::test]
    async fn talk_fences_bypass_full_queue_and_only_invalidate_transient_grants() {
        let (commands, mut queued) = mpsc::channel(2);
        let (priority, mut safety) = mpsc::channel(1);
        let handle = AudioHandle {
            commands,
            priority,
            talk_epoch: Arc::new(AtomicU64::new(0)),
            generation: Arc::new(AtomicU64::new(7)),
            shutdown: watch::channel(false).0,
            stopped: watch::channel(true).1,
        };
        for message in [
            Msg::PttGrant(w::PttGrant {
                grant_id: 7,
                ttl_ms: 250,
            }),
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: 1,
                muted: false,
            }),
        ] {
            let (reply, _response) = oneshot::channel();
            handle
                .commands
                .try_send(Command {
                    generation: 7,
                    talk_epoch: 0,
                    message,
                    admitted_at: Instant::now(),
                    reply,
                })
                .unwrap_or_else(|_| panic!("fill normal queue"));
        }
        for message in [
            Msg::PttReleaseAll(w::PttReleaseAll {}),
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: 1,
                muted: true,
            }),
        ] {
            let previous = handle.talk_epoch.load(Ordering::Acquire);
            let (result, ()) = tokio::join!(handle.written(message), async {
                let command = safety.recv().await.unwrap();
                assert_eq!(queued.len(), 2);
                assert_eq!(handle.talk_epoch.load(Ordering::Acquire), previous + 1);
                assert!(command.reply.send(Ok(Reply::Written(42))).is_ok());
            });
            assert_eq!(result, Ok(42));
        }
        let grant = queued.recv().await.unwrap();
        assert!(stale_talk(&grant, &handle.talk_epoch));
        let persistent = queued.recv().await.unwrap();
        assert!(!stale_talk(&persistent, &handle.talk_epoch));
        assert!(matches!(
            persistent.message,
            Msg::SetMicMute(w::SetMicMute { muted: false, .. })
        ));
        let (reply, _response) = oneshot::channel();
        handle
            .priority
            .try_send(Command {
                generation: 7,
                talk_epoch: 2,
                message: Msg::PttReleaseAll(w::PttReleaseAll {}),
                admitted_at: Instant::now(),
                reply,
            })
            .unwrap_or_else(|_| panic!("fill priority queue"));
        for message in [
            Msg::PttReleaseAll(w::PttReleaseAll {}),
            Msg::SetMicMute(w::SetMicMute {
                expected_generation: 1,
                muted: true,
            }),
        ] {
            assert!(matches!(
                handle.written(message).await,
                Err(AudioSupervisorError::PrivacyControlRejected)
            ));
            assert_eq!(handle.talk_epoch.load(Ordering::Acquire), 2);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_child_release_all_acknowledges_owned_dry_talk_without_physical_edge() {
        let root = Root::new();
        let (handle, mut events) = AudioSupervisor::start(root.config("release-all"))
            .await
            .unwrap();
        let (generation, _) = ready(&mut events).await;
        handle.probe().await.unwrap();
        let acquire = handle
            .acquire_mic_mute(w::DeviceSelection {
                selector: Some(w::device_selection::Selector::FollowDefault(2)),
            })
            .await
            .unwrap();
        let endpoint_generation = loop {
            match next(&mut events).await {
                AudioSupervisorEvent::MuteSnapshot {
                    request_id,
                    snapshot,
                    ..
                } if request_id == acquire => {
                    assert!(snapshot.guard_active);
                    break snapshot.generation;
                }
                AudioSupervisorEvent::CommandRejected { error, .. } => {
                    panic!("acquire rejected: {error:?}")
                }
                _ => {}
            }
        };
        let muted = handle
            .set_mic_mute(endpoint_generation, true)
            .await
            .unwrap();
        loop {
            if let AudioSupervisorEvent::MuteSnapshot {
                request_id,
                snapshot,
                ..
            } = next(&mut events).await
            {
                if request_id == muted {
                    assert!(snapshot.muted);
                    break;
                }
            }
        }
        let grant = handle.ptt_grant(73, 250).await.unwrap();
        loop {
            if let AudioSupervisorEvent::PttEvent {
                request_id, event, ..
            } = next(&mut events).await
            {
                if request_id == grant {
                    assert_eq!(event.kind, w::PttEventKind::Pressed as i32);
                    break;
                }
            }
        }
        let release = handle.ptt_release_all().await.unwrap();
        let (mut acknowledged, mut remuted) = (false, false);
        while !acknowledged || !remuted {
            match next(&mut events).await {
                AudioSupervisorEvent::PttEvent {
                    generation: observed,
                    request_id,
                    event,
                } if request_id == release => {
                    assert_eq!(observed, generation);
                    assert_eq!(event.kind, w::PttEventKind::Released as i32);
                    assert_eq!(event.reason, w::PttReleaseReason::Revoked as i32);
                    assert_eq!(
                        (event.grant_id, event.observed_at_ns, event.expires_at_ns),
                        (0, 0, 0)
                    );
                    acknowledged = true;
                }
                AudioSupervisorEvent::MuteSnapshot { snapshot, .. } if snapshot.muted => {
                    assert!(snapshot.guard_active);
                    remuted = true;
                }
                AudioSupervisorEvent::CommandRejected { error, .. } => {
                    panic!("release rejected: {error:?}")
                }
                AudioSupervisorEvent::Failed { reason, .. } => panic!("release failed: {reason}"),
                _ => {}
            }
        }
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_child_recycles_session_budget_before_hard_exhaustion() {
        async fn session(events: &mut mpsc::Receiver<AudioSupervisorEvent>, id: &[u8], kind: i32) {
            loop {
                match next(events).await {
                    AudioSupervisorEvent::SessionEvent { event, .. }
                        if event.session_id == id && event.kind == kind =>
                    {
                        return
                    }
                    AudioSupervisorEvent::Failed { reason, .. } => {
                        panic!("budget fixture failed: {reason}")
                    }
                    AudioSupervisorEvent::CommandRejected { error, .. } => {
                        panic!("budget fixture rejected: {error:?}")
                    }
                    AudioSupervisorEvent::RestartRequired { .. } => {
                        panic!("recycled before soft boundary")
                    }
                    _ => {}
                }
            }
        }
        let root = Root::new();
        let (handle, mut events) = AudioSupervisor::start(root.config("budget")).await.unwrap();
        let (generation, _) = ready(&mut events).await;
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let local = port();
        let request = |index: u16| {
            let mut request = start(2, local, peer.local_addr().unwrap().port());
            request.session_id[..2].copy_from_slice(&index.to_be_bytes());
            for (direction, material) in [
                (1, request.forward.as_mut().unwrap()),
                (2, request.reverse.as_mut().unwrap()),
            ] {
                material.key.fill(0);
                material.key[0] = direction;
                material.key[1..3].copy_from_slice(&index.to_be_bytes());
            }
            request
        };
        timeout(Duration::from_secs(60), async {
            for index in 0..240 {
                let request = request(index);
                let id = request.session_id.clone();
                handle.start_session(request).await.unwrap();
                session(&mut events, &id, 1).await;
                handle
                    .stop_session(id.as_slice().try_into().unwrap())
                    .await
                    .unwrap();
                session(&mut events, &id, 5).await;
                if index % 32 == 31 {
                    // Let a post-admission Pong retire no-reply safety requests;
                    // this fixture tests the child's lifetime budget, not IPC flooding.
                    tokio::time::sleep(Duration::from_millis(contract::PING_INTERVAL_MS + 10))
                        .await;
                }
            }
            handle.start_session(request(240)).await.unwrap();
            loop {
                if let AudioSupervisorEvent::RestartRequired {
                    generation: actual,
                    reason,
                } = next(&mut events).await
                {
                    assert_eq!(actual, generation);
                    assert_eq!(reason, AudioRestartReason::SessionBudget);
                    break;
                }
            }
            let (fresh, _) = ready(&mut events).await;
            assert!(fresh > generation);
            let request = request(241);
            let id = request.session_id.clone();
            handle.start_session(request).await.unwrap();
            session(&mut events, &id, 1).await;
            handle
                .stop_session(id.as_slice().try_into().unwrap())
                .await
                .unwrap();
            session(&mut events, &id, 5).await;
        })
        .await
        .expect("bounded child recycle at session soft budget");
        handle.shutdown().await.unwrap();
    }
}
