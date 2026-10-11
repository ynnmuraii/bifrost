//! Atomic file writing, as required by `docs/03-architecture.md`, «Правила
//! записи»:
//!
//! «Атомарно: временный файл → fsync → rename; прежняя версия → `.bak`.»
//!
//! The sequence is: write `<path>.tmp` and `fsync` it, move any existing
//! `<path>` aside to `<path>.bak`, then rename the temporary file onto `<path>`.
//! A reader therefore sees either the old file or the new one, never a partial
//! write, and the previous version survives as `<path>.bak`.
//!
//! Directory permissions (0700 / owner-only ACL) are *not* handled here: the
//! documents assign that to the platform setup layer, and this crate performs no
//! platform calls.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;

/// Write `contents` to `path` atomically, keeping the previous version as
/// `<path>.bak`.
///
/// The parent directory is created when missing. On any failure the temporary
/// file is removed and `path` is left untouched (unless the failure happened
/// after the old file was moved aside, in which case the old version is at
/// `<path>.bak`).
pub fn save_atomic(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    fs::create_dir_all(&directory).map_err(|source| io_error(&directory, source))?;

    let temporary = suffixed(path, ".tmp");
    let backup = suffixed(path, ".bak");

    write_synced(&temporary, contents)?;

    if path.exists() {
        // Windows refuses to rename onto an existing name, so drop the old
        // backup first; a missing backup is not an error.
        let _ = fs::remove_file(&backup);
        if let Err(source) = fs::rename(path, &backup) {
            let _ = fs::remove_file(&temporary);
            return Err(io_error(path, source));
        }
    }

    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(path, source));
    }

    sync_directory(&directory);
    Ok(())
}

fn write_synced(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let mut file = fs::File::create(path).map_err(|source| io_error(path, source))?;
    file.write_all(contents.as_bytes())
        .map_err(|source| io_error(path, source))?;
    file.sync_all().map_err(|source| io_error(path, source))
}

#[cfg(unix)]
fn sync_directory(directory: &Path) {
    // Best effort: making the rename durable needs a directory fsync, which is
    // only meaningful on Unix.
    if let Ok(handle) = fs::File::open(directory) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) {}

/// `path` with `suffix` appended to its name (`config.toml` → `config.toml.bak`).
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

fn io_error(path: &Path, source: std::io::Error) -> ConfigError {
    ConfigError::Io {
        path: path.to_path_buf(),
        source,
    }
}
