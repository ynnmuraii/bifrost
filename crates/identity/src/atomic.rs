//! Atomic file writes, per `docs/03-architecture.md` («Правила записи»).
//!
//! «Атомарно: временный файл → fsync → rename; прежняя версия → `.bak`». Both the
//! identity key and the trust store go through this, because a half-written trust file
//! or key would be indistinguishable from corruption at the next start.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Outcome of a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file was created.
    Created,
    /// The file existed and was replaced; the previous content is in `<path>.bak`.
    Replaced {
        /// The backup path.
        backup: PathBuf,
    },
}

/// Writes `bytes` to `path` atomically, backing up the previous content.
///
/// The temporary file lives in the same directory as `path` so the final `rename` is
/// on one filesystem and therefore atomic. The parent directory is fsynced when the
/// platform allows it, so a crash cannot leave a rename that is not durable.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<WriteOutcome> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;

    let temporary = temporary_path(path);
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    restrict_file(&temporary)?;

    let outcome = if path.exists() {
        // The backup is made through a temporary file and a `rename`, like the write itself:
        // `fs::copy` straight onto `<path>.bak` leaves a *torn* backup if it fails or is
        // interrupted (a full disk, a concurrent writer, a crash), and the documented
        // recovery («При ошибке разбора: загрузить `.bak`») would then have nothing to load.
        // This way `.bak` is always either the whole previous file or the whole new one.
        let backup = backup_path(path);
        copy_atomic(path, &backup)?;
        WriteOutcome::Replaced { backup }
    } else {
        WriteOutcome::Created
    };

    fs::rename(&temporary, path)?;
    sync_directory(directory);
    Ok(outcome)
}

/// Copies `from` onto `to` atomically: write a temporary sibling, fsync it, rename it over the
/// destination.
fn copy_atomic(from: &Path, to: &Path) -> io::Result<()> {
    let temporary = temporary_path(to);
    {
        let mut source = fs::File::open(from)?;
        let mut target = fs::File::create(&temporary)?;
        io::copy(&mut source, &mut target)?;
        target.sync_all()?;
    }
    restrict_file(&temporary)?;
    fs::rename(&temporary, to)?;
    Ok(())
}

/// Reads `path`; on a parse failure the caller asks for [`read_with_backup`] instead.
pub fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads `path`, falling back to `<path>.bak` when `parse` rejects the content.
///
/// `docs/03-architecture.md`: «При ошибке разбора: загрузить `.bak`, стартовать в Local,
/// показать предупреждение. Если повреждены оба файла — дефолты» — the "start in Local"
/// half belongs to the caller, this only finds usable bytes.
pub fn read_with_backup<T>(
    path: &Path,
    parse: impl Fn(&[u8]) -> Result<T, String>,
) -> io::Result<LoadResult<T>> {
    if let Some(bytes) = read(path)? {
        match parse(&bytes) {
            Ok(value) => return Ok(LoadResult::Loaded(value)),
            Err(primary_error) => {
                let backup = backup_path(path);
                if let Some(backup_bytes) = read(&backup)? {
                    if let Ok(value) = parse(&backup_bytes) {
                        return Ok(LoadResult::Recovered {
                            value,
                            primary_error,
                            backup,
                        });
                    }
                }
                return Ok(LoadResult::Corrupt { primary_error });
            }
        }
    }
    Ok(LoadResult::Missing)
}

/// What [`read_with_backup`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadResult<T> {
    /// The file was absent.
    Missing,
    /// The file parsed.
    Loaded(T),
    /// The file failed to parse and `.bak` was used instead.
    Recovered {
        /// The parsed backup content.
        value: T,
        /// Why the primary file was rejected.
        primary_error: String,
        /// The backup that was used.
        backup: PathBuf,
    },
    /// Both the file and its backup failed to parse.
    Corrupt {
        /// Why the primary file was rejected.
        primary_error: String,
    },
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// Makes the file reachable only by its owner (`docs/03-architecture.md`: the identity
/// key is a `0600` file, the directory `0700`; Windows relies on the user profile ACL).
#[cfg(unix)]
fn restrict_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn restrict_file(path: &Path) -> io::Result<()> {
    // Owner-only DACL on the file itself, mirroring `0600`: a permissive parent directory
    // must not be able to expose the sealed key.
    crate::acl::apply_owner_only(path).map_err(io::Error::other)
}

/// Creates `path` with owner-only permissions when it does not exist yet.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !path.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path)?;
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        fs::create_dir_all(path)?;
        // `docs/03-architecture.md`: «ACL на текущий SID в Windows». The ACE inherits into
        // files and subdirectories, so everything created here stays owner-only.
        crate::acl::apply_owner_only(path).map_err(io::Error::other)
    }
}

/// Whether a file's permissions are wider than owner-only (Linux check).
///
/// `docs/03-architecture.md`: «Core отказывается стартовать, если права шире, и выдаёт
/// понятную ошибку». On Windows this always reports `false`: the check belongs to the
/// ACL work, not to the POSIX mode bits.
#[cfg(unix)]
pub fn is_wider_than_owner(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 != 0)
}

#[cfg(windows)]
pub fn is_wider_than_owner(path: &Path) -> io::Result<bool> {
    // The mirror of `mode & 0o077 != 0`: `false` only when every allow ACE names the
    // current user SID.
    Ok(!crate::acl::is_owner_only(path).map_err(io::Error::other)?)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) {
    if let Ok(handle) = fs::File::open(directory) {
        let _ = handle.sync_all();
    }
}

#[cfg(windows)]
fn sync_directory(_directory: &Path) {
    // Opening a directory as a file needs `FILE_FLAG_BACKUP_SEMANTICS`; the identity key
    // is re-writable at any time, so an unsynced rename is an acceptable failure mode on
    // Windows (the value is regenerated, not lost).
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lws-atomic-tests-{}-{}-{label}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a temporary directory");
        dir
    }

    /// Batch-5 audit (F18): the backup is the *previous* content, whole, and it parses - the
    /// documented `.bak` recovery depends on it.
    #[test]
    fn the_backup_holds_the_previous_content_and_is_never_torn() {
        let dir = temp_dir("backup");
        let path = dir.join("trust.toml");
        write_atomic(&path, b"first").expect("the first write");
        let outcome = write_atomic(&path, b"second").expect("the second write");
        let WriteOutcome::Replaced { backup } = outcome else {
            panic!("the second write has a backup");
        };
        assert_eq!(fs::read(&path).expect("the primary"), b"second");
        assert_eq!(
            fs::read(&backup).expect("the backup"),
            b"first",
            "the backup is the whole previous file"
        );

        // A further write replaces the backup the same way, so the *current* file is the one
        // the recovery rule reads.
        write_atomic(&path, b"third").expect("the third write");
        assert_eq!(fs::read(&backup).expect("the backup"), b"second");
        assert_eq!(fs::read(&path).expect("the primary"), b"third");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A failed backup leaves the primary exactly as it was: no half-written file, and no
    /// backup that disagrees with the file it is supposed to protect.
    #[test]
    fn a_failed_backup_leaves_the_primary_untouched() {
        let dir = temp_dir("failed-backup");
        let path = dir.join("trust.toml");
        write_atomic(&path, b"first").expect("the first write");
        // A directory where the backup must go: the final rename onto it fails.
        fs::create_dir(dir.join("trust.toml.bak")).expect("a directory in the backup's place");
        assert!(
            write_atomic(&path, b"second").is_err(),
            "the write reports the failure"
        );
        assert_eq!(
            fs::read(&path).expect("the primary"),
            b"first",
            "the primary is not half-written"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
