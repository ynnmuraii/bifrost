//! Core's supervision of `lws-agent`: the per-launch channel, the launch token and
//! the `RECOVERY` restart.
//!
//! `docs/03-architecture.md` («Кто кого запускает»): «`lws-agent` — Только core
//! (`spawn`)», and «Core — единственный supervisor. Agent никогда не запускается
//! сам, поэтому не может быть «сиротой» с активным grab». Scenario S4 fixes what a
//! restart looks like: spawn in `RECOVERY`, send `ReleaseKeys` from core's
//! conservative ledger, wait for `Released`, then start a `NORMAL` agent.
//!
//! # The channel and the token
//!
//! Core creates the endpoint *before* spawning, so the agent can only ever connect
//! to a channel core already owns: a per-launch named pipe with the current user's
//! DACL, `FILE_FLAG_FIRST_PIPE_INSTANCE` and one instance on Windows (the documented
//! mechanism), and a per-launch socket in the 0700 runtime directory with a 0600
//! mode and a `SO_PEERCRED` check on Unix. The 32-byte launch token stays the second
//! check and never travels in `argv` or the environment: it goes into a file inside
//! a user-only directory, whose path is what the agent receives, and the agent
//! deletes it on read.
//!
//! # Deviation, stated
//!
//! The documents specify a `socketpair(AF_UNIX)` with an inherited descriptor for
//! the Unix agent channel. Handing a raw descriptor to a child needs `pre_exec` (an
//! `unsafe` block, which the apps must not contain), so the Unix channel is a
//! per-launch named socket instead; the security properties the documents care about
//! (nobody can pre-create or join it, only this user can connect) are the same,
//! because the socket lives in a 0700 directory with mode 0600 and every accepted
//! connection is checked with `SO_PEERCRED`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ::ipc::transport::{self, Endpoint};
use ::ipc::{AgentLink, IpcError, Watchdog};
use protocol::ipc::{self, agent_frame::Msg, AgentMode};

use crate::command::AgentState;
use crate::FRAME_MAJOR;

/// The stream an agent connection arrives on.
#[cfg(unix)]
pub type AgentStream = tokio::net::UnixStream;
/// See [`AgentStream`].
#[cfg(windows)]
pub type AgentStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// How long core waits for the agent to connect and answer the attach.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a `RECOVERY` release exchange may take before core gives up on it.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Core's heartbeat state for the agent channel (rule 11), with the startup grace the
/// documents imply.
///
/// # The clock and the grace
///
/// Rule 11 is a steady-state contract — «core шлёт агенту `Ping` каждые
/// `core_silence_ms / 4` … агент отвечает `Pong`» — and rule 6 scopes the release it
/// protects to `REMOTE`: «Core молчит в REMOTE дольше `core_silence_ms` … → agent
/// выполняет release и Local». A *starting* agent is neither: it holds no capture (the
/// backend is selected only after the attach, and capture needs a `BeginCapture`, which
/// can only follow `AgentReady`), and its spawn is already bounded by
/// [`LAUNCH_TIMEOUT`]. So the silence clock starts at the agent's **first frame** after a
/// launch — `AgentReady`, in practice, but any frame proves the process answers — and a
/// launch that produces no frame within the launch timeout is judged lost instead of
/// being waited for forever.
///
/// This is the CI-38220074904 failure on `windows-latest`: a loaded runner spends over
/// the 300 ms limit on spawn + connect + attach + the backend probe + `AgentReady`, and
/// the clock that ran from the *previous* launch's end declared every fresh agent lost
/// before it could answer — five launches in a row, the node `Blocked` most of the run.
/// With the grace a slow start costs its own launch timeout, not a restart loop.
///
/// The clock is this type's own: the supervisor outlives one session and marks every
/// launch, including the two starts a `RECOVERY` restart performs inside
/// [`Supervisor::restart_recovery`].
#[derive(Debug)]
pub struct AgentHeartbeat {
    /// Rule 11's periods: `Ping` every `core_silence_ms / 4`, silence limit `core_silence_ms`.
    watchdog: Watchdog,
    /// The epoch of this type's monotonic clock.
    started: Instant,
    /// When the current launch began, in `now_ms`; `None` before the first one.
    launched_at: Option<u64>,
    /// Whether a frame has arrived since [`AgentHeartbeat::launched`].
    live: bool,
    /// How long a launch may take to produce its first frame ([`LAUNCH_TIMEOUT`]).
    launch_timeout_ms: u64,
}

