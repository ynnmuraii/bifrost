//! Atomic file writing, as required by `docs/03-architecture.md`, «Правила
//! записи»:
//!
//! «Атомарно: временный файл → fsync → rename; прежняя версия → `.bak`.»
//!
//! The sequence is: write `<path>.tmp` and `fsync` it, copy and sync the old
//! primary into a temporary backup, publish that backup, then atomically rename
//! the new file onto `<path>`. The primary is never moved aside: a concurrent
//! reader sees the old or new file even when publishing the replacement fails.
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
/// file is removed and the primary is left untouched.
pub fn save_atomic(path: &Path, contents: &str) -> Result<(), ConfigError> {
    save_atomic_before_replace(path, contents, |_| {})
}

fn save_atomic_before_replace(
    path: &Path,
    contents: &str,
    before_replace: impl FnOnce(&Path),
) -> Result<(), ConfigError> {
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    fs::create_dir_all(&directory).map_err(|source| io_error(&directory, source))?;

    let temporary = suffixed(path, ".tmp");
    let backup = suffixed(path, ".bak");

    write_synced(&temporary, contents)?;

    if path.exists() {
        let backup_temporary = suffixed(path, ".bak.tmp");
        let copied = fs::copy(path, &backup_temporary).and_then(|_| {
            fs::File::options()
                .write(true)
                .open(&backup_temporary)?
                .sync_all()?;
            fs::rename(&backup_temporary, &backup)
        });
        if let Err(source) = copied {
            let _ = fs::remove_file(&backup_temporary);
            let _ = fs::remove_file(&temporary);
            return Err(io_error(path, source));
        }
    }

    before_replace(&temporary);

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
pub(crate) fn suffixed(path: &Path, suffix: &str) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_failure_preserves_primary_for_concurrent_reader() {
        let directory =
            std::env::temp_dir().join(format!("lws-config-replacement-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.toml");
        fs::write(&path, "previous").unwrap();
        let mut observed = None;
        let result = save_atomic_before_replace(&path, "next", |temporary| {
            // Pause at the publication boundary; a different thread reads the
            // primary while the writer is still in its critical section.
            observed = Some(std::thread::scope(|scope| {
                scope.spawn(|| fs::read_to_string(&path)).join().unwrap()
            }));
            // Inject a failure of the pending rename without OS-specific timing.
            fs::remove_file(temporary).unwrap();
        });
        assert!(
            result.is_err(),
            "the injected publication failure must surface"
        );
        assert_eq!(observed.unwrap().unwrap(), "previous");
        assert_eq!(fs::read_to_string(&path).unwrap(), "previous");
        fs::remove_dir_all(directory).unwrap();
    }
}
