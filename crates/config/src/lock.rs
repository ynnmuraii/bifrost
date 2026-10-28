//! A cross-process lock for the configuration file.
//!
//! `config.toml` has more than one writer: the core persists the ownership pair and the
//! layout revision (two stores, and the GUI drives both), and a first run writes the whole
//! document. More than one process can address the same root — a second `lws-core` start, a
//! CLI command while a core runs, a first run racing a core's first persistence — and each
//! writer reads the file, changes its own fields and writes the **whole** document back. The
//! in-process mutex the core used cannot see another process, so the loser's change vanished
//! (batch-6 audit, F17).
//!
//! The persistent sibling file is protected by an exclusive OS lock. Its age
//! never revokes a live writer; closing the handle (including process exit)
//! releases ownership. Never unlink the file: waiters must lock the same inode,
//! rather than accidentally acquiring independent locks on replacement files.
//!
//! Every writer takes it around its whole read-modify-write: [`update_locked`] is the shape
//! to use, and it is why the stores no longer write `config.toml` field by field.

use std::fs::TryLockError;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::ConfigError;

/// How long `acquire` waits for another writer before giving up.
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to sleep between attempts while another writer holds the lock.
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// A held OS lock. Dropping its file handle releases ownership without unlinking.
#[derive(Debug)]
pub struct FileLock {
    path: PathBuf,
    _file: fs::File,
}

impl FileLock {
    /// Takes the lock for `config`, waiting up to [`ACQUIRE_TIMEOUT`].
    pub fn acquire(config: &Path) -> Result<Self, ConfigError> {
        Self::acquire_within(config, ACQUIRE_TIMEOUT)
    }

    /// The same, with an explicit wait (tests use a short one).
    pub fn acquire_within(config: &Path, timeout: Duration) -> Result<Self, ConfigError> {
        let path = lock_path(config);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ConfigError::Io {
                path: path.clone(),
                source,
            })?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { path, _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(ConfigError::Locked { path });
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(TryLockError::Error(source)) => return Err(ConfigError::Io { path, source }),
            }
        }
    }

    /// The lock file (diagnostics/tests).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Where the lock for `config` lives: a sibling, so it is on the same filesystem.
