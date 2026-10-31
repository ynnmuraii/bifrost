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
use tokio::sync::{mpsc, oneshot};
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
    pub backend: w::BackendKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioSupervisorError {
    Unavailable,
    QueueFull,
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
    CommandRejected {
        generation: u64,
        request_id: u64,
        error: w::Error,
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
    generation: Arc<AtomicU64>,
}
enum Reply {
    Written(u64),
    Ready(w::Ready),
    Devices(Vec<w::Device>),
    Shutdown,
}
struct Command {
    generation: u64,
    message: Msg,
    reply: oneshot::Sender<Result<Reply>>,
}
impl AudioHandle {
    async fn command(&self, message: Msg) -> Result<Reply> {
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
        let (reply, rx) = oneshot::channel();
        self.commands
            .try_send(Command {
                generation,
                message,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AudioSupervisorError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => AudioSupervisorError::Unavailable,
            })?;
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
    pub async fn set_gain(&self, session_id: [u8; 16], percent: u32, muted: bool) -> Result<u64> {
        self.written(Msg::SetGain(w::SetGain {
            session_id: session_id.to_vec(),
            gain_percent: percent,
            muted,
        }))
        .await
    }
    pub async fn shutdown(&self) -> Result<()> {
        match self.command(Msg::Shutdown(w::Shutdown {})).await? {
            Reply::Shutdown => Ok(()),
            _ => Err(AudioSupervisorError::InvalidFrame),
        }
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
        let (events, event_rx) = mpsc::channel(QUEUE);
        let generation = Arc::new(AtomicU64::new(0));
        let (started, ready) = oneshot::channel();
        tokio::spawn(run(config, rx, events, generation.clone(), started));
        // Dropping this receiver on timeout also makes the actor reap a late child.
        timeout(LAUNCH_TIMEOUT, ready)
            .await
            .map_err(|_| AudioSupervisorError::Timeout)?
            .map_err(|_| AudioSupervisorError::Unavailable)??;
        Ok((
            AudioHandle {
                commands,
                generation,
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

async fn launch(config: &AudioSupervisorConfig, generation: u64) -> Result<Child> {
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
    let attached = timeout(LAUNCH_TIMEOUT, async {
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
    })
    .await
    .unwrap_or(Err(AudioSupervisorError::Timeout));
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

async fn serve(
    child: &mut Child,
    generation: u64,
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<AudioSupervisorEvent>,
    available: &AtomicU64,
    sessions: &mut BTreeSet<[u8; 16]>,
) -> Result<()> {
    let mut pending = BTreeMap::<u64, Pending>::new();
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
                        Msg::Pong(value) if value.serial > pong && value.serial <= ping => { pong = value.serial; }
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
                            if !sessions.contains(&session) { return Err(AudioSupervisorError::InvalidFrame); }
                            if event.kind == 5 { sessions.remove(&session); }
                            emit(events, AudioSupervisorEvent::SessionEvent { generation, event })?;
                        }
                        Msg::Health(mut health) => {
                            // Health is a coalesced snapshot and may predate the last Stop event.
                            health.sessions.retain(|session| sessions.iter().any(|id| id.as_slice() == session.session_id));
                            emit(events, AudioSupervisorEvent::Health { generation, health })?;
                        }
                        Msg::BackendEvent(event) => emit(events, AudioSupervisorEvent::BackendEvent { generation, event })?,
                        Msg::Error(error) => {
                            if id <= 1 || id > request { return Err(AudioSupervisorError::InvalidFrame); }
                            if let Some(response) = pending.remove(&id) { let _ = response.reply.send(Err(AudioSupervisorError::WorkerRejected(error.code))); }
                            if let Ok(session) = <[u8;16]>::try_from(error.session_id.as_slice()) { sessions.remove(&session); }
                            emit(events, AudioSupervisorEvent::CommandRejected { generation, request_id: id, error })?;
                        }
                        _ => return Err(AudioSupervisorError::InvalidFrame),
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    if command.reply.is_closed() { continue; }
                    if command.generation != generation {
                        let _ = command.reply.send(Err(AudioSupervisorError::Unavailable)); continue;
                    }
                    // The child holds one paged inventory snapshot at a time.
                    if pending.len() >= QUEUE || (matches!(&command.message, Msg::ListDevices(_)) && pending.values().any(|response| response.inventory.is_some())) {
                        let _ = command.reply.send(Err(AudioSupervisorError::QueueFull)); continue;
                    }
                    let next_request = request.checked_add(1).ok_or(AudioSupervisorError::InvalidFrame)?;
                    // Handle validation bounds the queue; AudioLink validates the actual wire frame.
                    let message = command.message;
                    let shutdown = matches!(message, Msg::Shutdown(_));
                    let inventory = match &message { Msg::ListDevices(query) => Some(Inventory { direction: query.direction, page: 0, revision: 0, devices: Vec::new() }), _ => None };
                    let response = matches!(message, Msg::ProbeBackend(_) | Msg::ListDevices(_));
                    let session = match &message { Msg::StartSession(start) => Some(start.session_id.as_slice().try_into().map_err(|_| AudioSupervisorError::InvalidFrame)?), _ => None };
                    if let Some(id) = session {
                        if sessions.len() >= contract::MAX_SESSIONS || sessions.contains(&id) {
                            let _ = command.reply.send(Err(AudioSupervisorError::WorkerRejected(3))); continue;
                        }
                    }
                    request = next_request;
                    if let Err(error) = send(child, request, message).await { let _ = command.reply.send(Err(error.clone())); return Err(error); }
                    if let Some(id) = session { sessions.insert(id); }
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

async fn run(
    config: AudioSupervisorConfig,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<AudioSupervisorEvent>,
    available: Arc<AtomicU64>,
    started: oneshot::Sender<Result<()>>,
) {
    let mut first = Some(started);
    let mut generation = 1u64;
    let mut restarts = VecDeque::<Instant>::new();
    loop {
        let mut child = match launch(&config, generation).await {
            Ok(child) => child,
            Err(error) => {
                if let Some(first) = first.take() {
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
                    return;
                }
                restarts.push_back(Instant::now());
                let Some(next_generation) = generation.checked_add(1) else {
                    return;
                };
                generation = next_generation;
                if !backoff(&mut commands, &events, restarts.len()).await {
                    return;
                }
                continue;
            }
        };
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
        let outcome = serve(
            &mut child,
            generation,
            &mut commands,
            &events,
            &available,
            &mut sessions,
        )
        .await;
        available.store(0, Ordering::Release);
        child.reap().await;
        while let Ok(command) = commands.try_recv() {
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
            return;
        }
        restarts.push_back(Instant::now());
        let Some(next_generation) = generation.checked_add(1) else {
            return;
        };
        generation = next_generation;
        if !backoff(&mut commands, &events, restarts.len()).await {
            return;
        }
    }
}
async fn backoff(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<AudioSupervisorEvent>,
    attempt: usize,
) -> bool {
    let delay = tokio::time::sleep(Duration::from_secs(1 << (attempt - 1)));
    tokio::pin!(delay);
    loop {
        tokio::select! {
            _ = &mut delay => return true,
            _ = events.closed() => return false,
            command = commands.recv() => match command {
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
            generation: Arc::new(AtomicU64::new(1)),
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
    }

    #[tokio::test]
    async fn backoff_rejects_queued_previous_generation() {
        let (tx, mut rx) = mpsc::channel(1);
        let (events, _events_rx) = mpsc::channel(1);
        let (reply, response) = oneshot::channel();
        tx.send(Command {
            generation: 1,
            message: Msg::StartSession(start(1, 40001, 40002)),
            reply,
        })
        .await
        .unwrap_or_else(|_| panic!("command queue"));
        assert!(backoff(&mut rx, &events, 1).await);
        assert!(matches!(
            response.await.unwrap(),
            Err(AudioSupervisorError::Unavailable)
        ));
        assert!(rx.try_recv().is_err());
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
        timeout(Duration::from_secs(10), async {
            while !sending || !receiving {
                tokio::select! {
                    event = source_events.recv() => match event.expect("source events") {
                        AudioSupervisorEvent::Health { health, .. } => sending |= health.sessions.iter().any(|session| session.sending == Some(true) && session.capture_active == Some(true) && session.sent_media_packets > 0),
                        AudioSupervisorEvent::Failed { reason, .. } => panic!("source failed: {reason}"),
                        AudioSupervisorEvent::CommandRejected { error, .. } => panic!("source rejected: {error:?}"),
                        _ => {}
                    },
                    event = listener_events.recv() => match event.expect("listener events") {
                        AudioSupervisorEvent::Health { health, .. } => receiving |= health.sessions.iter().any(|session| session.counters.as_ref().is_some_and(|counters| counters.received_packets > 0)),
                        AudioSupervisorEvent::Failed { reason, .. } => panic!("listener failed: {reason}"),
                        AudioSupervisorEvent::CommandRejected { error, .. } => panic!("listener rejected: {error:?}"),
                        _ => {}
                    },
                }
            }
        }).await.expect("real Dry media sending and receiving");
        listener.set_gain([13; 16], 75, false).await.unwrap();
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
        listener.stop_session([13; 16]).await.unwrap();
        source.shutdown().await.unwrap();
        listener.shutdown().await.unwrap();
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
}
