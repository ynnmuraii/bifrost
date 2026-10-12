//! The control channel under load: two hundred sequential `status` calls over one core.
//!
//! `lws-core status` is the CLI's answer to «what is core doing», and it is one connection:
//! connect, write one line, read one line. `docs/03-architecture.md` promises the channel is
//! always there, so no call may come back without an answer — and one did: on Windows core
//! polls `GuiListener::accept` as a branch of its `select!`, the tick won the race a few times
//! in two hundred, and the client read a closed pipe («core closed the control channel without
//! an answer») even though core had served every other request. `ipc::transport::windows`
//! makes that accept cancel safe (`PendingInstance`); this test is the count that noticed.
//!
//! ```text
//! cargo test -p lws-core --test control_channel -- --nocapture
//! ```
//!
//! The core is a real process, the agent is its dry one, and every call is a real CLI process:
//! nothing here is a mock of the thing that was broken.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Calls to make. The failure that motivated the test showed up once every few dozen calls.
const CALLS: u32 = 200;
/// How long to wait for a fresh core to answer at all.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The workspace binary under test.
fn core_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lws-core"))
}

/// A port that is free right now, so a parallel test suite cannot collide with this one.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a probe socket")
        .local_addr()
        .expect("a bound address")
        .port()
}

/// Writes the configuration one dry core needs: one display, no links, this user only.
fn write_config(root: &Path, port: u16) {
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).expect("the config directory");
    let mut config = std::fs::File::create(config_dir.join("config.toml")).expect("config.toml");
    // `listen_address` is a concrete address; `0.0.0.0` would need `allow_any_interface`.
    let text = format!(
        r#"config_version = 2

[node]
display_name = "load"
can_own = true
claim_on_startup = true

[platform]
backend = "auto"

[network]
listen_address = "127.0.0.1"
port_input = {port}
port_clipboard = {}

[topology]
[[topology.displays]]
node = "self"
id = "dry-0"
logical = [0, 0, 1920, 1080]
primary = true
"#,
        port + 1,
    );
    config.write_all(text.as_bytes()).expect("the config");
}

/// Starts `lws-core run` for `root` and waits until it answers `status`.
fn start_core(root: &Path) -> Child {
    let mut child = Command::new(core_binary())
        .arg("--root")
        .arg(root)
        .args(["run", "--dry-inject"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning lws-core run");

    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if status_line(root).is_some() {
            return child;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("core did not answer a status call within {READY_TIMEOUT:?}");
}

/// One `status` call: `Some(line)` when core answered `OK …`, `None` when it did not.
fn status_line(root: &Path) -> Option<String> {
    let mut child = Command::new(core_binary())
        .arg("--root")
        .arg(root)
        .arg("status")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning lws-core status");
    let deadline = Instant::now() + CALL_TIMEOUT;
    loop {
        match child.try_wait().expect("waiting for the client") {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let output = child.wait_with_output().expect("the client's output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or_default().trim().to_owned();
    if output.status.success() && line.starts_with("OK ") {
        Some(line)
    } else {
        None
    }
}

/// Two hundred calls in a row, each one answered.
#[test]
fn two_hundred_sequential_status_calls_are_all_answered() {
    let root = std::env::temp_dir().join(format!("lws-control-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write_config(&root, free_port());

    let mut core = start_core(&root);
    let mut unanswered = Vec::new();
    let started = Instant::now();
    for call in 1..=CALLS {
        if status_line(&root).is_none() {
            unanswered.push(call);
        }
    }
    let elapsed = started.elapsed();

    let _ = core.kill();
    let _ = core.wait();
    let _ = std::fs::remove_dir_all(&root);

    println!(
        "{CALLS} calls in {elapsed:?} ({:.1} ms each); unanswered: {:?}",
        elapsed.as_secs_f64() * 1000.0 / f64::from(CALLS),
        unanswered,
    );
    assert!(
        unanswered.is_empty(),
        "core closed the control channel without an answer on calls {unanswered:?}"
    );
}

/// How many clients hammer the channel at once, and how many calls each one makes.
const CLIENTS: usize = 8;
/// Calls per client.
const CALLS_PER_CLIENT: u32 = 50;

/// Eight clients at once, fifty calls each, every call answered.
///
/// The sequential test above covers the cancelled accept; this one covers the *window* around
/// it, which is where the channel really loses clients: a named pipe has one instance per
/// waiting client, and a client that arrives while the listener has none — the instance it was
/// waiting on has just been handed to core, and the next one does not exist yet — reads
/// `ERROR_FILE_NOT_FOUND`, or `ERROR_PIPE_BUSY` when every instance is connected but none is
/// being served. Both are the listener's bookkeeping, not the client's mistake:
/// `ipc::transport::windows` creates the next instance *before* the connected one leaves
/// `accept`, and a client retries a busy or briefly-absent pipe for two seconds before giving
/// up. Under eight clients at once the old shape lost calls within a few dozen, so the counts
/// below are the regression.
#[test]
fn eight_concurrent_clients_get_fifty_answers_each() {
    let root = std::env::temp_dir().join(format!("lws-control-stress-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write_config(&root, free_port());

    let mut core = start_core(&root);
    let started = Instant::now();
    let unanswered: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..CLIENTS)
            .map(|_| {
                scope.spawn(|| {
                    (0..CALLS_PER_CLIENT)
                        .filter(|_| status_line(&root).is_none())
                        .count()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a client thread"))
            .sum()
    });
    let elapsed = started.elapsed();

    let _ = core.kill();
    let _ = core.wait();
    let _ = std::fs::remove_dir_all(&root);

    let total = CLIENTS as u32 * CALLS_PER_CLIENT;
    println!(
        "{CLIENTS} clients x {CALLS_PER_CLIENT} calls = {total} in {elapsed:?}; unanswered: {unanswered}"
    );
    assert_eq!(
        unanswered, 0,
        "{unanswered} of {total} concurrent calls came back without an answer"
    );
}