impl AgentHeartbeat {
    /// The heartbeat for the `core_silence_ms` carried in every `AgentAttach`.
    ///
    /// The value is validated by the config schema (200..=500); a value that still
    /// arrives out of range falls back to the default, exactly as the agent does with
    /// the same attach field.
    pub fn new(core_silence_ms: u32) -> Self {
        Self::with_launch_timeout(core_silence_ms, LAUNCH_TIMEOUT.as_millis() as u64)
    }

    /// The same, with the launch-timeout bound as a parameter: the grace is part of the
    /// policy, so it is testable without waiting five seconds.
    pub fn with_launch_timeout(core_silence_ms: u32, launch_timeout_ms: u64) -> Self {
        let watchdog = Watchdog::for_core_silence_ms(core_silence_ms, 0).unwrap_or_else(|error| {
            tracing::warn!(%error, "core_silence_ms outside 200..=500; using the default");
            Watchdog::for_core_silence_ms(::ipc::DEFAULT_CORE_SILENCE_MS, 0)
                .expect("the default core_silence_ms is in range")
        });
        AgentHeartbeat {
            watchdog,
            started: Instant::now(),
            launched_at: None,
            live: false,
            launch_timeout_ms,
        }
    }

    /// Milliseconds on this type's own monotonic clock.
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// A launch began: the silence clock is held until the first frame, and a launch
    /// that never produces one is judged after [`LAUNCH_TIMEOUT`].
    pub fn launched(&mut self) {
        let now = self.now_ms();
        self.launched_at = Some(now);
        self.live = false;
        // The channel is new: the heartbeat period restarts with the launch, so a slow
        // start does not also owe a ping for the time it took.
        self.watchdog = Watchdog::for_core_silence_ms(self.watchdog.silence_limit_ms() as u32, now)
            .expect("the limit was validated at construction");
    }

    /// Any frame from the agent: the process is alive, and the silence clock runs from
    /// here («нет ни одного кадра, включая IPC `Ping`»).
    pub fn on_frame(&mut self) {
        let now = self.now_ms();
        self.live = true;
        self.watchdog.on_frame(now);
    }

    /// The machine was suspended: re-arm the silence clock.
    ///
    /// `CLOCK_MONOTONIC` is frozen while the machine sleeps, but the resume can still be
    /// read as silence — the process is not serviced for a while afterwards — and nothing
    /// the agent did caused it. A live channel stays live and its silence clock runs from
    /// now; a launch still in flight gets its grace back. The heartbeat period is left
    /// alone: the monotonic clock resumes, so the next `Ping` is due on its own schedule
    /// (within `core_silence_ms / 4`).
    pub fn on_resume(&mut self) {
        let now = self.now_ms();
        if self.live {
            self.watchdog.on_frame(now);
        } else if self.launched_at.is_some() {
            self.launched_at = Some(now);
        }
    }

    /// Whether it is time to send the heartbeat `Ping` (rule 11).
    pub fn heartbeat_due(&self) -> bool {
        self.watchdog.heartbeat_due(self.now_ms())
    }

    /// Record that the heartbeat `Ping` was sent.
    pub fn on_heartbeat(&mut self) {
        let now = self.now_ms();
        self.watchdog.on_heartbeat(now);
    }

    /// Milliseconds until the next heartbeat is due (0 when it already is).
    pub fn next_heartbeat_in_ms(&self) -> u64 {
        self.watchdog.next_heartbeat_in_ms(self.now_ms())
    }

    /// The peer-silence limit, also used to distinguish core's own loop stalls.
    pub fn silence_limit_ms(&self) -> u64 {
        self.watchdog.silence_limit_ms()
    }

    /// Whether the agent must be considered lost.
    ///
    /// Before the first frame of a launch only the launch timeout counts: the agent is
    /// starting, and there is nothing to release. Afterwards rule 6's silence limit is
    /// the watchdog's own.
    pub fn silence_exceeded(&self) -> bool {
        match (self.launched_at, self.live) {
            (Some(launched), false) => {
                self.now_ms().saturating_sub(launched) >= self.launch_timeout_ms
            }
            _ => self.watchdog.silence_exceeded(self.now_ms()),
        }
    }

    /// How long the agent has not been heard from, for the loss log.
    ///
    /// During the grace this is the age of the launch, which is what the operator needs
    /// to see: the agent never became ready, as opposed to going quiet.
    pub fn silence_ms(&self) -> u64 {
        match (self.launched_at, self.live) {
            (Some(launched), false) => self.now_ms().saturating_sub(launched),
            _ => self.watchdog.silence_ms(self.now_ms()),
        }
    }

