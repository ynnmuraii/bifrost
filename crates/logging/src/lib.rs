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
//! # Two layers, one format
//!
//! [`subscriber`] puts one filter over two `tracing_subscriber` layers: stderr, coloured
//! only when it is a terminal, and the file, never coloured. Colour is a property of the
//! destination, not of the process: a console on stderr — a *hidden* console is enough —
//! must not put `ESC[…m` into a file that parsers and grep read, which is what a single
//! `fmt()` subscriber whose `with_ansi` follows stderr did. stderr stays for a second
//! reason: `lws-core`'s own integration tests read a core's log from a redirected stderr.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::layer::SubscriberExt as _;

/// Limits a diagnostic stage to one log record per second across concurrent callers.
#[derive(Default)]
pub struct LogRateLimit(Mutex<Option<Instant>>);

impl LogRateLimit {
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Call only when the log level is enabled, so disabled diagnostics take no lock.
    pub fn allow(&self) -> bool {
        let mut last = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if last.is_some_and(|previous| now.duration_since(previous) < Duration::from_secs(1)) {
            return false;
        }
        *last = Some(now);
        true
    }
}

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

/// The writer of the file layer: one rotating file, and nothing else.
///
/// `tracing_subscriber::fmt::layer().with_writer(logging::file_writer(sink))` is how the two
/// binaries use it; the closure is what `MakeWriter` is implemented for, so no new trait
/// shows up in either `main.rs`.
pub struct FileOnly {
    file: Arc<Mutex<RotatingFile>>,
}

impl FileOnly {
    /// A writer for an open log file.
    pub fn new(file: Arc<Mutex<RotatingFile>>) -> FileOnly {
        FileOnly { file }
    }
}

impl Write for FileOnly {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
        Ok(())
    }
}

/// The `MakeWriter` of the file layer, for a sink from [`file_sink`].
pub fn file_writer(
    file: Arc<Mutex<RotatingFile>>,
) -> impl Fn() -> FileOnly + Send + Sync + 'static {
    move || FileOnly::new(Arc::clone(&file))
}

/// The logger both binaries install: one filter, a stderr layer and an optional file layer.
///
/// `stderr_is_terminal` is the *only* thing that decides colour, and it decides it for the
/// stderr layer alone. A daemon started with a console — `Start-Process` gives it a hidden
/// one — therefore still writes a file that no parser has to strip escapes out of.
pub fn subscriber<W>(
    filter: tracing_subscriber::EnvFilter,
    file: Option<W>,
    stderr_is_terminal: bool,
) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    // The bridge to `log`, which dependencies (winit, zbus) still emit, is installed by
    // `SubscriberInitExt::try_init` itself: calling `LogTracer::init` here would consume the
    // one-time `log::set_logger` slot and make that install fail.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(stderr_is_terminal)
        .with_writer(io::stderr)
        .with_target(false);
    let file_layer = file.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer)
            .with_target(false)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
}

/// Installs [`subscriber`] as the global logger.
///
/// Panics when one is already installed, which only happens if a binary installs two.
pub fn init<W>(filter: tracing_subscriber::EnvFilter, file: Option<W>, stderr_is_terminal: bool)
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::util::SubscriberInitExt::init(subscriber(filter, file, stderr_is_terminal))
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
    fn the_file_writer_writes_the_file() {
        let dir = temp_dir("file-only");
        let sink = file_sink(&dir, "lws-agent", 5, 5).expect("a log file");
        let mut writer = FileOnly::new(Arc::clone(&sink));
        writer.write_all(b"an event\n").expect("write");
        writer.flush().expect("flush");
        let text = std::fs::read_to_string(dir.join("lws-agent.log")).expect("the log file");
        assert_eq!(text, "an event\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The regression this crate is named for twice over: a console on stderr (a Windows
    /// hidden console is one) used to colour the *file* as well, and `ESC[…m` around every
    /// field broke the fault-injection harness' parser. The file layer is the one that must
    /// stay plain, even with `stderr_is_terminal` true.
    #[test]
    fn the_file_layer_never_writes_ansi_escapes() {
        struct Buffer(Arc<Mutex<Vec<u8>>>);

        impl Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().expect("the buffer").extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = {
            let bytes = Arc::clone(&bytes);
            move || Buffer(Arc::clone(&bytes))
        };
        let logger = subscriber(
            tracing_subscriber::EnvFilter::new("info"),
            Some(writer),
            true,
        );
        tracing::subscriber::with_default(logger, || {
            tracing::info!(field = "value", "a record for the file layer");
        });
        let text = String::from_utf8(bytes.lock().expect("the buffer").clone()).expect("utf-8");
        assert!(
            !text.contains('\u{1b}'),
            "the file layer carried an escape: {text:?}"
        );
        assert!(
            text.contains("field") && text.contains("a record for the file layer"),
            "the record reached the file layer: {text:?}"
        );
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
