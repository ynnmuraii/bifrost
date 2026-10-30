//! The one-shot move of the pre-0.4.0 `lan-workspace` directories.
//!
//! 0.4.0 renames [`APP_DIR`] to `bifrost`. Existing installations must keep their
//! configuration, trust store and identity key, so the first start of a release
//! moves the old directory next to the new one:
//!
//! * same parent, so it is a rename on the same filesystem — never a copy;
//! * never merged, never deleted: if both directories exist the new one wins and
//!   the old one is left untouched with a warning;
//! * under an inter-process lock (`.bifrost-migration.lock` in the parent), so
//!   the window, the tray and a core that start together decide once;
//! * if the move is refused — a still-running 0.3.x process holds a file inside
//!   on Windows, a read-only parent, a locked lock file — this run keeps using
//!   the **old** directory and the next start retries.
//!
//! Migration is selected per platform-default config/data/state slot. Arbitrary
//! explicit overrides stay untouched; `--root` disables migration entirely.
//! Explicit legacy defaults from a pre-0.4.0 updater are rewritten to the effective
//! paths before role dispatch, including the ephemeral runtime path.
//!
//! [`adopt`] is called once from `apps/bifrost/src/main.rs` before the role is
//! dispatched and installs the effective paths as this process's
//! [`crate::default_paths`] answer. Nothing else calls into this module, which
//! is what keeps `cargo test -p config` away from a developer's real profile.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::lock::FileLock;
use crate::paths::{self, Paths, APP_DIR};

/// The pre-0.4.0 application directory. Read as a migration source, never written.
pub const LEGACY_APP_DIR: &str = "lan-workspace";

/// The lock file stem in the parent directory; [`FileLock`] appends `.lock`.
const LOCK_STEM: &str = ".bifrost-migration";

/// How long to wait for a sibling process that is moving the same directory.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// What happened to one directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The directory is not a platform default (`--root` layout, explicit flag).
    NotApplicable,
    /// There is no legacy directory: a first run, or the move already happened.
    NothingLegacy,
    /// The legacy directory was renamed to the new one.
    Moved,
    /// Only the new directory exists.
    AlreadyNew,
    /// Both exist; the new one is used and the legacy one is left untouched.
    BothPresent,
    /// The legacy directory is still in use; this run uses it and the next start retries.
    Deferred,
}

/// A log line the relocation wants to emit, kept so a role can log it after its
/// subscriber exists (the window's logging starts after this module runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// How the note should be logged.
    pub severity: Severity,
    /// The line itself.
    pub text: String,
}

/// The severity of a [`Note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A move succeeded.
    Info,
    /// Something needs the user's attention (both directories, deferred move).
    Warn,
}

/// The result of [`relocate`]: the paths this run must use plus what happened.
#[derive(Debug)]
pub struct Relocation {
    /// Effective paths for this process: the new directories, or the legacy ones
    /// for a directory whose move was deferred.
    pub paths: Paths,
    /// Outcome of the configuration directory.
    pub config: Outcome,
    /// Outcome of the data directory.
    pub data: Outcome,
    /// Outcome of the state directory.
    pub state: Outcome,
    /// Lines to log once a subscriber exists.
    pub notes: Vec<Note>,
}

/// The process-wide effective defaults installed by [`adopt`].
static STARTUP: OnceLock<Paths> = OnceLock::new();
/// The report of [`adopt`], so the window can log it after `init_logging`.
static REPORT: OnceLock<Relocation> = OnceLock::new();

/// The effective defaults of a process that adopted them, if any.
pub(crate) fn startup_paths() -> Option<&'static Paths> {
    STARTUP.get()
}

/// Moves the legacy directories, logs the outcome and installs the effective
/// paths as this process's defaults.
///
/// `None` only when the platform directories cannot be resolved at all (a
/// missing `HOME`/`%APPDATA%`); the role then fails the same way it would have
/// without this module.
pub fn adopt(args: &mut [String]) -> Option<&'static Relocation> {
    let cli = crate::CliPaths::parse(args.iter());
    if cli.root.is_some() {
        return None;
    }
    let paths = paths::resolve().ok()?;
    let relocation = prepare_arguments(paths, &cli, args, LOCK_WAIT, &rename_dir);
    log(&relocation);
    let _ = STARTUP.set(relocation.paths.clone());
    let _ = REPORT.set(relocation);
    REPORT.get()
}

