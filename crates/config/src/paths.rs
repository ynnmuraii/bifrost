//! Per-OS file locations.
//!
//! This module is the **single source** of the documented path and file names
//! (`docs/03-architecture.md`, «Файлы и пути»): `ipc` imports [`APP_DIR`] and
//! [`GUI_SOCKET_FILE`] from here instead of repeating the literals, and `identity` imports
//! [`IDENTITY_DPAPI_FILE`], [`IDENTITY_KEY_FILE`] and [`TRUST_FILE`]. The alternative — an
//! identity-owned `KEY_FILE_NAME` — was rejected because `identity` pulls in `rustls`/`ring`
//! and `config` must keep cross-checking against `x86_64-unknown-linux-gnu` (ring needs a
//! C toolchain there), so the dependency direction is `ipc`/`identity` → `config`, never the
//! other way.
//!
//! `docs/03-architecture.md`, «Файлы и пути»:
//!
//! | Назначение | Windows | Linux |
//! |---|---|---|
//! | Настройки | `%APPDATA%\lan-workspace\config.toml` | `$XDG_CONFIG_HOME/lan-workspace/config.toml` |
//! | Доверенные узлы | `%APPDATA%\lan-workspace\trust.toml` | `$XDG_CONFIG_HOME/lan-workspace/trust.toml` |
//! | Приватный ключ | `%APPDATA%\lan-workspace\identity.dpapi` | `Secret Service`; иначе `$XDG_DATA_HOME/lan-workspace/identity.key` (0600) |
//! | Логи | `%LOCALAPPDATA%\lan-workspace\logs\` | `$XDG_STATE_HOME/lan-workspace/logs/` |
//! | IPC GUI | `\\.\pipe\lan-workspace-<SID>` | `$XDG_RUNTIME_DIR/lan-workspace/gui.sock` |
//! | Портальные токены | — | `$XDG_STATE_HOME/lan-workspace/portal-restore.toml` (0600, каталог 0700) |
//!
//! The environment is read directly instead of using the `directories` crate:
//! the documents name XDG variables and `%APPDATA%` explicitly, `directories`
//! appends `\config`, `\data` and `\cache` levels that the table does not have,
//! and reading four variables keeps the crate dependency-free and identical on
//! the Windows host and the Linux target.
//!
//! `layout.pending.toml` (divergent post-partition edits) has no row of its own;
//! it is placed next to `config.toml`, which is where the rest of the persisted
//! document state lives.

use core::fmt;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;

/// Application directory name under every root.
pub const APP_DIR: &str = "lan-workspace";

/// The file names inside [`Paths`]' directories.
pub const CONFIG_FILE: &str = "config.toml";
/// Trust store file name.
pub const TRUST_FILE: &str = "trust.toml";
/// Windows private key blob file name.
pub const IDENTITY_DPAPI_FILE: &str = "identity.dpapi";
/// Linux private key file name, used when no Secret Service is available.
pub const IDENTITY_KEY_FILE: &str = "identity.key";
/// Wayland portal restore tokens file name.
pub const PORTAL_RESTORE_FILE: &str = "portal-restore.toml";
/// Divergent post-partition layout edits file name.
pub const LAYOUT_PENDING_FILE: &str = "layout.pending.toml";
/// Logs directory name.
pub const LOGS_DIR: &str = "logs";
/// GUI IPC socket file name (Linux).
pub const GUI_SOCKET_FILE: &str = "gui.sock";

/// Resolved directories for one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}

impl Paths {
    /// Build a set of roots explicitly; used by tests and by callers that do
    /// not want the environment.
    pub fn new(
        config_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        runtime_dir: Option<PathBuf>,
    ) -> Paths {
        Paths {
            config_dir: config_dir.into(),
            data_dir: data_dir.into(),
            state_dir: state_dir.into(),
            runtime_dir,
        }
    }

    /// `%APPDATA%\lan-workspace` on Windows,
    /// `$XDG_CONFIG_HOME/lan-workspace` (or `~/.config/lan-workspace`) elsewhere.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Where the identity key lives: `%APPDATA%\lan-workspace` on Windows,
    /// `$XDG_DATA_HOME/lan-workspace` elsewhere. Private keys themselves belong
    /// to the identity layer, not to this crate.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Logs and portal tokens: `%LOCALAPPDATA%\lan-workspace` on Windows,
    /// `$XDG_STATE_HOME/lan-workspace` elsewhere.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// `$XDG_RUNTIME_DIR/lan-workspace`; `None` on Windows, which uses a named
    /// pipe instead.
    pub fn runtime_dir(&self) -> Option<&Path> {
        self.runtime_dir.as_deref()
    }

