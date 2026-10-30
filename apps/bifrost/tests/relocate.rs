//! The one binary-level proof that a 0.4.0 start moves the pre-0.4.0 directory.
//!
//! Every other harness starts `bifrost` with `--root`, which never uses the
//! application directory name, so the startup move is inert there. This test
//! points the platform variables at a temporary profile, starts the real binary
//! without a path flag, and inspects the directories it left behind. The child
//! cannot reach a running core, so its exit status is not the assertion — the
//! filesystem is.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_profile(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "bifrost-relocate-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temporary profile");
    root
}

/// The platform roots the binary will resolve: XDG variables on Unix, `APPDATA`
/// and `LOCALAPPDATA` on Windows.
struct Roots {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
}

impl Roots {
    fn under(root: &Path) -> Self {
        #[cfg(windows)]
        {
            let roaming = root.join("roaming");
            Roots {
                config: roaming.clone(),
                data: roaming,
                state: root.join("local"),
            }
        }
        #[cfg(not(windows))]
        {
            Roots {
                config: root.join("config"),
                data: root.join("data"),
                state: root.join("state"),
            }
        }
    }

    fn legacy(&self, dir: &Path) -> PathBuf {
        dir.join("lan-workspace")
    }

    fn app(&self, dir: &Path) -> PathBuf {
        dir.join("bifrost")
    }
}

/// A child isolated from the user's profile by platform environment variables.
fn profile_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bifrost"));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        command.env("APPDATA", root.join("roaming"));
        command.env("LOCALAPPDATA", root.join("local"));
    }
    #[cfg(not(windows))]
    {
        command.env("HOME", root.join("home"));
        command.env("XDG_CONFIG_HOME", root.join("config"));
        command.env("XDG_DATA_HOME", root.join("data"));
        command.env("XDG_STATE_HOME", root.join("state"));
        command.env_remove("XDG_RUNTIME_DIR");
    }
    command
}

/// No core is required: status only exercises startup migration and path resolution.
fn run_in_profile(root: &Path) {
    let _ = profile_command(root)
        .arg("status")
        .status()
        .expect("bifrost starts");
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
    std::fs::write(path, body).expect("file");
}

fn cleanup(root: &Path) {
    if std::env::var_os("BIFROST_KEEP_ROOTS").is_none() {
        let _ = std::fs::remove_dir_all(root);
    } else {
        println!("profile kept at {}", root.display());
    }
}

#[test]
fn a_start_moves_the_legacy_profile_directory_once() {
    let root = temp_profile("moves");
    let roots = Roots::under(&root);
    write(&roots.legacy(&roots.config).join("config.toml"), "node\n");
    write(&roots.legacy(&roots.data).join("identity.key"), "key\n");
    write(
        &roots.legacy(&roots.state).join("logs").join("old.log"),
        "log\n",
    );

    run_in_profile(&root);

    assert!(
        roots.app(&roots.config).join("config.toml").is_file(),
        "config moved to {}",
        roots.app(&roots.config).display()
    );
    assert!(
        !roots.legacy(&roots.config).exists(),
        "the legacy config dir is gone"
    );
    assert!(roots.app(&roots.data).join("identity.key").is_file());
    assert!(roots
        .app(&roots.state)
        .join("logs")
        .join("old.log")
        .is_file());
    assert!(!roots.legacy(&roots.state).exists());

    // A second start is a no-op, not a second move.
    run_in_profile(&root);
    assert!(roots.app(&roots.config).join("config.toml").is_file());
    assert!(!roots.legacy(&roots.config).exists());

    cleanup(&root);
}

