//! Local transport endpoints, one module per platform.
//!
//! Both platforms expose the same names, so application code is written once:
//!
//! | item | Windows | Linux |
//! |---|---|---|
//! | [`Endpoint`] | a `\\.\pipe\…` name | a socket path |
//! | [`listen`] | creates the pipe with the current user's DACL | binds the socket 0600 in a 0700 directory |
//! | [`GuiListener::accept`] | the DACL already ran in the kernel | `SO_PEERCRED` uid check after `accept` |
//! | [`connect`] | `CreateFile` on the pipe | `connect(2)` on the socket |
//! | [`agent_pair`] | a private single-instance pipe pair | `socketpair(AF_UNIX)` |
//! | [`gui_endpoint`] | `\\.\pipe\lan-workspace-<SID>` | `$XDG_RUNTIME_DIR/lan-workspace/gui.sock` |
//!
//! See the platform module documentation for the exact security properties and
//! for the one documented deviation (the agent pair on Windows).

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{
    agent_pair, connect, gui_socket_path, listen, peer_credentials, Endpoint, GuiListener,
    DIR_MODE, SOCKET_MODE,
};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{
    agent_pair, connect, current_user_sid_string, gui_pipe_name, gui_sddl, listen, Endpoint,
    GuiListener, PipeOptions, PipeSecurity, PIPE_NAME_PREFIX, PIPE_PATH_PREFIX,
};

#[cfg(not(any(unix, windows)))]
compile_error!(
    "the ipc crate implements the local transport only for Windows (named pipes) and \
     Unix (Unix sockets), which is what docs/03-architecture.md specifies"
);

use crate::IpcError;

/// The stream an accepted local client is served on.
///
/// `lws-core` serves two local channels with it — the CLI's control line and the GUI's
/// `GuiFrame` link — and both platforms hand back their own type, so the alias names the
/// one place that difference lives.
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
/// See [`ClientStream`].
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// The stream a local client *connects* with: what [`connect`] hands back.
///
/// One name for the other end of [`ClientStream`], so a client (`lws-core status`, `lws-gui`,
/// a test that drives `GuiFrame`) is written once for both platforms.
#[cfg(unix)]
pub type ClientConnection = tokio::net::UnixStream;
/// See [`ClientConnection`].
#[cfg(windows)]
pub type ClientConnection = tokio::net::windows::named_pipe::NamedPipeClient;

/// The directory a per-user endpoint name is derived from.
///
/// Windows has no runtime directory, so `lws-core` falls back to the state directory for
/// everything it keeps in one (`apps/lws-core/src/main.rs::private_root`); this mirrors
/// that choice so the daemon, the GUI and the CLI agree on one name without a lookup.
fn endpoint_root(paths: &config::Paths) -> &std::path::Path {
    paths.runtime_dir().unwrap_or_else(|| paths.state_dir())
}

/// The `--root` tag for a per-user endpoint name, or `None` for the default directories.
///
/// A GUI endpoint is per *user* (`\\.\pipe\lan-workspace-<SID>`, the documented name),
/// because a desktop has one GUI, but `--root` promises to put «config/, data/, state/ and
/// run/ under DIR», and two cores for the same user — the loopback smoke, a test host —
/// must not fight over one name. The tag is an FNV-1a hash of the canonical runtime
/// directory: the daemon, the GUI and the CLI build it from the same [`config::Paths`], so
/// they agree without touching the filesystem, and a pipe name cannot carry the path's `\`
/// itself. Both documented defaults count as "not a `--root`".
#[cfg(windows)]
pub fn root_tag(root: &std::path::Path) -> Option<String> {
    let defaults = config::Config::default_paths().ok()?;
    let canonical =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let ours = canonical(root);
    let is_default = [defaults.runtime_dir(), Some(defaults.state_dir())]
        .into_iter()
        .flatten()
        .any(|default| canonical(default) == ours);
    if is_default {
        return None;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ours.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

/// The documented GUI endpoint of a resolved set of paths.
///
/// `docs/03-architecture.md`, «Транспорт и аутентификация»: `\\.\pipe\lan-workspace-<SID>`
/// with the DACL of the current user and `PIPE_REJECT_REMOTE_CLIENTS` on Windows, and
/// `$XDG_RUNTIME_DIR/lan-workspace/gui.sock` (directory 0700, socket 0600) on Linux. This is
/// the *only* place that name is built: `lws-core` binds it, `lws-gui` connects to it, and a
/// test that starts a core with `--root` asks for the same endpoint the core did.
///
/// # Errors
///
/// On Linux, `XDG_RUNTIME_DIR` unset: the documented path needs it and nothing guesses
/// `/tmp` (`config::Paths::runtime_dir` is `None` then). On Windows, failing to read the
/// current user's SID.
#[cfg(unix)]
pub fn gui_endpoint(paths: &config::Paths) -> Result<Endpoint, IpcError> {
    let runtime = paths.runtime_dir().ok_or_else(|| {
        IpcError::config(
            "XDG_RUNTIME_DIR is not set; the GUI socket has no documented path without it",
        )
    })?;
    Ok(Endpoint::socket(gui_socket_path(runtime)))
}

/// See the Unix variant: the same name, as a named pipe.
#[cfg(windows)]
pub fn gui_endpoint(paths: &config::Paths) -> Result<Endpoint, IpcError> {
    let sid = current_user_sid_string()?;
    Ok(Endpoint::pipe(match root_tag(endpoint_root(paths)) {
        Some(tag) => format!("{PIPE_PATH_PREFIX}{PIPE_NAME_PREFIX}{sid}-{tag}"),
        None => gui_pipe_name(&sid),
    }))
}

/// The local control channel of *one core*: the GUI endpoint's documented rules with a
/// `ctl` name, plus the same `--root` tag.
///
/// The control channel belongs to a core, not to the desktop, so it never shares the GUI
/// name: `$XDG_RUNTIME_DIR/lan-workspace/ctl.sock` next to `gui.sock` on Linux, and
/// `\\.\pipe\lan-workspace-ctl-<SID>` next to the GUI pipe on Windows.
#[cfg(unix)]
pub fn ctl_endpoint(paths: &config::Paths) -> Result<Endpoint, IpcError> {
    let runtime = paths.runtime_dir().ok_or_else(|| {
        IpcError::config(
            "XDG_RUNTIME_DIR is not set; the control socket has no documented path without it",
        )
    })?;
    Ok(Endpoint::socket(
        gui_socket_path(runtime).with_file_name("ctl.sock"),
    ))
}

/// See the Unix variant.
#[cfg(windows)]
pub fn ctl_endpoint(paths: &config::Paths) -> Result<Endpoint, IpcError> {
    let sid = current_user_sid_string()?;
    Ok(Endpoint::pipe(format!(
        "{}{}ctl-{sid}{}",
        PIPE_PATH_PREFIX,
        PIPE_NAME_PREFIX,
        root_tag(endpoint_root(paths))
            .map(|tag| format!("-{tag}"))
            .unwrap_or_default()
    )))
}
