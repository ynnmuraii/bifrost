//! Isolated audio child. No identity/transport/core dependency, no implicit native probing.
#![forbid(unsafe_code)]
mod worker;
use audio_backend::{
    dry::{DryBackend, DryStream, ScriptEvent},
    *,
};
use ipc::audio_worker::{self as contract, AudioLink, LaunchToken};
use protocol::audio_worker::{self as w, audio_frame::Msg};
use std::{
    fmt,
    path::PathBuf,
    process::ExitCode,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, watch},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const COMMAND_CAPACITY: usize = 16;
const RESPONSE_CAPACITY: usize = 32;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown,
    CoreLost,
    WorkerFailed,
}
#[derive(Debug)]
pub enum RoleError {
    Ipc(ipc::IpcError),
    WorkerFailed,
}
impl fmt::Display for RoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ipc(_) => "audio child IPC failed",
            Self::WorkerFailed => "audio child worker failed",
        })
    }
}
impl std::error::Error for RoleError {}
impl From<ipc::IpcError> for RoleError {
    fn from(value: ipc::IpcError) -> Self {
        Self::Ipc(value)
    }
}
struct Permit {
    id: [u8; 16],
    authorized: Arc<AtomicBool>,
}
struct Shared {
    stopping: AtomicBool,
    permits: Mutex<Vec<Permit>>,
}
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
impl Shared {
    fn new() -> Self {
        Self {
            stopping: AtomicBool::new(false),
            permits: Mutex::new(Vec::with_capacity(contract::MAX_SESSIONS)),
        }
    }
    fn stop_all(&self) {
        self.stopping.store(true, Ordering::Release);
        for permit in lock(&self.permits).iter() {
            permit.authorized.store(false, Ordering::Release);
        }
    }
    fn revoke(&self, id: &[u8]) {
        for permit in lock(&self.permits).iter().filter(|permit| permit.id == id) {
            permit.authorized.store(false, Ordering::Release);
        }
    }
    fn remove(&self, id: &[u8]) {
        lock(&self.permits).retain(|permit| permit.id != id);
    }
}
// Revocation also runs if an embedding harness cancels the serving future.
struct AuthorityGuard(Arc<Shared>);
impl Drop for AuthorityGuard {
    fn drop(&mut self) {
        self.0.stop_all();
    }
}
struct Command {
    request_id: u64,
    message: Msg,
    secrets: Option<contract::SessionSecrets>,
    permit: Option<Arc<AtomicBool>>,
}
struct Response {
    request_id: u64,
    message: Msg,
}
fn unavailable(backend: i32, probed: bool, reason: i32) -> w::Ready {
    w::Ready {
        backend,
        probed,
        capabilities: (1..=6)
            .map(|capability| w::CapabilityAvailability {
                capability,
                availability: 3,
                reason,
            })
            .collect(),
    }
}
fn dry_backend() -> Result<DryBackend, BackendError> {
    let mut backend = DryBackend::new(Capabilities {
        system_capture: true,
        exclude_own_process_tree: true,
        ..Capabilities::default()
    });
    let output = DeviceId::new("DRY_OUTPUT")?;
    let capture = DeviceId::new("DRY_CAPTURE")?;
    for (id, direction) in [
        (output.clone(), Direction::Render),
        (capture.clone(), Direction::Capture),
    ] {
        backend.apply(ScriptEvent::UpsertDevice(Device {
            id,
            label: "Dry audio".into(),
            direction,
            format: AudioFormat {
                sample_rate: 48_000,
                channels: 2,
            },
        }))?;
    }
    let defaults = DefaultEndpoints {
        render: Some(output),
        capture: Some(capture),
    };
    backend.apply(ScriptEvent::SetDefaults(Defaults {
        multimedia: defaults.clone(),
        communications: defaults,
    }))?;
    while backend.poll_event().is_some() {}
    Ok(backend)
}

