//! The rotating log file `lws-core` and `lws-agent` both write.
//!
//! `docs/03-architecture.md`, «Конфигурация, пути и логи» puts the logs in one directory
//! per user (`%LOCALAPPDATA%\lan-workspace\logs\` on Windows,
//! `$XDG_STATE_HOME/lan-workspace/logs/` elsewhere) and gives `[log]` a `level`, a
//! `max_file_mb` and a `max_files`. A daemon that a human starts once and then closes the
//! window of writes those files itself, because its stderr may be nowhere: the GUI starts it
//! with no console at all, and a Windows service-shaped process has no terminal to inherit.
//!
//! What goes to the files is exactly what goes to stderr — the same `tracing` events, no
//! second set of rules. Nothing in this crate can invent a field, so the promise that a key
//! or clipboard content is never logged is kept where the events are *written*
//! (`crates/identity` never logs key material, `crates/clipboard` never logs payloads).
//!
//! # One writer, two destinations
//!
//! [`Tee`] is what `tracing_subscriber::fmt().with_writer(...)` gets: every record goes to
//! stderr **and** to the file. stderr stays, and not only for a human: `lws-core`'s own
//! integration tests read a core's log from a redirected stderr, and a daemon started from a
//! terminal should say what it is doing there too.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// At least this much in one file, whatever `[log] max_file_mb` says.
///
/// A configuration that asks for `0` (or a corrupt one that parses as `0`) would otherwise
/// rotate on every record and keep a directory of single lines.
const MIN_FILE_BYTES: u64 = 1024 * 1024;

/// One append-only log file that rotates itself at a size.
///
/// The files of one name are `<name>.log` (the current one) and `<name>.1.log` …
/// `<name>.<max_files - 1>.log` (the previous ones, newest first). Rotation renames them
/// along and reopens a fresh `<name>.log`; the oldest is removed, so `max_files` files are
/// kept and no more.
pub struct RotatingFile {
    path: PathBuf,
    name: String,
    file: File,
    written: u64,
    max_bytes: u64,
    max_files: u32,
}

impl RotatingFile {
    /// Opens `<dir>/<name>.log`, creating `dir` and the file when they are missing.
    ///
    /// `None` is the honest answer when the directory cannot be created or the file cannot be
    /// opened: a host with a read-only state directory still gets its stderr, and the caller
    /// reports the reason once instead of failing to start over a log.
    pub fn open(dir: &Path, name: &str, max_file_mb: u32, max_files: u32) -> Option<RotatingFile> {
        if let Err(error) = std::fs::create_dir_all(dir) {
            eprintln!("cannot create the log directory {}: {error}", dir.display());
            return None;
        }
        let path = dir.join(format!("{name}.log"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                eprintln!("cannot open the log file {}: {error}", path.display());
                error
            })
            .ok()?;
        let written = file.metadata().map(|data| data.len()).unwrap_or(0);
        Some(RotatingFile {
            path,
            name: name.to_owned(),
            file,
            written,
            max_bytes: u64::from(max_file_mb)
                .saturating_mul(1024 * 1024)
                .max(MIN_FILE_BYTES),
            max_files: max_files.max(1),
        })
    }

    /// The file being written, for the one line that tells a human where the logs are.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rotates when the file has reached its size: the oldest file goes, the others shift,
    /// and a fresh one is opened. A rename that fails is not fatal — the file keeps growing,
    /// which beats losing the record that was about to be written.
    fn rotate_if_full(&mut self) {
        if self.written < self.max_bytes {
            return;
        }
        let Some(dir) = self.path.parent().map(Path::to_path_buf) else {
            return;
        };
        let file_of = |index: u32| dir.join(format!("{}.{index}.log", self.name));
        if self.max_files <= 1 {
            // One file only: start it over rather than keep a directory of them.
            if let Ok(fresh) = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                self.file = fresh;
                self.written = 0;
            }
            return;
        }
        let _ = std::fs::remove_file(file_of(self.max_files - 1));
        for index in (1..self.max_files - 1).rev() {
            let _ = std::fs::rename(file_of(index), file_of(index + 1));
        }
        if std::fs::rename(&self.path, file_of(1)).is_ok() {
            if let Ok(fresh) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                self.file = fresh;
                self.written = 0;
            }
        }
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_full();
        let written = self.file.write(buf)?;
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// Opens a rotating file for `tracing`, or reports why it cannot.
///
/// The caller hands the result to [`Tee`]; `None` means "stderr only", which is what a host
/// with an unwritable log directory gets.
pub fn file_sink(
    dir: &Path,
    name: &str,
    max_file_mb: u32,
    max_files: u32,
) -> Option<Arc<Mutex<RotatingFile>>> {
    RotatingFile::open(dir, name, max_file_mb, max_files).map(|file| Arc::new(Mutex::new(file)))
}

/// The per-record writer: stderr always, the log file when one is open.
///
/// `tracing_subscriber::fmt().with_writer(move || Tee::new(sink.clone()))` is how the two
/// binaries use it; the closure is what `MakeWriter` is implemented for, so no new trait
/// shows up in either `main.rs`.
pub struct Tee {
    file: Option<Arc<Mutex<RotatingFile>>>,
}

impl Tee {
    /// A writer for an open log file, or for stderr alone.
    pub fn new(file: Option<Arc<Mutex<RotatingFile>>>) -> Tee {
        Tee { file }
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // stderr first: it is the destination that always exists, and a busy file must not
        // be able to swallow the line a person watching a terminal is waiting for.
        let _ = io::stderr().write_all(buf);
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lws-logging-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        dir
    }

    #[test]
    fn a_file_rotates_at_its_size_and_keeps_max_files_of_them() {
        let dir = temp_dir("rotate");
        // 1 MiB is the floor, so write just past it: the second write rotates.
        let mut file = RotatingFile::open(&dir, "lws-core", 0, 3).expect("a log file");
        let line = vec![b'x'; 1024 * 1024];
        file.write_all(&line).expect("the first mebibyte");
        file.write_all(b"second\n").expect("past the size");
        file.flush().expect("flush");

        assert!(dir.join("lws-core.log").is_file(), "the current file");
        assert!(dir.join("lws-core.1.log").is_file(), "the rotated one");
        assert!(
            !dir.join("lws-core.2.log").exists(),
            "3 files means .log + .1"
        );
        let rotated = std::fs::read(dir.join("lws-core.1.log")).expect("the rotated file");
        assert_eq!(rotated.len(), 1024 * 1024, "the size the rotation saw");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tee_writes_the_same_bytes_to_the_file() {
        let dir = temp_dir("tee");
        let sink = file_sink(&dir, "lws-agent", 5, 5).expect("a log file");
        let mut tee = Tee::new(Some(Arc::clone(&sink)));
        tee.write_all(b"an event\n").expect("write");
        tee.flush().expect("flush");
        let text = std::fs::read_to_string(dir.join("lws-agent.log")).expect("the log file");
        assert_eq!(text, "an event\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unwritable_directory_is_reported_and_not_fatal() {
        // A path whose parent is a *file* cannot become a directory.
        let dir = temp_dir("unwritable");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("the blocker file");
        assert!(RotatingFile::open(&blocker.join("logs"), "lws-core", 5, 5).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
