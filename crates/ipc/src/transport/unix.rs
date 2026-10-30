//! Unix-domain-socket endpoints (Linux and other Unix targets).
//!
//! `docs/03-architecture.md`, «Транспорт и аутентификация»:
//!
//! | | Linux |
//! |---|---|
//! | core ↔ agent | `socketpair(AF_UNIX)`, унаследованный дескриптор |
//! | gui ↔ core | UDS `$XDG_RUNTIME_DIR/bifrost/gui.sock`, каталог 0700, сокет 0600, проверка `SO_PEERCRED` uid |
//!
//! [`agent_pair`] is the `socketpair` of the first row; the core passes one end
//! to the agent it spawns, so there is no path to connect to and no window in
//! which another process could get there first.
//!
//! [`listen`] implements the second row: the runtime directory is created 0700 if
//! missing, the socket is chmod-ed 0600, and every accepted peer is checked
//! against `SO_PEERCRED` with the [`crate::peer::SameUser`] hook.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::error::IpcError;
use crate::peer::{PeerCheck, PeerCredentials};

/// Mode of the runtime directory (owner only).
pub const DIR_MODE: u32 = 0o700;
/// Mode of the GUI socket (owner only).
pub const SOCKET_MODE: u32 = 0o600;

/// Where a Unix socket lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    path: PathBuf,
}

impl Endpoint {
    /// An endpoint at an explicit path.
    pub fn socket(path: impl Into<PathBuf>) -> Self {
        Endpoint { path: path.into() }
    }

    /// `$XDG_RUNTIME_DIR/bifrost/gui.sock`.
    ///
    /// Core should prefer the value from `config::Paths::gui_ipc()`, which
    /// resolves the same path from the same environment variable; this
    /// constructor exists for callers that have no configuration at hand.
    pub fn gui() -> Result<Self, IpcError> {
        let runtime = std::env::var("XDG_RUNTIME_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                IpcError::config(
                    "XDG_RUNTIME_DIR is not set; the GUI socket has no documented path without it",
                )
            })?;
        Ok(Endpoint::socket(gui_socket_path(Path::new(&runtime))))
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A human-readable form for logs and errors.
    pub fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// `runtime_dir/bifrost/gui.sock`.
///
/// The two names come from [`config::paths`], the single source of the documented file
/// names; this function only adds the `$XDG_RUNTIME_DIR` prefix.
pub fn gui_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir
        .join(config::paths::APP_DIR)
        .join(config::paths::GUI_SOCKET_FILE)
}

/// The listening side of the GUI channel.
#[derive(Debug)]
pub struct GuiListener {
    listener: UnixListener,
    path: PathBuf,
    identity: (u64, u64),
}

/// Bind the GUI socket, creating the runtime directory 0700 when missing.
///
/// A stale socket left by a crashed core is replaced; a *live* one is reported as
/// [`IpcError::AlreadyRunning`] instead of being stolen.
pub fn listen(endpoint: &Endpoint) -> Result<GuiListener, IpcError> {
    let path = endpoint.path();
    ensure_runtime_dir(path)?;

    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(IpcError::AlreadyRunning {
                    endpoint: endpoint.describe(),
                });
            }
            fs::remove_file(path)
                .map_err(|source| IpcError::io("removing a stale GUI socket", source))?;
            UnixListener::bind(path)
                .map_err(|source| IpcError::io("binding the GUI socket", source))?
        }
        Err(error) => return Err(IpcError::io("binding the GUI socket", error)),
    };

    let metadata = fs::symlink_metadata(path)
        .map_err(|source| IpcError::io("reading the bound socket metadata", source))?;
    let listener = GuiListener {
        listener,
        path: path.to_path_buf(),
        identity: (metadata.dev(), metadata.ino()),
    };
    fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(|source| IpcError::io("setting the GUI socket mode to 0600", source))?;

    Ok(listener)
}

impl Drop for GuiListener {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl GuiListener {
    /// The bound socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The mode of the socket file, for diagnostics and tests.
    pub fn socket_mode(&self) -> Result<u32, IpcError> {
        let metadata = fs::metadata(&self.path)
            .map_err(|source| IpcError::io("reading the GUI socket metadata", source))?;
        Ok(metadata.permissions().mode() & 0o777)
    }

    /// Accept one GUI connection and check its `SO_PEERCRED`.
    pub async fn accept(&mut self, check: &dyn PeerCheck) -> Result<UnixStream, IpcError> {
        let (stream, _address) = self
            .listener
            .accept()
            .await
            .map_err(|source| IpcError::io("accepting a GUI connection", source))?;
        let credentials = peer_credentials(&stream)?;
        check.check(&credentials).map_err(|rejected| {
            crate::error::rejected_on(&self.path.display().to_string(), rejected)
        })?;
        Ok(stream)
    }
}

/// Connect to a Unix socket endpoint.
pub async fn connect(endpoint: &Endpoint) -> Result<UnixStream, IpcError> {
    let stream = UnixStream::connect(endpoint.path())
        .await
        .map_err(|source| IpcError::io("connecting to the IPC socket", source))?;
    Ok(stream)
}

/// The `socketpair(AF_UNIX)` of the core ↔ agent channel.
///
/// The two ends are already connected and unnamed, which is what the documents
/// require: there is no socket to connect to before core is ready.
pub async fn agent_pair() -> Result<(UnixStream, UnixStream), IpcError> {
    let (core_end, agent_end) = UnixStream::pair()
        .map_err(|source| IpcError::io("creating the agent socketpair", source))?;
    Ok((core_end, agent_end))
}

/// `SO_PEERCRED` of an accepted socket.
pub fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials, IpcError> {
    let credentials = stream
        .peer_cred()
        .map_err(|source| IpcError::io("reading SO_PEERCRED", source))?;
    Ok(PeerCredentials::peer_cred(
        credentials.uid(),
        credentials.gid(),
        credentials.pid().and_then(|pid| u32::try_from(pid).ok()),
    ))
}

fn ensure_runtime_dir(socket: &Path) -> Result<(), IpcError> {
    let Some(parent) = socket
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(DIR_MODE);
    match builder.create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(IpcError::io("creating the runtime directory", error)),
    }
    let metadata = fs::symlink_metadata(parent)
        .map_err(|source| IpcError::io("reading the runtime directory metadata", source))?;
    if !metadata.is_dir() || Some(metadata.uid()) != crate::peer::current_uid() {
        return Err(IpcError::config(
            "IPC runtime directory must be a real directory owned by the effective uid",
        ));
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(DIR_MODE))
        .map_err(|source| IpcError::io("setting the runtime directory mode to 0700", source))
}