/// Serves an already same-user-authenticated local endpoint. The caller supplies the opaque
/// token from the one-use file. Tests cross this same interface with real local endpoints.
pub async fn run_connection<S>(io: S, token: LaunchToken) -> Result<ExitReason, RoleError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    run_connection_options(io, token, Options::default()).await
}
#[derive(Default)]
struct Options {
    #[cfg(test)]
    observed: Option<Arc<tests::Observed>>,
}
async fn run_connection_options<S>(
    io: S,
    token: LaunchToken,
    _options: Options,
) -> Result<ExitReason, RoleError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut link = AudioLink::worker(io);
    let mut first = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.recv())
        .await
        .map_err(|_| RoleError::WorkerFailed)??;
    let presented = first.token.take().ok_or(RoleError::WorkerFailed)?;
    ipc::agent::verify_launch_token(token.expose_secret(), presented.expose_secret())?;
    drop(token);
    drop(presented);
    let request_id = first.frame.request_id;
    let Some(Msg::Attach(attach)) = first.frame.msg.take() else {
        return Err(RoleError::WorkerFailed);
    };
    let requested = attach.backend;
    drop(attach);
    let kind = if requested == 2 {
        2
    } else if cfg!(target_os = "linux") {
        3
    } else {
        4
    };
    tokio::time::timeout(
        WRITE_TIMEOUT,
        link.send(request_id, Msg::Ready(unavailable(kind, false, 1))),
    )
    .await
    .map_err(|_| RoleError::WorkerFailed)??;
    match requested {
        2 => {
            let pump: worker::Pump<DryStream> = Box::new(move |stream| {
                stream.pump(480)?;
                #[cfg(test)]
                if let Some(observed) = &_options.observed {
                    observed.observe(stream);
                }
                Ok(())
            });
            serve(link, kind, dry_backend, Some(pump)).await
        }
        1 => {
            #[cfg(target_os = "linux")]
            {
                serve(
                    link,
                    kind,
                    platform_audio_linux::LinuxBackend::connect,
                    None,
                )
                .await
            }
            #[cfg(windows)]
            {
                serve(link, kind, platform_audio_win::WindowsBackend::new, None).await
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                Err(RoleError::WorkerFailed)
            }
        }
        _ => Err(RoleError::WorkerFailed),
    }
}
async fn serve<S, B>(
    mut link: AudioLink<S>,
    kind: i32,
    factory: fn() -> Result<B, BackendError>,
    pump: Option<worker::Pump<B::Stream>>,
) -> Result<ExitReason, RoleError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    B: Backend + 'static,
    B::Stream: 'static,
{
    let shared = Arc::new(Shared::new());
    let _authority = AuthorityGuard(shared.clone());
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(COMMAND_CAPACITY);
    let (response_tx, mut response_rx) = mpsc::channel(RESPONSE_CAPACITY);
    let (health_tx, health_rx) = watch::channel(w::Health::default());
    let worker_shared = shared.clone();
    let handle = thread::Builder::new()
        .name("bfa-control".into())
        .spawn(move || {
            worker::run(
                worker_shared,
                command_rx,
                response_tx,
                health_tx,
                kind,
                factory,
                pump,
            )
        })
        .map_err(|_| RoleError::WorkerFailed)?;
    let clock = Instant::now();
    let last = Arc::new(AtomicU64::new(0));
    let watch_last = last.clone();
    let watch_shared = shared.clone();
    let watchdog = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            if watch_shared.stopping.load(Ordering::Acquire) {
                return;
            }
            if clock.elapsed().as_millis() as u64
                >= watch_last.load(Ordering::Acquire) + u64::from(contract::CORE_SILENCE_MS)
            {
                watch_shared.stop_all();
                return;
            }
        }
    });
    let mut status_tick = tokio::time::interval(Duration::from_secs(1));
    status_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut check = tokio::time::interval(Duration::from_millis(10));
    let mut reason = ExitReason::CoreLost;
    let mut failure = None;
    loop {
        tokio::select! {
            biased;
            _=check.tick()=>{if shared.stopping.load(Ordering::Acquire){break;}},
            received=link.recv()=>{
                let mut received=match received {Ok(received)=>received,Err(_)=>break};
                last.store(clock.elapsed().as_millis() as u64,Ordering::Release);
                let request_id=received.frame.request_id;let Some(message)=received.frame.msg.take() else{break;};
                match message {
                    Msg::Ping(ping)=>{if send(&mut link,0,Msg::Pong(w::Pong {serial:ping.serial})).await.is_err(){break;}},
                    Msg::Shutdown(_)=>{reason=ExitReason::Shutdown;break;},
                    message=>{
                        let mut permit=None;
                        if let Msg::StopSession(stop)=&message {shared.revoke(&stop.session_id);}
                        if let Msg::StartSession(start)=&message {
                            let id:[u8;16]=start.session_id.as_slice().try_into().expect("AudioLink validated session id");
                            let admitted={let mut permits=lock(&shared.permits);if permits.len()>=contract::MAX_SESSIONS||permits.iter().any(|permit|permit.id==id){false}else{let authorized=Arc::new(AtomicBool::new(true));permits.push(Permit {id,authorized:authorized.clone()});permit=Some(authorized);true}};
                            if !admitted {if send(&mut link,request_id,Msg::Error(w::Error {code:3,session_id:id.to_vec()})).await.is_err(){break;}continue;}
                        }
                        let id=if let Msg::StartSession(start)=&message{Some(start.session_id.clone())}else{None};
                        if command_tx.try_send(Command {request_id,message,secrets:received.secrets.take(),permit}).is_err(){
                            if let Some(id)=id {shared.revoke(&id);shared.remove(&id);}
                            if send(&mut link,request_id,Msg::Error(w::Error {code:3,session_id:Vec::new()})).await.is_err(){break;}
                        }
                    }
                }
            },
            response=response_rx.recv()=>{match response {Some(response)=>{if send(&mut link,response.request_id,response.message).await.is_err(){break;}},None=>{if !shared.stopping.load(Ordering::Acquire){reason=ExitReason::WorkerFailed;}break;}}},
            _=status_tick.tick()=>{let health=health_rx.borrow().clone();if send(&mut link,0,Msg::Health(health)).await.is_err(){break;}},
        }
    }
    shared.stop_all();
    drop(command_tx);
    watchdog.abort();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    if handle.is_finished() {
        if handle.join().is_err() {
            failure = Some(RoleError::WorkerFailed);
        }
    } else {
        failure = Some(RoleError::WorkerFailed);
    }
    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(reason)
    }
}
async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    link: &mut AudioLink<S>,
    request_id: u64,
    message: Msg,
) -> Result<(), RoleError> {
    tokio::time::timeout(WRITE_TIMEOUT, link.send(request_id, message))
        .await
        .map_err(|_| RoleError::WorkerFailed)??;
    Ok(())
}

