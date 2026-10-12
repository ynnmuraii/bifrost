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
use std::time::Duration;

use ::ipc::transport::{self, Endpoint};
use ::ipc::{AgentLink, IpcError};
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
    dry_inject: bool,
    dry_input_events: usize,
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
        dry_inject: bool,
        dry_input_events: usize,
        dry_motion: bool,
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
            dry_inject,
            dry_input_events,
            dry_motion,
            emergency_hotkey,
            backend_override,
            require_emergency_hotkey,
            core_silence_ms,
            portal,
            takeover,
            repeat_mode,
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

    /// Spawns one agent, waits for it to connect and sends the `AgentAttach`.
    pub async fn start(&mut self, mode: AgentMode) -> Result<(), String> {
        self.stop().await;
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
            // The agent explains itself on stderr; stdout stays quiet.
            .stdout(std::process::Stdio::null());
        if self.dry_inject {
            command.arg("--dry-inject");
            if self.dry_input_events > 0 {
                command
                    .arg("--dry-input-events")
                    .arg(self.dry_input_events.to_string());
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
            Some(link) => link.send(message).await.map_err(|error| match error {
                IpcError::Closed => "the agent channel is closed".to_owned(),
                other => other.to_string(),
            }),
            None => Err("no agent is attached".to_owned()),
        }
    }

    /// Receives one frame, or reports why the channel ended.
    pub async fn recv(&mut self) -> Result<ipc::AgentFrame, String> {
        match self.link.as_mut() {
            Some(link) => link.recv().await.map_err(|error| error.to_string()),
            None => Err("no agent is attached".to_owned()),
        }
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

    /// Marks the agent as attached and ready (after `AgentReady`).
    pub fn mark_ready(&mut self) {
        self.state = AgentState::Ready;
    }

    /// Marks the agent as lost.
    pub fn mark_lost(&mut self) {
        self.state = AgentState::Lost;
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

    #[tokio::test]
    async fn an_agent_that_cannot_be_spawned_is_reported_as_an_error() {
        let runtime = temp_dir("runtime");
        let private = temp_dir("private");
        let mut supervisor = Supervisor::new(
            PathBuf::from("definitely-not-an-agent-program"),
            private.clone(),
            runtime.clone(),
            false,
            0,
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