/// Logs the last [`adopt`] report again, for a role whose subscriber is created
/// after startup (the window).
pub fn log_last_report() {
    if let Some(relocation) = REPORT.get() {
        log(relocation);
    }
}

fn log(relocation: &Relocation) {
    for note in &relocation.notes {
        match note.severity {
            Severity::Info => tracing::info!("{}", note.text),
            Severity::Warn => {
                tracing::warn!("{}", note.text);
                // Roles with a console (core, CLI, updater, tray) must see this
                // even before the tracing subscriber exists.
                eprintln!("bifrost: {}", note.text);
            }
        }
    }
}

/// Moves the legacy directories next to every default directory of `paths`.
///
/// The runtime directory is not migrated: it is ephemeral, and a stale
/// `lan-workspace` directory under `$XDG_RUNTIME_DIR` is left alone.
#[must_use]
pub fn relocate(paths: Paths) -> Relocation {
    relocate_with(paths, [true; 3], LOCK_WAIT, &rename_dir)
}

/// Recognizes a slot's old platform default, not an arbitrary directory with the same name.
fn legacy_default(path: &Path) -> Option<PathBuf> {
    if path.file_name()? != APP_DIR {
        return None;
    }
    Some(path.parent()?.join(LEGACY_APP_DIR))
}