struct Args {
    endpoint: String,
    token_file: PathBuf,
}
fn parse(args: Vec<String>) -> Result<Args, ()> {
    let mut args = args.into_iter();
    let (mut endpoint, mut token_file) = (None, None);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--ipc-endpoint" if endpoint.is_none() => endpoint = Some(args.next().ok_or(())?),
            "--token-file" if token_file.is_none() => {
                token_file = Some(PathBuf::from(args.next().ok_or(())?))
            }
            _ => return Err(()),
        }
    }
    Ok(Args {
        endpoint: endpoint.ok_or(())?,
        token_file: token_file.ok_or(())?,
    })
}
/// Internal role dispatch; token *bytes* never appear in arguments/environment/logs.
pub fn run_audio(args: Vec<String>) -> ExitCode {
    let Ok(args) = parse(args) else {
        eprintln!("usage: bifrost audio --ipc-endpoint <local endpoint> --token-file <private one-use file>");
        return ExitCode::from(2);
    };
    let token = match contract::take_launch_token(&args.token_file) {
        Ok(token) => token,
        Err(_) => {
            eprintln!("bifrost audio: launch token unavailable");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(unix)]
    let endpoint = ipc::transport::Endpoint::socket(args.endpoint);
    #[cfg(windows)]
    let endpoint = ipc::transport::Endpoint::pipe(args.endpoint);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return ExitCode::FAILURE,
    };
    let result = runtime.block_on(async {
        let io = ipc::transport::connect(&endpoint).await?;
        run_connection(io, token).await
    });
    match result {
        Ok(ExitReason::Shutdown) => ExitCode::SUCCESS,
        Ok(_) | Err(_) => {
            eprintln!("bifrost audio: stopped or lost core");
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests;