    /// Milliseconds until the next liveness decision is due, for the loop's deadline.
    pub fn next_silence_check_in_ms(&self) -> u64 {
        match (self.launched_at, self.live) {
            (Some(launched), false) => self
                .launch_timeout_ms
                .saturating_sub(self.now_ms().saturating_sub(launched)),
            _ => self.watchdog.remaining_ms(self.now_ms()),
        }
    }
}

/// The agent process and its channel.
/// The receiver-takeover settings the agent needs (KVM-035).
///
/// They travel in `AgentAttach` because the agent has no config of its own: core reads
/// `[input]` and hands over the decision, and the agent still checks its environment
/// (`takeover_detection = "off"` is honoured here as well, so a stale attach cannot arm a
/// detector).
#[derive(Debug, Clone, Copy)]
pub struct TakeoverSettings {
    /// `[input].takeover_detection`.
    pub detection: ipc::TakeoverDetection,
    /// `[input].takeover_threshold_px`.
    pub threshold_px: u32,
}

pub struct Supervisor {
    program: PathBuf,
    /// Directory for the per-launch token file (user-only).
    private_dir: PathBuf,
    /// Directory/namespace for the per-launch endpoint.
    runtime_dir: PathBuf,
    /// Where the agent's own rotating log file goes (`[log]`'s directory, `logs/`).
    logs_dir: PathBuf,
    dry_inject: bool,
    dry_input_events: usize,
    dry_displays: bool,
    dev_capture_injected: bool,
    /// With `--dry-inject`: let the agent's synthetic pointer cross the right edge.
    dry_motion: bool,
    emergency_hotkey: String,
    backend_override: ipc::BackendKind,
    require_emergency_hotkey: bool,
    core_silence_ms: u32,
    /// `[input].takeover_detection` / `.takeover_threshold_px`, sent in every `AgentAttach`.
    takeover: TakeoverSettings,
    /// `[input].repeat_mode`: which side repeats a held key (KVM-036).
    repeat_mode: ipc::RepeatMode,
    /// Portal restore tokens, persisted in `portal-restore.toml` and shipped in
    /// `AgentAttach.restore_tokens`; new ones arrive back as `PortalGrantChanged`.
    portal: crate::portal::PortalGrants,
    /// A portal failure disables new portal sessions across automatic agent restarts.
    allow_portal_sessions: bool,
    /// Rule 11's heartbeat, with the startup grace; the supervisor marks every launch
    /// and every received frame, so the state survives a session's watchdog questions.
    heartbeat: AgentHeartbeat,
    launches: u64,
    child: Option<tokio::process::Child>,
    /// The kill-on-close job the current child lives in (see [`crate::job`]); dropping it
    /// kills the agent, which is what core wants whenever it owns no agent any more.
    job: Option<crate::job::KillOnCoreExit>,
    link: Option<AgentLink<AgentStream>>,
    state: AgentState,
}