fn prepare_arguments(
    defaults: Paths,
    cli: &crate::CliPaths,
    args: &mut [String],
    wait: Duration,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Relocation {
    let selected = |value: &Option<PathBuf>, default: &Path| {
        value
            .as_ref()
            .is_none_or(|value| Some(value) == legacy_default(default).as_ref())
    };
    let enabled = [
        selected(&cli.config_dir, defaults.config_dir()),
        selected(&cli.data_dir, defaults.data_dir()),
        selected(&cli.state_dir, defaults.state_dir()),
    ];
    let relocation = relocate_with(defaults.clone(), enabled, wait, rename);
    let slots = [
        (
            "--config-dir",
            Some(defaults.config_dir()),
            Some(relocation.paths.config_dir()),
        ),
        (
            "--data-dir",
            Some(defaults.data_dir()),
            Some(relocation.paths.data_dir()),
        ),
        (
            "--state-dir",
            Some(defaults.state_dir()),
            Some(relocation.paths.state_dir()),
        ),
        (
            "--runtime-dir",
            defaults.runtime_dir(),
            relocation.paths.runtime_dir(),
        ),
    ];
    let mut index = 0;
    while index < args.len() {
        let (flag, inline) = args[index]
            .split_once('=')
            .map_or((args[index].as_str(), false), |(flag, _)| (flag, true));
        let slot = slots.iter().find(|(name, _, _)| *name == flag);
        let value_index = if inline { index } else { index + 1 };
        if let Some((_, default, effective)) = slot {
            if let (Some(default), Some(effective), Some(argument)) =
                (default, effective, args.get(value_index))
            {
                let value = if inline {
                    argument.split_once('=').unwrap().1
                } else {
                    argument
                };
                if legacy_default(default).as_deref() == Some(Path::new(value)) {
                    args[value_index] = if inline {
                        format!("{flag}={}", effective.display())
                    } else {
                        effective.display().to_string()
                    };
                }
            }
            index = value_index + 1;
        } else {
            index += 1;
        }
    }
    relocation
}

/// The production rename, as a plain function so a test can inject its own.
fn rename_dir(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

/// [`relocate`] with the lock wait and the rename as parameters, for tests.
fn relocate_with(
    paths: Paths,
    enabled: [bool; 3],
    wait: Duration,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Relocation {
    let mut cache: Vec<(PathBuf, Outcome, PathBuf)> = Vec::new();
    let mut notes = Vec::new();
    let mut selected_slot = |dir: &Path, enabled: bool| {
        if enabled {
            slot(dir, wait, rename, &mut cache, &mut notes)
        } else {
            (Outcome::NotApplicable, dir.to_path_buf())
        }
    };
    let (config, config_dir) = selected_slot(paths.config_dir(), enabled[0]);
    let (data, data_dir) = selected_slot(paths.data_dir(), enabled[1]);
    let (state, state_dir) = selected_slot(paths.state_dir(), enabled[2]);
    Relocation {
        paths: Paths::new(
            config_dir,
            data_dir,
            state_dir,
            paths.runtime_dir().map(Path::to_path_buf),
        ),
        config,
        data,
        state,
        notes,
    }
}

/// One directory slot; `config` and `data` are the same directory on Windows, so
/// the work (and the outcome) is shared instead of running twice.
fn slot(
    dir: &Path,
    wait: Duration,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    cache: &mut Vec<(PathBuf, Outcome, PathBuf)>,
    notes: &mut Vec<Note>,
) -> (Outcome, PathBuf) {
    if let Some((_, outcome, effective)) = cache.iter().find(|(cached, _, _)| cached == dir) {
        return (*outcome, effective.clone());
    }
    let outcome = relocate_one(dir, wait, rename, notes);
    cache.push((dir.to_path_buf(), outcome.0, outcome.1.clone()));
    outcome
}

type OutcomeAndPath = (Outcome, PathBuf);

/// The whole decision for one directory. See the module documentation.
fn relocate_one(
    new_dir: &Path,
    wait: Duration,
    rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    notes: &mut Vec<Note>,
) -> OutcomeAndPath {
    let Some(parent) = new_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return (Outcome::NotApplicable, new_dir.to_path_buf());
    };
    if new_dir.file_name().and_then(|name| name.to_str()) != Some(APP_DIR) {
        return (Outcome::NotApplicable, new_dir.to_path_buf());
    }
    let legacy = parent.join(LEGACY_APP_DIR);
    let new_is_dir = new_dir.is_dir();
    let new_exists = new_dir.exists();
    let legacy_meta = std::fs::symlink_metadata(&legacy).ok();
    // A symlink is `is_symlink()`, not `is_dir()`: never follow, never move it.
    let legacy_is_dir = legacy_meta
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_dir());

    if legacy_meta.is_some() && !legacy_is_dir {
        notes.push(Note {
            severity: Severity::Warn,
            text: format!(
                "{} is not a directory; leaving it alone and using {}",
                legacy.display(),
                new_dir.display()
            ),
        });
        return (
            if new_is_dir {
                Outcome::BothPresent
            } else {
                Outcome::NothingLegacy
            },
            new_dir.to_path_buf(),
        );
    }

    if !legacy_is_dir {
        if new_exists && !new_is_dir {
            notes.push(Note {
                severity: Severity::Warn,
                text: format!("{} exists but is not a directory", new_dir.display()),
            });
        }
        let outcome = if new_is_dir {
            Outcome::AlreadyNew
        } else {
            Outcome::NothingLegacy
        };
        return (outcome, new_dir.to_path_buf());
    }

    if new_exists {
        if new_is_dir {
            notes.push(Note {
                severity: Severity::Warn,
                text: format!(
                    "both {} and the legacy {} exist; using {} and leaving {} untouched. \
                     Stop any older Bifrost version and check {} before removing it.",
                    new_dir.display(),
                    legacy.display(),
                    new_dir.display(),
                    legacy.display(),
                    legacy.display(),
                ),
            });
            return (Outcome::BothPresent, new_dir.to_path_buf());
        }
        notes.push(Note {
            severity: Severity::Warn,
            text: format!(
                "{} exists but is not a directory; continuing with the legacy {}",
                new_dir.display(),
                legacy.display()
            ),
        });
        return (Outcome::Deferred, legacy);
    }

    // Nothing visible yet, so take the lock and decide again under it: a sibling
    // process may have moved the directory while this one was starting.
    let _lock = match FileLock::acquire_within(&parent.join(LOCK_STEM), wait) {
        Ok(lock) => lock,
        Err(error) => {
            if new_dir.exists() {
                let outcome = if legacy.exists() {
                    Outcome::BothPresent
                } else {
                    Outcome::AlreadyNew
                };
                return (outcome, new_dir.to_path_buf());
            }
            notes.push(Note {
                severity: Severity::Warn,
                text: format!(
                    "could not lock {} for the app-directory move ({error}); continuing with {} \
                     and retrying on the next start",
                    parent.join(LOCK_STEM).display(),
                    legacy.display()
                ),
            });
            return (Outcome::Deferred, legacy);
        }
    };

    if new_dir.exists() {
        let outcome = if legacy.exists() {
            Outcome::BothPresent
        } else {
            Outcome::AlreadyNew
        };
        return (outcome, new_dir.to_path_buf());
    }

    match rename(&legacy, new_dir) {
        Ok(()) => {
            notes.push(Note {
                severity: Severity::Info,
                text: format!("moved {} to {}", legacy.display(), new_dir.display()),
            });
            (Outcome::Moved, new_dir.to_path_buf())
        }
        Err(error) => {
            if new_dir.exists() {
                return (Outcome::AlreadyNew, new_dir.to_path_buf());
            }
            notes.push(Note {
                severity: Severity::Warn,
                text: format!(
                    "could not move {} to {}: {error}. Continuing with {}; the next start retries",
                    legacy.display(),
                    new_dir.display(),
                    legacy.display()
                ),
            });
            (Outcome::Deferred, legacy)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bifrost-config-relocate-{}-{}-{label}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temporary directory");
        root
    }

    /// `config|data|state/bifrost` under `root`, as the platform default layout.
    fn app_paths(root: &Path) -> Paths {
        Paths::new(
            root.join("config").join(APP_DIR),
            root.join("data").join(APP_DIR),
            root.join("state").join(APP_DIR),
            None,
        )
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        std::fs::write(path, body).expect("file");
    }

    fn warnings(relocation: &Relocation) -> usize {
        relocation
            .notes
            .iter()
            .filter(|note| note.severity == Severity::Warn)
            .count()
    }

    #[test]
    fn mixed_overrides_migrate_only_the_remaining_default_slots() {
        for (flag, skipped) in [
            ("--config-dir", Some(0)),
            ("--data-dir", Some(1)),
            ("--state-dir", Some(2)),
            ("--runtime-dir", None),
        ] {
            let root = temp_root("mixed");
            let defaults = app_paths(&root);
            let dirs = [
                defaults.config_dir(),
                defaults.data_dir(),
                defaults.state_dir(),
            ];
            for dir in dirs {
                write(&legacy_default(dir).unwrap().join("kept"), "legacy");
            }
            // Even a directory named lan-workspace is arbitrary outside this slot's parent.
            let custom = root.join("custom").join(LEGACY_APP_DIR);
            write(&custom.join("kept"), "custom");
            let mut args = vec![flag.into(), custom.display().to_string(), "status".into()];
            let original = args.clone();
            let cli = crate::CliPaths::parse(&args);
            let report =
                prepare_arguments(defaults.clone(), &cli, &mut args, LOCK_WAIT, &rename_dir);
            assert_eq!(args, original);
            for (index, dir) in dirs.into_iter().enumerate() {
                assert_eq!(dir.join("kept").exists(), skipped != Some(index));
                assert_eq!(
                    legacy_default(dir).unwrap().join("kept").exists(),
                    skipped == Some(index)
                );
            }
            assert_eq!(
                std::fs::read_to_string(custom.join("kept")).unwrap(),
                "custom"
            );
            if skipped == Some(2) {
                assert_eq!(report.state, Outcome::NotApplicable);
            }
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn legacy_desktop_handoff_rewrites_all_path_flags_including_runtime() {
        for deferred in [false, true] {
            let root = temp_root("handoff");
            let paths = app_paths(&root);
            let defaults = Paths::new(
                paths.config_dir(),
                paths.data_dir(),
                paths.state_dir(),
                Some(root.join("runtime").join(APP_DIR)),
            );
            // Exact flag order emitted by v0.3.3 session/update.rs::desktop_args.
            let mut args = vec!["tray".into()];
            for (flag, dir) in [
                ("--config-dir", defaults.config_dir()),
                ("--data-dir", defaults.data_dir()),
                ("--state-dir", defaults.state_dir()),
                ("--runtime-dir", defaults.runtime_dir().unwrap()),
            ] {
                let legacy = legacy_default(dir).unwrap();
                write(&legacy.join("kept"), "legacy");
                args.extend([flag.into(), legacy.display().to_string()]);
            }
            let cli = crate::CliPaths::parse(&args);
            let rename = |source: &Path, target: &Path| {
                if deferred {
                    Err(std::io::ErrorKind::PermissionDenied.into())
                } else {
                    std::fs::rename(source, target)
                }
            };
            let report = prepare_arguments(defaults.clone(), &cli, &mut args, LOCK_WAIT, &rename);
            let rewritten = crate::CliPaths::parse(&args);
            assert_eq!(
                rewritten.config_dir.as_deref(),
                Some(report.paths.config_dir())
            );
            assert_eq!(rewritten.data_dir.as_deref(), Some(report.paths.data_dir()));
            assert_eq!(
                rewritten.state_dir.as_deref(),
                Some(report.paths.state_dir())
            );
            assert_eq!(rewritten.runtime_dir.as_deref(), defaults.runtime_dir());
            assert!(legacy_default(defaults.runtime_dir().unwrap())
                .unwrap()
                .join("kept")
                .exists());
            assert_eq!(
                report.config,
                if deferred {
                    Outcome::Deferred
                } else {
                    Outcome::Moved
                }
            );
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn root_disables_even_explicit_legacy_default_adoption() {
        let mut args = vec![
            "--root".into(),
            "isolated".into(),
            "--config-dir".into(),
            "lan-workspace".into(),
        ];
        let original = args.clone();
        assert!(adopt(&mut args).is_none());
        assert_eq!(args, original);
    }

    #[test]
    fn inline_legacy_default_flag_is_rewritten_without_changing_other_arguments() {
        let root = temp_root("inline");
        let defaults = app_paths(&root);
        let mut args = vec![
            format!(
                "--state-dir={}",
                legacy_default(defaults.state_dir()).unwrap().display()
            ),
            "run".into(),
            "--dry-inject".into(),
        ];
        let cli = crate::CliPaths::parse(&args);
        let report = prepare_arguments(defaults, &cli, &mut args, LOCK_WAIT, &rename_dir);
        assert_eq!(
            args[0],
            format!("--state-dir={}", report.paths.state_dir().display())
        );
        assert_eq!(&args[1..], &["run", "--dry-inject"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn moves_the_legacy_directory_and_its_contents() {
        let root = temp_root("moves");
        let old = root.join("config").join(LEGACY_APP_DIR);
        write(&old.join("config.toml"), "node_id = \"a\"\n");
        write(&old.join("logs").join("bifrost-core.log"), "kept\n");

        let relocation = relocate(app_paths(&root));

        assert_eq!(relocation.config, Outcome::Moved);
        assert_eq!(relocation.data, Outcome::NothingLegacy);
        assert_eq!(relocation.state, Outcome::NothingLegacy);
        assert!(!old.exists(), "the legacy directory is gone");
        assert_eq!(
            std::fs::read_to_string(relocation.paths.config_file()).expect("moved file"),
            "node_id = \"a\"\n"
        );
        assert!(relocation.paths.config_dir().join("logs").is_dir());
        assert_eq!(warnings(&relocation), 0);
        assert!(relocation
            .notes
            .iter()
            .any(|note| note.severity == Severity::Info));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_directory_without_legacy_is_untouched() {
        let root = temp_root("already-new");
        let new = root.join("config").join(APP_DIR);
        write(&new.join("config.toml"), "new\n");

        let relocation = relocate(app_paths(&root));

        assert_eq!(relocation.config, Outcome::AlreadyNew);
        assert_eq!(
            std::fs::read_to_string(new.join("config.toml")).expect("file"),
            "new\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn both_present_keeps_the_legacy_copy() {
        let root = temp_root("both");
        let new = root.join("config").join(APP_DIR);
        let old = root.join("config").join(LEGACY_APP_DIR);
        write(&new.join("config.toml"), "new\n");
        write(&old.join("config.toml"), "old\n");

        let relocation = relocate(app_paths(&root));

        assert_eq!(relocation.config, Outcome::BothPresent);
        assert_eq!(relocation.paths.config_dir(), new);
        assert_eq!(
            std::fs::read_to_string(old.join("config.toml")).expect("legacy kept"),
            "old\n"
        );
        assert_eq!(warnings(&relocation), 1, "the user must be told");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn shared_config_and_data_move_once() {
        let root = temp_root("shared");
        let shared_new = root.join("roaming").join(APP_DIR);
        let shared_old = root.join("roaming").join(LEGACY_APP_DIR);
        write(&shared_old.join("trust.toml"), "trust\n");
        let paths = Paths::new(
            shared_new.clone(),
            shared_new.clone(),
            root.join("state"),
            None,
        );

        let relocation = relocate(paths);

        assert_eq!(relocation.config, Outcome::Moved);
        assert_eq!(relocation.data, Outcome::Moved);
        assert_eq!(relocation.paths.config_dir(), shared_new);
        assert_eq!(relocation.paths.data_dir(), shared_new);
        assert!(!shared_old.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_root_layout_is_not_a_candidate() {
        let root = temp_root("root-layout");
        let legacy = root.join(LEGACY_APP_DIR);
        write(&legacy.join("config.toml"), "untouched\n");
        let paths = Paths::new(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            Some(root.join("run")),
        );

        let relocation = relocate(paths);

        assert_eq!(relocation.config, Outcome::NotApplicable);
        assert_eq!(relocation.data, Outcome::NotApplicable);
        assert_eq!(relocation.state, Outcome::NotApplicable);
        assert!(legacy.is_dir(), "a --root layout has no legacy sibling");
        assert_eq!(
            std::fs::read_to_string(legacy.join("config.toml")).expect("file"),
            "untouched\n"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_runtime_directory_is_never_migrated() {
        let root = temp_root("runtime");
        let runtime = root.join("run");
        write(&runtime.join(LEGACY_APP_DIR).join("gui.sock"), "");
        let paths = Paths::new(
            root.join("config").join(APP_DIR),
            root.join("data").join(APP_DIR),
            root.join("state").join(APP_DIR),
            Some(runtime.clone()),
        );

        let relocation = relocate(paths);

        assert_eq!(relocation.paths.runtime_dir(), Some(runtime.as_path()));
        assert!(runtime.join(LEGACY_APP_DIR).is_dir(), "left alone");
        assert!(!runtime.join(APP_DIR).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// What Windows does when a still-running 0.3.x process owns a file inside
    /// the legacy directory: the rename is refused. Injected, so the test does not
    /// depend on a sharing violation being reproducible on the host.
    #[test]
    fn a_refused_rename_keeps_using_the_legacy_directory() {
        let root = temp_root("refused");
        let old = root.join("config").join(LEGACY_APP_DIR);
        write(&old.join("config.toml"), "old\n");
        fn refuse(_: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }

        let relocation = relocate_with(app_paths(&root), [true; 3], LOCK_WAIT, &refuse);

        assert_eq!(relocation.config, Outcome::Deferred);
        assert_eq!(relocation.paths.config_dir(), old.as_path());
        assert!(old.join("config.toml").is_file());
        assert!(!root.join("config").join(APP_DIR).exists());
        assert_eq!(warnings(&relocation), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_held_lock_defers_to_the_legacy_directory() {
        let root = temp_root("locked");
        let old = root.join("config").join(LEGACY_APP_DIR);
        write(&old.join("config.toml"), "old\n");
        let parent = root.join("config");
        let _held = FileLock::acquire(&parent.join(LOCK_STEM)).expect("test holds the lock");

        let relocation = relocate_with(
            app_paths(&root),
            [true; 3],
            Duration::from_millis(50),
            &rename_dir,
        );

        assert_eq!(relocation.config, Outcome::Deferred);
        assert_eq!(relocation.paths.config_dir(), old.as_path());
        assert!(warnings(&relocation) >= 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_symlink_is_left_alone() {
        use std::os::unix::fs::symlink;
        let root = temp_root("symlink");
        let target = root.join("somewhere-else");
        std::fs::create_dir_all(&target).expect("target");
        std::fs::create_dir_all(root.join("config")).expect("config");
        symlink(&target, root.join("config").join(LEGACY_APP_DIR)).expect("symlink");

        let relocation = relocate(app_paths(&root));

        assert_ne!(relocation.config, Outcome::Moved);
        assert!(
            std::fs::symlink_metadata(root.join("config").join(LEGACY_APP_DIR))
                .expect("legacy symlink still exists")
                .file_type()
                .is_symlink()
        );
        assert_eq!(warnings(&relocation), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
