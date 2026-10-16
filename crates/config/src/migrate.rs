//! The one-shot, on-disk half of the legacy-hotkey compatibility.
//!
//! [`crate::Config::load`] accepts the legacy keys everywhere, so a read-only
//! role — the GUI, the tray, a CLI status — starts on a pre-`[input.hotkeys]`
//! file without touching it. The core owns `config.toml`, so it calls
//! [`migrate_legacy_hotkeys`] once: the file is read, the legacy keys are folded
//! into `[input.hotkeys]` and the whole document is written back through
//! [`crate::update_locked`], the same lock and the same single writer every other
//! persistence path uses, so a concurrent ownership or layout write survives.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::legacy;
use crate::lock::update_locked;

/// The suffix of the copy [`migrate_legacy_hotkeys`] keeps next to the file.
const PRESERVED_SUFFIX: &str = ".pre-hotkeys";

/// Rewrites `config_path` through the single-writer path when it still carries a
/// legacy hotkey key, and says whether it did.
///
/// Before the rewrite, the untouched original is copied once to
/// `<config_path>.pre-hotkeys` (see [`preserve_original`]). The rewrite itself
/// lands the migrated document through [`update_locked`], whose atomic save keeps
/// the version it replaced as `config.toml.bak` — but the core persists its
/// ownership pair moments later through the same path, and *that* write replaces
/// `.bak`, so the copy taken before the first write is the only one that still
/// holds the original.
///
/// The second start therefore does not write: once `[input.hotkeys]` is in the
/// file there is no legacy key left to find.
pub fn migrate_legacy_hotkeys(config_path: &Path) -> Result<bool, ConfigError> {
    let original = read(config_path)?;
    if !carries_legacy_hotkeys(&original)? {
        return Ok(false);
    }
    let preserved = preserve_original(config_path, &original)?;
    tracing::info!(
        path = %preserved.display(),
        "the pre-migration configuration is kept before the legacy hotkey keys are folded in"
    );
    update_locked(config_path, |_| ())?;
    Ok(true)
}

/// The whole file as text.
fn read(config_path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(config_path).map_err(|source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })
}

/// True when `text` still sets a legacy hotkey key.
fn carries_legacy_hotkeys(text: &str) -> Result<bool, ConfigError> {
    let document: toml::Value =
        toml::from_str(text).map_err(|source| ConfigError::Parse { source })?;
    Ok(legacy::present(&document))
}

/// Copies `original` to `<config_path>.pre-hotkeys` and returns that path.
///
/// Created with `create_new`, so an existing copy is never overwritten: the first
/// migration's copy outlives every later write. On Unix the copy carries
/// `config.toml`'s own mode — not the mode the umask would give a fresh file — and
/// it takes that mode before any of the content is written, so the original is
/// never briefly readable by anyone else.
fn preserve_original(config_path: &Path, original: &str) -> Result<PathBuf, ConfigError> {
    let kept = preserved_path(config_path);
    let io_error = |path: &Path, source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&kept)
    {
        Ok(file) => file,
        // The copy is already there: keep it and migrate anyway.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(kept),
        Err(source) => return Err(io_error(&kept, source)),
    };
    copy_mode(config_path, &kept).map_err(|source| io_error(&kept, source))?;
    file.write_all(original.as_bytes())
        .map_err(|source| io_error(&kept, source))?;
    file.sync_all().map_err(|source| io_error(&kept, source))?;
    Ok(kept)
}

/// `config_path` with [`PRESERVED_SUFFIX`] appended to its name.
fn preserved_path(config_path: &Path) -> PathBuf {
    let mut name = config_path.as_os_str().to_owned();
    name.push(PRESERVED_SUFFIX);
    PathBuf::from(name)
}

/// On Unix, give `to` the mode of `from` (`config.toml` is `0600`).
#[cfg(unix)]
fn copy_mode(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(from)?.permissions().mode() & 0o7777;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode))
}

/// Windows (and everything else): the per-user directory ACL the identity layer
/// applies already covers the sibling, and [`crate::save_atomic`] sets no mode
/// either, so there is nothing to copy.
#[cfg(not(unix))]
fn copy_mode(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Ok(())
}