impl Supervisor {
    /// Builds the supervisor; nothing is spawned until [`Supervisor::start`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        program: PathBuf,
        private_dir: PathBuf,
        runtime_dir: PathBuf,
        logs_dir: PathBuf,
        dry_inject: bool,
        dry_input_events: usize,
        dry_motion: bool,
        dry_displays: bool,
        dev_capture_injected: bool,
        emergency_hotkey: String,
        backend_override: ipc::BackendKind,
        require_emergency_hotkey: bool,
        core_silence_ms: u32,
        portal: crate::portal::PortalGrants,
        takeover: TakeoverSettings,
        repeat_mode: ipc::RepeatMode,
    ) -> Self {
        Supervisor {
            program,
            private_dir,
            runtime_dir,
            logs_dir,
            dry_inject,
            dry_input_events,
            dry_motion,
            dry_displays,
            dev_capture_injected,
            emergency_hotkey,
            backend_override,
            require_emergency_hotkey,
            core_silence_ms,
            portal,
            takeover,
            repeat_mode,
            allow_portal_sessions: true,
            heartbeat: AgentHeartbeat::new(core_silence_ms),
            launches: 0,
            child: None,
            job: None,
            link: None,
            state: AgentState::Starting,
        }
    }

    /// The agent's state word for the status line.
    pub const fn state(&self) -> AgentState {
        self.state
    }

    /// The portal token store: the session writes back what the agent reported and
    /// clears it when the user forgets the grants.
    pub fn portal(&mut self) -> &mut crate::portal::PortalGrants {
        &mut self.portal
    }
    /// Prevents automatic agent restarts from re-opening failed portal sessions.
    pub fn set_portal_sessions_allowed(&mut self, allowed: bool) {
        self.allow_portal_sessions = allowed;
    }

    /// Spawns one agent, waits for it to connect and sends the `AgentAttach`.
    pub async fn start(&mut self, mode: AgentMode) -> Result<(), String> {
        self.stop().await;
        // The launch is where the silence clock resets (see [`AgentHeartbeat`]): the
        // frames of the *previous* channel prove nothing about this one.
        self.heartbeat.launched();
        self.launches += 1;
        let endpoint = launch_endpoint(&self.runtime_dir, self.launches)?;
        let mut listener = transport::listen(&endpoint).map_err(|error| error.to_string())?;
        let token = random_token()?;
        let token_path = token_file(&self.private_dir, self.launches);
        write_token_file(&token_path, &token)?;

        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg("--ipc-endpoint")
            .arg(endpoint_spec(&endpoint))
            .arg("--token-file")
            .arg(&token_path)
            // The agent writes `<logs>/lws-agent.log` itself; core only says where, because
            // the agent does not read `config.toml` (docs/03: one log directory per user).
            .arg("--logs-dir")
            .arg(&self.logs_dir)
            // The agent explains itself on stderr; stdout stays quiet.
            .stdout(std::process::Stdio::null());
        // A console window flashing once per agent launch is what a core started by the GUI
        // would otherwise show: the agent is a console program, and a parent with no console
        // gives its child a new one unless it says not to.
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        if self.dev_capture_injected {
            command.arg("--dev-capture-injected");
        }
        if self.dry_inject {
            command.arg("--dry-inject");
            if self.dry_input_events > 0 {
                command
                    .arg("--dry-input-events")
                    .arg(self.dry_input_events.to_string());
            }
            if self.dry_displays {
                command.arg("--dry-displays");
            }
            if self.dry_motion {
                command.arg("--dry-motion");
            }
            // The dry backend owns no real selection, so the clipboard suite drives it through
            // a directory of files: `in.txt` is what a test writes to fake a user's copy, and
            // `out.txt` is what the agent's clipboard currently holds. It lives beside the
            // launch token, in the directory core already owns.
            let clipboard_dir = self.private_dir.join("clipboard-dry");
            let _ = std::fs::create_dir_all(&clipboard_dir);
            command.arg("--dry-clipboard-dir").arg(&clipboard_dir);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("cannot spawn {}: {error}", self.program.display()))?;
        // The agent must not be able to outlive core (see `crate::job`): it is attached to a
        // job that the kernel kills when core's handle closes. Failing to attach is not fatal
        // — the agent's own watchdog stays the release path — but it is worth a warning.
        self.job = match crate::job::KillOnCoreExit::attach(&child) {
            Ok(job) => Some(job),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the agent is not in a kill-on-close job; its own watchdog is the only \
                     release path if core is killed"
                );
                None
            }
        };
        self.child = Some(child);
        self.state = match mode {
            AgentMode::Recovery => AgentState::Recovery,
            _ => AgentState::Starting,
        };

        let stream = match tokio::time::timeout(
            LAUNCH_TIMEOUT,
            listener.accept(&::ipc::peer::SameUser::current()),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                self.stop().await;
                return Err(format!("agent did not connect: {error}"));
            }
            Err(_) => {
                self.stop().await;
                return Err("agent did not connect within the launch timeout".to_owned());
            }
        };
        let mut link = AgentLink::new(stream, FRAME_MAJOR, protocol::limits::PROTOCOL_MINOR);
        let attach = crate::routing::attach(
            token.to_vec(),
            mode,
            &self.emergency_hotkey,
            self.backend_override,
            self.core_silence_ms,
            self.require_emergency_hotkey,
            self.portal.to_ipc(),
            self.takeover.detection,
            self.takeover.threshold_px,
            self.repeat_mode,
            self.allow_portal_sessions,
        );
        if let Err(error) = link.attach(attach).await {
            self.stop().await;
            return Err(format!("AgentAttach failed: {error}"));
        }
        tracing::info!(
            mode = mode.as_str_name(),
            program = self.program.display().to_string().as_str(),
            "agent started"
        );
        self.link = Some(link);
        Ok(())
    }

    /// Whether an agent is attached right now.
    pub const fn is_attached(&self) -> bool {
        self.link.is_some()
    }

    /// Sends one frame.
    pub async fn send(&mut self, message: Msg) -> Result<(), String> {
        match self.link.as_mut() {
            Some(link) => {
                let started = tracing::enabled!(tracing::Level::DEBUG).then(Instant::now);
                static LOG_RATE: logging::LogRateLimit = logging::LogRateLimit::new();
                let frame = crate::routing::agent_frame_name(&message);
                let result = link.send(message).await;
                let elapsed = started.map_or(Duration::ZERO, |start| start.elapsed());
                if tracing::enabled!(tracing::Level::DEBUG) && LOG_RATE.allow() {
                    tracing::debug!(frame, elapsed_ms = elapsed.as_millis(), "core -> agent");
                }
                result.map_err(|error| match error {
                    IpcError::Closed => "the agent channel is closed".to_owned(),
                    other => other.to_string(),
                })
            }
            None => Err("no agent is attached".to_owned()),
        }
    }

    /// Receives one frame, or reports why the channel ended.
    ///
    /// Every received frame is liveness (rule 6: «нет ни одного кадра, включая IPC
    /// `Ping`»), so the heartbeat hears about it here — including the frames a
    /// `RECOVERY` restart consumes through this same method.
    pub async fn recv(&mut self) -> Result<ipc::AgentFrame, String> {
        match self.link.as_mut() {
            Some(link) => {
                let frame = link.recv().await.map_err(|error| error.to_string())?;
                self.heartbeat.on_frame();
                Ok(frame)
            }
            None => Err("no agent is attached".to_owned()),
        }
    }
    /// Everything the dying agent already sent, before a restart drops this channel.
    ///
    /// Nothing is discarded (audit F7): the caller reports the portal grants as it always did,
    /// and hands every other frame to the session — a `Released` or a `DeadlineExpired` that
    /// is thrown away leaves the FSM waiting for a release that already happened, so the
    /// transition only finishes when the release guard expires.
    pub async fn drain_agent_tail(&mut self) -> Vec<ipc::AgentFrame> {
        let mut frames = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(1), self.recv()).await {
                Ok(Ok(frame)) => frames.push(frame),
                Ok(Err(_)) | Err(_) => return frames,
            }
        }
    }

    /// Whether the heartbeat `Ping` (rule 11) is due on the current channel.
    pub fn heartbeat_due(&self) -> bool {
        self.heartbeat.heartbeat_due()
    }

    /// Records that the heartbeat `Ping` was sent.
    pub fn on_heartbeat(&mut self) {
        self.heartbeat.on_heartbeat();
    }

    /// The machine was suspended: re-arm the heartbeat's silence clock, so the freeze is
    /// not read as the agent's silence (see [`AgentHeartbeat::on_resume`]).
    pub fn on_resume(&mut self) {
        self.heartbeat.on_resume();
    }

    /// The peer-silence limit configured for this channel.
    pub fn silence_limit_ms(&self) -> u64 {
        self.heartbeat.silence_limit_ms()
    }

    /// Whether the agent must be considered lost (the grace and rule 6's limit).
    pub fn silence_exceeded(&self) -> bool {
        self.heartbeat.silence_exceeded()
    }

    /// How long the agent has not been heard from, for the loss log.
    pub fn silence_ms(&self) -> u64 {
        self.heartbeat.silence_ms()
    }

    /// Milliseconds until the heartbeat `Ping` is due.
    pub fn next_heartbeat_in_ms(&self) -> u64 {
        self.heartbeat.next_heartbeat_in_ms()
    }

    /// Milliseconds until the next liveness decision, for the loop's deadline.
    pub fn next_silence_check_in_ms(&self) -> u64 {
        self.heartbeat.next_silence_check_in_ms()
    }

    /// Whether the child process has exited (the channel may not have noticed yet).
    pub fn child_exited(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(Some(_))),
            None => false,
        }
    }

    /// Kills the agent and drops the channel.
    pub async fn stop(&mut self) {
        self.link = None;
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(mut child) = self.child.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }
        // Dropping the job is the belt to `start_kill`'s braces: whatever the child did with
        // the kill, the kernel takes it out with the job.
        self.job = None;
        if matches!(self.state, AgentState::Ready | AgentState::Starting) {
            self.state = AgentState::Lost;
        }
    }

    /// Requests the agent's orderly shutdown so it releases capture before exiting.
    pub async fn shutdown(&mut self) {
        let requested = if let Some(link) = self.link.as_mut() {
            link.send(Msg::Shutdown(ipc::AgentShutdown {}))
                .await
                .is_ok()
        } else {
            false
        };
        if let Some(mut child) = self.child.take() {
            if !requested
                || tokio::time::timeout(Duration::from_secs(2), child.wait())
                    .await
                    .is_err()
            {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
        self.link = None;
        self.job = None;
        self.state = AgentState::Lost;
    }

    /// Marks the agent as attached and ready (after `AgentReady`).
    pub fn mark_ready(&mut self) {
        self.state = AgentState::Ready;
    }

    /// The `RECOVERY` restart of scenario S4: release the ledger, then a fresh
    /// `NORMAL` agent.
    ///
    /// Returns the number of keys and buttons the recovery agent reported letting go
    /// of, for the log.
    pub async fn restart_recovery(
        &mut self,
        keys: &[input_model::HidKey],
        buttons: &[input_model::MouseButton],
    ) -> Result<(u32, u32), String> {
        self.start(AgentMode::Recovery).await?;
        // The agent reports `AgentReady` right after the attach; then core sends the
        // ledger (`docs/03-architecture.md`, S4).
        let ready = tokio::time::timeout(RECOVERY_TIMEOUT, self.recv()).await;
        match ready {
            Ok(Ok(frame)) => {
                if !matches!(frame.msg, Some(Msg::Ready(_))) {
                    tracing::warn!("recovery agent did not report AgentReady first");
                }
            }
            Ok(Err(error)) => {
                self.stop().await;
                return Err(format!("recovery agent failed: {error}"));
            }
            Err(_) => {
                self.stop().await;
                return Err("recovery agent stayed silent".to_owned());
            }
        }
        self.send(crate::routing::release_keys(keys, buttons))
            .await?;
        let released = tokio::time::timeout(RECOVERY_TIMEOUT, self.recv()).await;
        let counts = match released {
            Ok(Ok(frame)) => match frame.msg {
                Some(Msg::Released(released)) => {
                    (released.keys_released, released.buttons_released)
                }
                _ => (0, 0),
            },
            Ok(Err(error)) => {
                self.stop().await;
                return Err(format!("recovery release failed: {error}"));
            }
            Err(_) => (0, 0),
        };
        tracing::info!(
            keys = counts.0,
            buttons = counts.1,
            "recovery release reported"
        );
        // A `RECOVERY` agent accepts nothing else (rule 7), so the node gets a fresh
        // `NORMAL` one rather than trying to promote it.
        self.start(AgentMode::Normal).await?;
        Ok(counts)
    }
}

/// The per-launch endpoint core owns before the agent exists.
///
/// Windows uses the documented `\\.\pipe\lan-workspace-agent-<pid>-<n>` name; Unix
/// uses a socket in the runtime directory (`agent-<pid>-<n>.sock`).
#[cfg(windows)]
fn launch_endpoint(_runtime_dir: &Path, launch: u64) -> Result<Endpoint, String> {
    Ok(Endpoint::pipe(format!(
        "{}{}agent-{}-{launch}",
        transport::PIPE_PATH_PREFIX,
        transport::PIPE_NAME_PREFIX,
        std::process::id(),
    )))
}

/// See the Windows variant.
#[cfg(unix)]
fn launch_endpoint(runtime_dir: &Path, launch: u64) -> Result<Endpoint, String> {
    let path = transport::gui_socket_path(runtime_dir)
        .with_file_name(format!("agent-{}-{launch}.sock", std::process::id()));
    Ok(Endpoint::socket(path))
}

/// What the agent is told on the command line: the endpoint, never a secret.
fn endpoint_spec(endpoint: &Endpoint) -> String {
    endpoint.describe()
}

/// 32 random bytes for the first `AgentAttach`.
fn random_token() -> Result<[u8; 32], String> {
    let first = identity::random16().map_err(|error| error.to_string())?;
    let second = identity::random16().map_err(|error| error.to_string())?;
    let mut token = [0u8; 32];
    token[..16].copy_from_slice(&first);
    token[16..].copy_from_slice(&second);
    Ok(token)
}

/// `launch-<n>.token` inside the private directory.
fn token_file(private_dir: &Path, launch: u64) -> PathBuf {
    private_dir.join(format!("launch-{launch}.token"))
}

/// Writes the token where only this user can read it and the agent deletes it.
fn write_token_file(path: &Path, token: &[u8]) -> Result<(), String> {
    std::fs::write(path, token).map_err(|error| format!("writing {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("setting {} to 0600: {error}", path.display()))?;
    }
    Ok(())
}

/// The default agent program: `lws-agent` next to `lws-core`.
pub fn default_agent_program() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = current
        .parent()
        .ok_or_else(|| format!("{} has no directory", current.display()))?;
    let name = if cfg!(windows) {
        "lws-agent.exe"
    } else {
        "lws-agent"
    };
    Ok(directory.join(name))
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lws-core-agent-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        dir
    }

    #[test]
    fn each_launch_gets_its_own_endpoint() {
        let runtime = temp_dir("runtime");
        let first = endpoint_spec(&launch_endpoint(&runtime, 1).expect("endpoint"));
        let second = endpoint_spec(&launch_endpoint(&runtime, 2).expect("endpoint"));
        assert_ne!(first, second, "two launches must not share a channel");

        #[cfg(windows)]
        {
            // `docs/03-architecture.md`: `\\.\pipe\lan-workspace-agent-<pid>-<n>`.
            assert!(first.contains("lan-workspace-agent-"), "{first}");
            assert!(first.ends_with("-1"), "{first}");
            assert!(second.ends_with("-2"), "{second}");
        }
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(first.ends_with(&format!("agent-{pid}-1.sock")), "{first}");
            assert!(second.ends_with(&format!("agent-{pid}-2.sock")), "{second}");
            // The socket lives in the 0700 runtime directory, next to the GUI socket.
            assert!(
                std::path::Path::new(&first).starts_with(&runtime),
                "{first} is not inside {}",
                runtime.display()
            );
        }
    }

    #[test]
    fn the_token_file_is_per_launch_and_written_owner_only() {
        let private = temp_dir("private");
        let path = token_file(&private, 7);
        assert_eq!(path, private.join("launch-7.token"));
        assert_ne!(path, token_file(&private, 8));

        let mut token = [0u8; 32];
        for (index, byte) in token.iter_mut().enumerate() {
            *byte = index as u8;
        }
        write_token_file(&path, &token).expect("write");
        assert_eq!(std::fs::read(&path).expect("read back"), token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the token file is owner-only");
        }

        let _ = std::fs::remove_dir_all(&private);
    }

    #[test]
    fn tokens_are_32_bytes_and_never_repeat() {
        let first = random_token().expect("entropy");
        let second = random_token().expect("entropy");
        assert_eq!(first.len(), 32);
        assert_eq!(second.len(), 32);
        assert_ne!(first, second);
    }

    #[test]
    fn the_default_agent_program_sits_next_to_core() {
        let program = default_agent_program().expect("path");
        let name = program
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        assert_eq!(
            name,
            if cfg!(windows) {
                "lws-agent.exe"
            } else {
                "lws-agent"
            }
        );
        assert!(program.parent().is_some());
    }

    #[test]
    fn dev_capture_flag_is_independent_of_dry_inject() {
        let command_args = |dev_capture_injected| {
            let supervisor = Supervisor::new(
                PathBuf::from("agent"),
                PathBuf::from("private"),
                PathBuf::from("runtime"),
                PathBuf::from("logs"),
                false,
                0,
                false,
                false,
                dev_capture_injected,
                "Ctrl+Alt+Shift+F12".to_owned(),
                ipc::BackendKind::Auto,
                true,
                300,
                crate::portal::PortalGrants::default(),
                TakeoverSettings {
                    detection: ipc::TakeoverDetection::Auto,
                    threshold_px: 12,
                },
                ipc::RepeatMode::ReceiverSoftware,
            );
            let mut command = tokio::process::Command::new(&supervisor.program);
            if supervisor.dev_capture_injected {
                command.arg("--dev-capture-injected");
            }
            if supervisor.dry_inject {
                command.arg("--dry-inject");
            }
            command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert!(command_args(true).contains(&"--dev-capture-injected".to_owned()));
        assert!(!command_args(false).contains(&"--dev-capture-injected".to_owned()));
        assert!(!command_args(true).contains(&"--dry-inject".to_owned()));
    }

    #[tokio::test]
    async fn an_agent_that_cannot_be_spawned_is_reported_as_an_error() {
        let runtime = temp_dir("runtime");
        let private = temp_dir("private");
        let mut supervisor = Supervisor::new(
            PathBuf::from("definitely-not-an-agent-program"),
            private.clone(),
            runtime.clone(),
            private.join("logs"),
            false,
            0,
            false,
            false,
            false,
            "Ctrl+Alt+Shift+F12".to_owned(),
            ipc::BackendKind::Auto,
            true,
            300,
            crate::portal::PortalGrants::default(),
            TakeoverSettings {
                detection: ipc::TakeoverDetection::Auto,
                threshold_px: 12,
            },
            ipc::RepeatMode::ReceiverSoftware,
        );
        assert_eq!(
            supervisor.state(),
            AgentState::Starting,
            "nothing spawned yet"
        );

        let error = supervisor
            .start(AgentMode::Normal)
            .await
            .expect_err("a missing program cannot be started");
        assert!(error.contains("cannot spawn"), "{error}");
        assert!(!supervisor.is_attached());

        let _ = std::fs::remove_dir_all(&runtime);
        let _ = std::fs::remove_dir_all(&private);
    }
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;

    /// The grace, in the shape the CI failure had: a launch whose first frame arrives
    /// after more than the whole silence limit must not be judged silent.
    #[test]
    fn the_silence_clock_does_not_run_before_the_agents_first_frame() {
        // 200 ms is the documented minimum, so the heartbeat is 50 ms and the grace is
        // clearly longer than the limit it protects.
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 400);
        heartbeat.launched();

        // The agent is starting: the silence clock is held even well past the limit.
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !heartbeat.silence_exceeded(),
            "a starting agent is not a silent one (300 ms > the 200 ms limit)"
        );
        let launch_age = heartbeat.silence_ms();
        assert!(
            launch_age >= 300,
            "the log shows at least the requested launch age: {launch_age} ms"
        );

        // Its first frame starts the clock: silence is measured from here, not from the
        // launch or from the previous channel.
        heartbeat.on_frame();
        assert_eq!(
            heartbeat.silence_ms(),
            0,
            "the clock starts at the first frame"
        );
    }

    #[test]
    fn the_first_frame_starts_the_silence_clock() {
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 5_000);
        heartbeat.launched();
        // A start that outlasts the limit twice over: the old clock called this "the agent
        // stopped answering"; it is only startup.
        std::thread::sleep(Duration::from_millis(400));
        heartbeat.on_frame();
        assert!(
            !heartbeat.silence_exceeded(),
            "the clock starts at the frame"
        );

        // Rule 6 applies from the first frame onwards: silent past the limit is lost.
        std::thread::sleep(Duration::from_millis(260));
        assert!(
            heartbeat.silence_exceeded(),
            "260 ms of silence is past the 200 ms limit"
        );

        // Any later frame resets it, exactly as the sans-IO watchdog's contract says.
        heartbeat.on_frame();
        assert_eq!(heartbeat.silence_ms(), 0, "any frame resets the silence");
        assert!(!heartbeat.silence_exceeded());
    }

    #[test]
    fn a_launch_that_never_answers_is_lost_at_the_launch_timeout() {
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 150);
        heartbeat.launched();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !heartbeat.silence_exceeded(),
            "inside the launch timeout the agent is still starting"
        );
        // Sleep can overshoot by several milliseconds; assert the deadline's state, not
        // the exact remainder implied by the requested 50 ms sleep.
        let remaining = heartbeat.next_silence_check_in_ms();
        assert!(
            (1..150).contains(&remaining),
            "the launch deadline is still ahead and has advanced: remaining={remaining} ms"
        );

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            heartbeat.silence_exceeded(),
            "a launch that never produces a frame is lost at the launch timeout"
        );
        assert_eq!(
            heartbeat.next_silence_check_in_ms(),
            0,
            "the decision is due now"
        );
    }

    #[test]
    fn a_suspend_does_not_read_as_the_agents_silence() {
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 5_000);
        heartbeat.launched();
        heartbeat.on_frame();
        // The machine sleeps: the process is not serviced, so the monotonic clock alone
        // would show the whole sleep as silence.
        std::thread::sleep(Duration::from_millis(260));
        assert!(
            heartbeat.silence_exceeded(),
            "without a resume, 260 ms of no frames is silence (the 200 ms limit)"
        );
        // The resume is what `Session::observe_clock` reports, and it re-arms the clock.
        heartbeat.on_resume();
        assert_eq!(
            heartbeat.silence_ms(),
            0,
            "the freeze is not the agent's silence"
        );
        assert!(!heartbeat.silence_exceeded());
    }

    #[test]
    fn the_heartbeat_period_restarts_with_every_launch() {
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 5_000);
        heartbeat.launched();
        // Rule 11: one heartbeat every `core_silence_ms / 4`.
        assert!(!heartbeat.heartbeat_due(), "a fresh channel owes no ping");
        assert!(heartbeat.next_heartbeat_in_ms() <= 50);
        std::thread::sleep(Duration::from_millis(60));
        assert!(heartbeat.heartbeat_due(), "50 ms later the ping is due");
        heartbeat.on_heartbeat();
        assert!(!heartbeat.heartbeat_due(), "and not again immediately");

        // A second launch owes no ping for the time the first one took either.
        std::thread::sleep(Duration::from_millis(60));
        heartbeat.launched();
        assert!(!heartbeat.heartbeat_due());
        assert!(!heartbeat.silence_exceeded());
    }

    #[test]
    fn the_grace_covers_every_start_a_restart_performs() {
        let mut heartbeat = AgentHeartbeat::with_launch_timeout(200, 400);
        // The RECOVERY restart: its own start, its frame, then the NORMAL start.
        heartbeat.launched();
        std::thread::sleep(Duration::from_millis(100));
        heartbeat.on_frame();
        heartbeat.launched();
        assert!(
            !heartbeat.silence_exceeded(),
            "the fresh NORMAL launch gets the grace again"
        );
        heartbeat.on_frame();
        assert_eq!(heartbeat.silence_ms(), 0);
    }
}