#[must_use]
pub fn lock_path(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// Reads `config`, applies `change` and writes it back, all under the lock.
///
/// The unit of work is the whole read-modify-write: two writers that only change their own
/// fields still read the file the other one wrote, which is what makes their changes survive
/// (a lock around the write alone would not).
pub fn update_locked<T>(
    config_path: &Path,
    change: impl FnOnce(&mut crate::Config) -> T,
) -> Result<T, ConfigError> {
    let _lock = FileLock::acquire(config_path)?;
    let mut config = crate::Config::load(config_path)?;
    let outcome = change(&mut config);
    config.save(config_path)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::Config;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lws-config-lock-{}-{}-{label}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a temporary directory");
        dir
    }

    /// A configuration on disk with a known serial and revision.
    fn write_config(dir: &Path) -> PathBuf {
        let path = dir.join("config.toml");
        let mut config = Config::default();
        // The schema refuses a configuration without a concrete listen address.
        config.network.listen_address = "192.168.1.10".to_owned();
        config.workspace.owner_claim_serial = 1;
        config.workspace.owner_node_id = crate::peer_reference(&[0x11; 32]);
        config.topology.revision = 1;
        // A revision above zero needs its author named (the schema's own consistency rule).
        config.topology.author = crate::peer_reference(&[0x22; 32]);
        config.topology.max_seen_revision = 1;
        config.topology.max_seen_author = crate::peer_reference(&[0x22; 32]);
        config.save(&path).expect("the initial configuration");
        path
    }

    /// Batch-6 audit (F17): two writers that change *different* fields, N times each, must
    /// both survive - the loser used to overwrite the winner's whole file.
    ///
    /// Separate `update_locked` calls (so separate locks and separate reads) with a barrier
    /// between them is the closest a unit test gets to two processes; the lock itself is a
    /// file, so the same exclusion applies across processes.
    #[test]
    fn two_writers_of_different_fields_both_survive() {
        let dir = temp_dir("two-writers");
        let path = write_config(&dir);
        const ROUNDS: u64 = 20;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let serial_writer = {
            let path = path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                for round in 0..ROUNDS {
                    barrier.wait();
                    update_locked(&path, |config| {
                        config.workspace.owner_claim_serial = 100 + round;
                    })
                    .expect("the serial writer holds the lock");
                }
            })
        };
        let revision_writer = {
            let path = path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                for round in 0..ROUNDS {
                    barrier.wait();
                    update_locked(&path, |config| {
                        // The schema keeps the floor at or above the applied revision, which is
                        // what the real layout store writes too.
                        config.topology.revision = 200 + round;
                        config.topology.max_seen_revision = 200 + round;
                    })
                    .expect("the revision writer holds the lock");
                }
            })
        };
        serial_writer.join().expect("the serial writer");
        revision_writer.join().expect("the revision writer");

        let config = Config::load(&path).expect("the file still parses");
        assert_eq!(
            config.workspace.owner_claim_serial,
            100 + ROUNDS - 1,
            "the serial writer's last value survived"
        );
        assert_eq!(
            config.topology.revision,
            200 + ROUNDS - 1,
            "the revision writer's last value survived"
        );
        FileLock::acquire_within(&path, Duration::ZERO)
            .expect("the OS lock is released when the last writer is done");
    }

    /// An abandoned lock file never prevents the kernel from admitting a writer.
    #[test]
    fn an_abandoned_lock_file_is_reused() {
        let dir = temp_dir("stale");
        let path = write_config(&dir);
        let lock = lock_path(&path);
        fs::write(&lock, b"1234\n").expect("a lock file");
        // Age is irrelevant: even a fresh abandoned file is immediately usable.
        let old = SystemTime::UNIX_EPOCH;
        fs::File::options()
            .write(true)
            .open(&lock)
            .expect("open the lock file")
            .set_modified(old)
            .expect("backdate the lock file");

        let held = FileLock::acquire_within(&path, Duration::from_millis(200))
            .expect("an abandoned file is reused, not waited out");
        assert_eq!(held.path(), lock.as_path());
        drop(held);
        // And `update_locked` does not trip over it either.
        update_locked(&path, |config| {
            config.topology.revision = 7;
            config.topology.max_seen_revision = 7;
        })
        .expect("the update runs");
        assert_eq!(
            Config::load(&path)
                .expect("the file parses")
                .topology
                .revision,
            7
        );
    }

    #[test]
    fn backdated_live_owner_cannot_be_displaced() {
        let dir = temp_dir("backdated-live");
        let path = write_config(&dir);
        let owner = FileLock::acquire(&path).unwrap();
        fs::File::options()
            .write(true)
            .open(owner.path())
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH)
            .unwrap();
        assert!(
            matches!(
                FileLock::acquire_within(&path, Duration::ZERO),
                Err(ConfigError::Locked { .. })
            ),
            "mtime cannot revoke a live owner's exclusive lock"
        );
        drop(owner);
        let successor = FileLock::acquire_within(&path, Duration::ZERO).unwrap();
        assert!(
            FileLock::acquire_within(&path, Duration::ZERO).is_err(),
            "the successor remains the exclusive owner"
        );
        drop(successor);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn releasing_lock_preserves_the_shared_lock_file() {
        let dir = temp_dir("persistent-lock");
        let path = write_config(&dir);
        let owner = FileLock::acquire(&path).unwrap();
        let lock = owner.path().to_owned();
        drop(owner);
        assert!(
            lock.exists(),
            "unlinking allows writers to lock different files"
        );
        let successor = FileLock::acquire_within(&path, Duration::ZERO).unwrap();
        assert!(FileLock::acquire_within(&path, Duration::ZERO).is_err());
        drop(successor);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn process_lock_owner_exits_without_drop() {
        let Some(path) = std::env::var_os("LWS_CONFIG_LOCK_CHILD") else {
            return;
        };
        let owner = FileLock::acquire(Path::new(&path)).unwrap();
        std::mem::forget(owner);
    }

    #[test]
    fn process_exit_releases_lock_without_waiting_for_stale_age() {
        let dir = temp_dir("process-exit");
        let path = write_config(&dir);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lock::tests::process_lock_owner_exits_without_drop",
            ])
            .env("LWS_CONFIG_LOCK_CHILD", &path)
            .status()
            .unwrap();
        assert!(status.success());
        let successor = FileLock::acquire_within(&path, Duration::ZERO)
            .expect("the kernel releases a departed process's lock immediately");
        drop(successor);
        fs::remove_dir_all(dir).unwrap();
    }

    /// A live lock is respected: a second writer waits, then reports the timeout.
    #[test]
    fn a_live_lock_is_respected_and_times_out() {
        let dir = temp_dir("live");
        let path = write_config(&dir);
        let held = FileLock::acquire(&path).expect("the first lock");
        let error = FileLock::acquire_within(&path, Duration::from_millis(100))
            .expect_err("the second writer must not take a live lock");
        assert!(
            matches!(error, ConfigError::Locked { .. }),
            "the refusal names the lock: {error}"
        );
        drop(held);
        FileLock::acquire_within(&path, Duration::from_millis(100))
            .expect("and it is free once the writer drops it");
    }
}
