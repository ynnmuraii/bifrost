//! AC-16: `bifrost-core run` writes `config.toml` itself when there is none, and still
//! turns its control channel on afterwards.
//!
//! The very first start on a fresh `--root` is the scenario: no `config/config.toml`,
//! a real daemon, a real agent, no user input. Everything is waited for with a
//! deadline, so a slow machine delays the test instead of failing it, and the numbers
//! are far below the fault-injection harness's.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The `bifrost` binary under test (the agent is the same executable).
fn core_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bifrost"))
}

/// A directory no other test shares.
fn unique_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
        "bifrost-first-run-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temporary directory must be creatable");
    dir
}

/// Waits for something the poll reports, with a deadline instead of a sleep.
fn wait_for<T>(what: &str, timeout: Duration, mut poll: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = poll() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One running daemon on a fresh root.
struct Core {
    root: PathBuf,
    log: PathBuf,
    child: Child,
}

impl Core {
    /// `bifrost-core --root <root> run`, dry like every other suite (no OS input on the
    /// runner) and with the agent next to the core binary.
    fn start(root: &Path) -> Core {
        let log = root.join("core.log");
        let out = std::fs::File::create(&log).expect("the log file must be creatable");
        let err = out.try_clone().expect("the log file must be clonable");
        let child = Command::new(core_exe())
            .arg("--root")
            .arg(root)
            .args(["run", "--dry-inject", "--dry-input-events", "4"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("the core starts");
        Core {
            root: root.to_path_buf(),
            log,
            child,
        }
    }

    /// One control-channel request; the retry loop covers the window where the daemon
    /// has not bound the channel yet.
    fn ctl(&self, request: &str) -> Option<String> {
        for _ in 0..20 {
            let output = Command::new(core_exe())
                .arg("--root")
                .arg(&self.root)
                .args(request.split(' '))
                .output()
                .expect("the CLI runs");
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if stdout.starts_with("OK") {
                return Some(stdout);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// What the daemon said so far, for the failure message.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Whether the daemon has already given up, so a wait fails at once with its
    /// log instead of burning the deadline.
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_first_run_writes_a_configuration_and_reaches_the_control_channel() {
    let root = unique_dir("first-run");
    let mut core = Core::start(&root);

    // The file appears by itself, before anyone asks for a status.
    let config_file = root.join("config").join("config.toml");
    let text = wait_for("config.toml", Duration::from_secs(30), || {
        if let Ok(read) = std::fs::read_to_string(&config_file) {
            return Some(read);
        }
        if core.has_exited() {
            panic!(
                "the core exited before writing config.toml:\n{}",
                core.log()
            );
        }
        None
    });

    // …and what it wrote is the documented first run, not a placeholder.
    let config = config::Config::from_str(&text).expect("the first-run text parses");
    config.validate().expect("the first-run document validates");
    assert_eq!(config.config_version, 2);
    assert!(!config.node.display_name.trim().is_empty());
    assert!(!config.network.listen_address.is_empty());
    assert!(config.node.can_own);
    // «Windows: true, ноутбуки: false» — the example's own comment.
    assert_eq!(
        config.node.claim_on_startup,
        cfg!(windows),
        "claim_on_startup follows the host, not the schema default"
    );
    // ADR-008: the channel is opt-in, so a first run leaves it off.
    assert!(!config.clipboard.enabled);
    match config::first_run::lan_addresses().first().copied() {
        Some(best) => assert_eq!(config.network.listen_address, best.to_string()),
        None => {
            // The documented escape hatch, and the only one validation accepts.
            assert_eq!(config.network.listen_address, "0.0.0.0");
            assert!(config.network.allow_any_interface);
        }
    }

    // The daemon is alive and answers its control channel with the file it just wrote.
    let status = wait_for("a status answer", Duration::from_secs(30), || {
        if let Some(line) = core.ctl("status") {
            return Some(line);
        }
        if core.has_exited() {
            panic!("the core exited before answering `status`:\n{}", core.log());
        }
        None
    });
    assert!(
        status.starts_with("OK "),
        "unexpected status answer: {status}"
    );
}