#[test]
fn a_start_never_merges_or_deletes_an_existing_new_directory() {
    let root = temp_profile("both");
    let roots = Roots::under(&root);
    write(&roots.app(&roots.config).join("config.toml"), "new\n");
    write(&roots.legacy(&roots.config).join("config.toml"), "old\n");

    run_in_profile(&root);

    assert_eq!(
        std::fs::read_to_string(roots.app(&roots.config).join("config.toml")).expect("new"),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(roots.legacy(&roots.config).join("config.toml")).expect("old kept"),
        "old\n"
    );

    cleanup(&root);
}

#[test]
fn partial_overrides_do_not_abandon_the_default_identity_and_config() {
    for flag in ["--state-dir", "--runtime-dir"] {
        let root = temp_profile("partial");
        let roots = Roots::under(&root);
        write(&roots.legacy(&roots.config).join("config.marker"), "config");
        write(
            &roots.legacy(&roots.data).join("identity.marker"),
            "identity",
        );
        write(&roots.legacy(&roots.state).join("state.marker"), "state");
        let custom = root.join("custom").join("lan-workspace");
        write(&custom.join("untouched"), "override");
        let _ = profile_command(&root)
            .arg(flag)
            .arg(&custom)
            .arg("status")
            .status()
            .expect("bifrost starts");
        assert!(roots.app(&roots.config).join("config.marker").is_file());
        assert!(roots.app(&roots.data).join("identity.marker").is_file());
        if flag == "--state-dir" {
            assert!(roots.legacy(&roots.state).join("state.marker").is_file());
            assert!(!roots.app(&roots.state).exists());
        } else {
            assert!(roots.app(&roots.state).join("state.marker").is_file());
        }
        assert_eq!(
            std::fs::read_to_string(custom.join("untouched")).unwrap(),
            "override"
        );
        cleanup(&root);
    }
}

#[test]
fn legacy_desktop_arguments_and_default_restart_use_the_same_identity() {
    let root = temp_profile("desktop-handoff");
    let roots = Roots::under(&root);
    write(&roots.legacy(&roots.config).join("config.marker"), "config");
    write(
        &roots.legacy(&roots.data).join("identity.marker"),
        "identity",
    );
    write(&roots.legacy(&roots.state).join("state.marker"), "state");
    // v0.3.3 apps/lws-core/src/session/update.rs::desktop_args emits these
    // separated pairs, in this order; Windows has no runtime pair by default.
    let mut args = Vec::new();
    for (flag, dir) in [
        ("--config-dir", &roots.config),
        ("--data-dir", &roots.data),
        ("--state-dir", &roots.state),
    ] {
        args.push(flag.to_owned());
        args.push(roots.legacy(dir).display().to_string());
    }
    #[cfg(not(windows))]
    args.extend([
        "--runtime-dir".into(),
        root.join("runtime")
            .join("lan-workspace")
            .display()
            .to_string(),
    ]);
    let mut handoff = profile_command(&root);
    let mut restart = profile_command(&root);
    #[cfg(not(windows))]
    {
        handoff.env("XDG_RUNTIME_DIR", root.join("runtime"));
        restart.env("XDG_RUNTIME_DIR", root.join("runtime"));
    }
    let first = handoff
        .args(&args)
        .arg("peers")
        .stdout(Stdio::piped())
        .output()
        .expect("handoff role starts");
    assert!(
        first.status.success(),
        "legacy handoff must load the adopted profile"
    );
    let second = restart
        .arg("peers")
        .stdout(Stdio::piped())
        .output()
        .expect("default core restart starts");
    assert!(second.status.success());
    assert_eq!(
        first.stdout, second.stdout,
        "both launches keep the same node identity"
    );
    assert!(!roots.legacy(&roots.config).exists());
    assert!(!roots.legacy(&roots.data).exists());
    assert!(!roots.legacy(&roots.state).exists());
    assert!(roots.app(&roots.config).join("config.marker").is_file());
    assert!(roots.app(&roots.data).join("identity.marker").is_file());
    assert!(roots
        .app(&roots.state)
        .join("logs")
        .join("bifrost-core.log")
        .is_file());
    cleanup(&root);
}
