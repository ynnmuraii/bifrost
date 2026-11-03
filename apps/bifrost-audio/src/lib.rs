//! Isolated audio child. No identity/transport/core dependency, no implicit native probing.
//!
//! Explicit `BackendKind::Dry` is a deterministic harness, never a native fallback:
//! its current media token is 1, initial revision 1, title "Dry media", state Paused,
//! and Play/Pause/Toggle/Next/Previous are available. Commands execute on its control
//! pump, change playback state as appropriate and increment that token's revision.
//! Metadata is still exposed only under an explicitly enabled, authorized Source watch.
//!
//! Mic toggles persist without a TTL. Remote PTT grants alone are timed; shortcut
//! observation needs no mic guard and GUI-only talk grants need no native hotkey.
//! Clean release/shutdown restores via the backend guard contract (best-effort on
//! Linux); a fresh child never reacquires or unmutes an endpoint automatically.
//! Privacy reductions bypass the generic native-control queue. Watch withdrawal blanks
//! metadata at IPC admission; mute/revoke/release wake the independent guard timer.
//! Admission fences prevent older queued openings from reviving withdrawn authority.
//! Release-all cancels talk authority, not an independently explicit persistent unmute.
//! Its epoch fences old grants and native observations even with a full control queue.
//! A held native key stays fenced across expiry/reset/reconfiguration until a backend
//! proves physical key-up; only a subsequent fresh press may request talk again.
#![forbid(unsafe_code)]
mod clock;
mod lifecycle;
mod privacy;
mod ptt_lease;
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
    role: i32,
    authorized: Arc<AtomicBool>,
    local_sound: Arc<AtomicBool>,
    deadline: Instant,
    stop_reason: i32,
}
struct Shared {
    stopping: AtomicBool,
    media_enabled: AtomicBool,
    permits: Mutex<Vec<Permit>>,
    privacy: Arc<privacy::Privacy>,
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
            media_enabled: AtomicBool::new(false),
            permits: Mutex::new(Vec::with_capacity(contract::MAX_SESSIONS)),
            privacy: privacy::Privacy::new(),
        }
    }
    fn has_source(&self, now: Instant) -> bool {
        lock(&self.permits).iter().any(|permit| {
            permit.role == 1 && now < permit.deadline && permit.authorized.load(Ordering::Acquire)
        })
    }
    fn release_source_local_sound(&self, id: &[u8]) {
        for permit in lock(&self.permits)
            .iter()
            .filter(|permit| permit.role == 1 && permit.id == id)
        {
            permit.local_sound.store(false, Ordering::Release);
        }
    }
    fn source_local_sound(&self, id: &[u8]) -> bool {
        self.source_local_sound_permit(id)
            .is_some_and(|allowed| allowed.load(Ordering::Acquire))
    }
    fn source_local_sound_permit(&self, id: &[u8]) -> Option<Arc<AtomicBool>> {
        lock(&self.permits)
            .iter()
            .find(|permit| permit.role == 1 && permit.id == id)
            .map(|permit| permit.local_sound.clone())
    }
    fn stop_all(&self) {
        self.stopping.store(true, Ordering::Release);
        for permit in lock(&self.permits).iter() {
            permit.authorized.store(false, Ordering::Release);
        }
    }
    fn revoke(&self, id: &[u8]) {
        let now = Instant::now();
        for permit in lock(&self.permits)
            .iter_mut()
            .filter(|permit| permit.id == id)
        {
            if permit.authorized.load(Ordering::Acquire) && now >= permit.deadline {
                permit.stop_reason = 9;
            }
            permit.authorized.store(false, Ordering::Release);
        }
    }
    fn expire(&self, now: Instant) {
        for permit in lock(&self.permits).iter_mut() {
            if now >= permit.deadline && permit.authorized.load(Ordering::Acquire) {
                permit.stop_reason = 9;
                permit.authorized.store(false, Ordering::Release);
            }
        }
    }
    fn renew(&self, id: &[u8], ttl_ms: u32, now: Instant) -> bool {
        let mut permits = lock(&self.permits);
        let Some(permit) = permits.iter_mut().find(|permit| permit.id == id) else {
            return false;
        };
        if now >= permit.deadline {
            if permit.authorized.load(Ordering::Acquire) {
                permit.stop_reason = 9;
                permit.authorized.store(false, Ordering::Release);
            }
            return false;
        }
        if !(1..=1000).contains(&ttl_ms) || !permit.authorized.load(Ordering::Acquire) {
            return false;
        }
        permit.deadline = now + Duration::from_millis(u64::from(ttl_ms));
        true
    }
    fn stop_reason(&self, id: &[u8]) -> i32 {
        lock(&self.permits)
            .iter()
            .find(|permit| permit.id == id)
            .map_or(1, |permit| permit.stop_reason)
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
    admitted_at: Instant,
    privacy_epoch: u64,
    talk_epoch: u64,
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
        endpoint_mute: true,
        global_ptt: true,
        media: true,
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
    let token = audio_backend::MediaSessionToken(1);
    let mut media = audio_backend::MediaInfo::new(
        "bifrost-dry".into(),
        "Dry media".into(),
        "Harness only".into(),
        token,
        1,
        audio_backend::PlaybackState::Paused,
        audio_backend::MediaCommands {
            play: true,
            pause: true,
            toggle: true,
            next: true,
            previous: true,
            seek: true,
        },
    )?;
    media.position_ms = Some(10_000);
    media.duration_ms = Some(180_000);
    media.seek_min_ms = Some(0);
    media.seek_max_ms = Some(180_000);
    // Encoded image belongs only to the explicit Dry fixture, never a native fallback.
    media.artwork = audio_backend::artwork::normalize(
        &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2,
            8, 2, 0, 0, 0, 253, 212, 154, 115, 0, 0, 0, 22, 73, 68, 65, 84, 120, 156, 99, 80, 241,
            152, 224, 49, 65, 133, 97, 130, 138, 135, 202, 4, 15, 0, 27, 122, 3, 241, 244, 33, 109,
            130, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
        ],
        None,
    );
    backend.apply(ScriptEvent::SetMedia {
        session: token,
        info: Some(media),
    })?;
    backend.apply(ScriptEvent::SetCurrentMediaSession {
        session: Some(token),
    })?;
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
    #[cfg(test)]
    dry_factory: Option<fn() -> Result<DryBackend, BackendError>>,
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
    tracing::info!(
        backend_kind = kind,
        worker_generation = first.frame.worker_generation,
        "audio child attached"
    );
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
            #[cfg(test)]
            let factory = _options.dry_factory.unwrap_or(dry_backend);
            #[cfg(not(test))]
            let factory = dry_backend;
            serve(
                link,
                kind,
                factory,
                Some(pump),
                Some(DryBackend::pump_control),
            )
            .await
        }
        1 => {
            #[cfg(target_os = "linux")]
            {
                serve(
                    link,
                    kind,
                    platform_audio_linux::LinuxBackend::connect,
                    None,
                    None,
                )
                .await
            }
            #[cfg(windows)]
            {
                serve(
                    link,
                    kind,
                    platform_audio_win::WindowsBackend::new,
                    None,
                    None,
                )
                .await
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
    control_pump: Option<fn(&mut B)>,
) -> Result<ExitReason, RoleError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    B: Backend + 'static,
    B::Stream: 'static,
    B::Mute: 'static,
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
                control_pump,
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
            watch_shared.expire(Instant::now());
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
    let mut ptt_forwarder = ptt_lease::Forwarder::default();
    let mut media_revision = 0u64;
    let mut media_present = false;
    loop {
        tokio::select! {
            biased;
            _=check.tick()=>{
                if shared.stopping.load(Ordering::Acquire){break;}
                let now=Instant::now();
                if let Some(mut event)=ptt_forwarder.expire(now) {
                    shared.privacy.fence_native(clock::now_ns());
                    event.safety_epoch=shared.privacy.native_epoch.load(Ordering::Acquire);
                    ptt_forwarder.fence(shared.privacy.native_fence_ns.load(Ordering::Acquire),shared.privacy.revoked_through.load(Ordering::Acquire));
                    if send(&mut link,0,Msg::PttEvent(event)).await.is_err(){break;}
                    if send(&mut link,0,Msg::PttStatus(shared.privacy.fenced_status(clock::now_ns()))).await.is_err(){break;}
                }
                if !shared.has_source(now) {shared.media_enabled.store(false,Ordering::Release);}
                if media_present && !shared.media_enabled.load(Ordering::Acquire) {
                    let Some(revision)=media_revision.checked_add(1) else{failure=Some(RoleError::WorkerFailed);break;};
                    media_revision=revision;media_present=false;
                    if send(&mut link,0,Msg::MediaSessions(w::MediaSessions {
                        sessions:Vec::new(),current_token:None,revision,
                    })).await.is_err(){break;}
                }
            },
            received=link.recv()=>{
                let mut received=match received {Ok(received)=>received,Err(_)=>break};
                last.store(clock.elapsed().as_millis() as u64,Ordering::Release);
                let request_id=received.frame.request_id;let Some(message)=received.frame.msg.take() else{break;};
                match message {
                    Msg::Ping(ping)=>{if send(&mut link,0,Msg::Pong(w::Pong {serial:ping.serial})).await.is_err(){break;}},
                    Msg::Shutdown(_)=>{reason=ExitReason::Shutdown;break;},
                    Msg::ReleaseSourceLocalSound(release)=>{shared.release_source_local_sound(&release.session_id);},
                    Msg::RenewSession(renew)=>{
                        if !shared.renew(&renew.session_id,renew.ttl_ms,Instant::now())
                            && send(&mut link,request_id,Msg::Error(w::Error {code:6,session_id:renew.session_id})).await.is_err(){break;}
                    },
                    message=>{
                        if let Some(result)=shared.privacy.admit(request_id,&message) {
                            if matches!(&message,Msg::PttReleaseAll(_) | Msg::SetMicMute(w::SetMicMute {muted:true,..})) {
                                ptt_forwarder.fence(
                                    shared.privacy.native_fence_ns.load(Ordering::Acquire),
                                    shared.privacy.revoked_through.load(Ordering::Acquire),
                                );
                                if send(&mut link,0,Msg::PttStatus(shared.privacy.fenced_status(clock::now_ns()))).await.is_err(){break;}
                            }
                            if matches!(result,Ok(false))
                                && send(&mut link,request_id,Msg::PttEvent(w::PttEvent {grant_id:0,kind:3,reason:2,observed_at_ns:0,expires_at_ns:0,safety_epoch:shared.privacy.native_epoch.load(Ordering::Acquire)})).await.is_err(){break;}
                            if let Err(code)=result {
                                let response=if code==6 {match &message {
                                    Msg::PttRevoke(revoke)=>Msg::PttEvent(w::PttEvent {grant_id:revoke.grant_id,kind:5,reason:2,observed_at_ns:0,expires_at_ns:0,safety_epoch:0}),
                                    _=>Msg::Error(w::Error {code,session_id:Vec::new()}),
                                }} else {Msg::Error(w::Error {code,session_id:Vec::new()})};
                                if send(&mut link,request_id,response).await.is_err(){break;}
                            }
                            continue;
                        }
                        let mut permit=None;
                        if let Msg::StopSession(stop)=&message {shared.revoke(&stop.session_id);}
                        if let Msg::StartSession(start)=&message {
                            let id:[u8;16]=start.session_id.as_slice().try_into().expect("AudioLink validated session id");
                            let deadline=Instant::now()+Duration::from_millis(u64::from(start.initial_authorization_ms));
                            let admitted={let mut permits=lock(&shared.permits);if permits.len()>=contract::MAX_SESSIONS||permits.iter().any(|permit|permit.id==id){false}else{let authorized=Arc::new(AtomicBool::new(true));permits.push(Permit {id,role:start.role,authorized:authorized.clone(),local_sound:Arc::new(AtomicBool::new(!start.source_local_sound_released)),deadline,stop_reason:1});permit=Some(authorized);true}};
                            if !admitted {if send(&mut link,request_id,Msg::Error(w::Error {code:3,session_id:id.to_vec()})).await.is_err(){break;}continue;}
                        }
                        let id=if let Msg::StartSession(start)=&message{Some(start.session_id.clone())}else{None};
                        let media_watch=if let Msg::SetMediaWatch(watch)=&message {Some(watch.enabled)} else{None};
                        if media_watch == Some(false) {
                            shared.media_enabled.store(false,Ordering::Release);
                            let Some(revision)=media_revision.checked_add(1) else{failure=Some(RoleError::WorkerFailed);break;};
                            media_revision=revision;media_present=false;
                            if send(&mut link,0,Msg::MediaSessions(w::MediaSessions {sessions:Vec::new(),current_token:None,revision})).await.is_err(){break;}
                        }
                        if command_tx.try_send(Command {request_id,admitted_at:Instant::now(),privacy_epoch:shared.privacy.epoch.load(Ordering::Acquire),talk_epoch:shared.privacy.talk_epoch.load(Ordering::Acquire),message,secrets:received.secrets.take(),permit}).is_err(){
                            if let Some(id)=id {shared.revoke(&id);shared.remove(&id);}
                            if send(&mut link,request_id,Msg::Error(w::Error {code:3,session_id:Vec::new()})).await.is_err(){break;}
                        } else if let Some(enabled)=media_watch {
                            shared.media_enabled.store(enabled,Ordering::Release);
                        }
                    }
                }
            },
            response=response_rx.recv()=>{match response {
                Some(response)=>{
                    let message=match response.message {
                        Msg::PttEvent(event)=>{
                            if response.request_id==0 && event.grant_id==0 && event.safety_epoch!=shared.privacy.native_epoch.load(Ordering::Acquire){continue;}
                            let Some(event)=ptt_forwarder.accept(event,clock::now_ns(),Instant::now()) else{continue;};
                            Msg::PttEvent(event)
                        },
                        Msg::PttStatus(status)=>{
                            if status.safety_epoch!=shared.privacy.native_epoch.load(Ordering::Acquire){continue;}
                            Msg::PttStatus(status)
                        },
                        Msg::MediaSessions(mut snapshot)=>{
                            if !shared.media_enabled.load(Ordering::Acquire) || !shared.has_source(Instant::now()) {
                                snapshot.sessions.clear();snapshot.current_token=None;
                            }
                            let Some(revision)=media_revision.checked_add(1) else{failure=Some(RoleError::WorkerFailed);break;};
                            media_revision = revision;
                            snapshot.revision = revision;
                            media_present = !snapshot.sessions.is_empty();
                            Msg::MediaSessions(snapshot)
                        },
                        message=>message,
                    };
                    if send(&mut link,response.request_id,message).await.is_err(){break;}
                },
                None=>{if !shared.stopping.load(Ordering::Acquire){reason=ExitReason::WorkerFailed;}break;}
            }},
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
    lifecycle::message(&message);
    tokio::time::timeout(WRITE_TIMEOUT, link.send(request_id, message))
        .await
        .map_err(|_| RoleError::WorkerFailed)??;
    Ok(())
}

struct Args {
    endpoint: String,
    token_file: PathBuf,
    logs_dir: Option<PathBuf>,
    worker_generation: u64,
}
fn parse(args: Vec<String>) -> Result<Args, ()> {
    let mut args = args.into_iter();
    let (mut endpoint, mut token_file) = (None, None);
    let (mut logs_dir, mut worker_generation) = (None, None);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--ipc-endpoint" if endpoint.is_none() => endpoint = Some(args.next().ok_or(())?),
            "--token-file" if token_file.is_none() => {
                token_file = Some(PathBuf::from(args.next().ok_or(())?))
            }
            "--logs-dir" if logs_dir.is_none() => {
                logs_dir = Some(PathBuf::from(args.next().ok_or(())?));
            }
            "--worker-generation" if worker_generation.is_none() => {
                let generation = args.next().ok_or(())?.parse::<u64>().map_err(|_| ())?;
                if generation == 0 {
                    return Err(());
                }
                worker_generation = Some(generation);
            }
            _ => return Err(()),
        }
    }
    Ok(Args {
        endpoint: endpoint.ok_or(())?,
        token_file: token_file.ok_or(())?,
        logs_dir,
        worker_generation: worker_generation.unwrap_or(1),
    })
}
/// Internal role dispatch; token *bytes* never appear in arguments/environment/logs.
///
/// `--logs-dir` follows the input agent's launch contract. Only scalar lifecycle
/// observations enter `bifrost-audio.log`; media metadata, keys and PCM never do.
pub fn run_audio(args: Vec<String>) -> ExitCode {
    let Ok(args) = parse(args) else {
        eprintln!("usage: bifrost audio --ipc-endpoint <local endpoint> --token-file <private one-use file> [--logs-dir <path>] [--worker-generation <nonzero integer>]");
        return ExitCode::from(2);
    };
    lifecycle::init(args.logs_dir.as_deref(), args.worker_generation);
    let token = match contract::take_launch_token(&args.token_file) {
        Ok(token) => token,
        Err(_) => {
            tracing::error!("audio child launch token unavailable");
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
        Err(_) => {
            tracing::error!("audio child runtime initialization failed");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let io = ipc::transport::connect(&endpoint).await?;
        run_connection(io, token).await
    });
    match result {
        Ok(reason) => {
            tracing::info!(exit_reason = ?reason, "audio child stopped");
            if reason == ExitReason::Shutdown {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            // RoleError's Display is a fixed category, never its embedded IPC/native data.
            tracing::error!(reason = %error, "audio child failed");
            eprintln!("bifrost audio: stopped or lost core");
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests;