    /// `config.toml`.
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }

    /// `<config.toml>.bak`, the previous version kept by the atomic writer.
    pub fn config_backup_file(&self) -> PathBuf {
        self.config_dir.join(format!("{CONFIG_FILE}.bak"))
    }

    /// `trust.toml` (kept separate so diagnostics can ship settings without it).
    pub fn trust_file(&self) -> PathBuf {
        self.config_dir.join(TRUST_FILE)
    }

    /// The private key reference: a DPAPI blob on Windows, a 0600 file on Linux
    /// when no Secret Service is used.
    pub fn identity_file(&self) -> PathBuf {
        if cfg!(windows) {
            self.config_dir.join(IDENTITY_DPAPI_FILE)
        } else {
            self.data_dir.join(IDENTITY_KEY_FILE)
        }
    }

    /// `logs/`, where rotation happens.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_dir.join(LOGS_DIR)
    }

    /// `portal-restore.toml`; never written on Windows.
    pub fn portal_restore_file(&self) -> PathBuf {
        self.state_dir.join(PORTAL_RESTORE_FILE)
    }

    /// `layout.pending.toml`, next to the applied document.
    pub fn layout_pending_file(&self) -> PathBuf {
        self.config_dir.join(LAYOUT_PENDING_FILE)
    }

    /// The GUI IPC endpoint: a `gui.sock` path on Unix, `None` on Windows where
    /// the endpoint is the named pipe `\\.\pipe\lan-workspace-<SID>`.
    pub fn gui_ipc(&self) -> Option<PathBuf> {
        self.runtime_dir
            .as_ref()
            .map(|dir| dir.join(GUI_SOCKET_FILE))
    }
}

impl fmt::Display for Paths {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "config={}, data={}, state={}, runtime={}",
            self.config_dir.display(),
            self.data_dir.display(),
            self.state_dir.display(),
            match &self.runtime_dir {
                Some(dir) => dir.display().to_string(),
                None => "<none>".to_owned(),
            }
        )
    }
}

/// Resolve the documented directories for the current user.
pub fn default_paths() -> Result<Paths, ConfigError> {
    resolve()
}

#[cfg(windows)]
fn resolve() -> Result<Paths, ConfigError> {
    let roaming = var("APPDATA", "the roaming application-data directory")?;
    let local = var("LOCALAPPDATA", "the local application-data directory")?;
    let config_dir = PathBuf::from(roaming).join(APP_DIR);
    Ok(Paths {
        data_dir: config_dir.clone(),
        config_dir,
        state_dir: PathBuf::from(local).join(APP_DIR),
        runtime_dir: None,
    })
}

#[cfg(not(windows))]
fn resolve() -> Result<Paths, ConfigError> {
    let home = var("HOME", "the user home directory")?;
    let home = PathBuf::from(home);
    let config_dir = xdg("XDG_CONFIG_HOME", home.join(".config")).join(APP_DIR);
    let data_dir = xdg("XDG_DATA_HOME", home.join(".local").join("share")).join(APP_DIR);
    let state_dir = xdg("XDG_STATE_HOME", home.join(".local").join("state")).join(APP_DIR);
    // $XDG_RUNTIME_DIR has no documented fallback: without it the GUI socket has
    // no path, and `gui_ipc` reports `None` instead of guessing /tmp.
    let runtime_dir = optional_var("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join(APP_DIR));
    Ok(Paths {
        config_dir,
        data_dir,
        state_dir,
        runtime_dir,
    })
}

#[cfg(not(windows))]
fn xdg(name: &str, fallback: PathBuf) -> PathBuf {
    optional_var(name).map(PathBuf::from).unwrap_or(fallback)
}

fn var(name: &str, purpose: &str) -> Result<String, ConfigError> {
    optional_var(name).ok_or_else(|| ConfigError::Paths {
        detail: format!("{name} is not set; needed for {purpose}"),
    })
}

fn optional_var(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}
