//! The window's half, driven against two real cores.
//!
//! `cargo test -p lws-gui` never opens a window: the view model is a struct literal and
//! `apply` is a pure fold, which is what the rest of the suite checks. This file is the other
//! half of that claim — the same [`Bridge`] the window owns (`apps/lws-gui/src/app.rs`), two
//! real `lws-core run` processes with the dry agent, and no display, no GPU and no network
//! beyond loopback. It is the GUI's counterpart of `apps/lws-core/tests/gui_channel.rs`: that
//! one speaks `GuiFrame` to two cores and asserts the contract; this one drives the *window*
//! — `boot`, the client thread with its own 2 s reconnect loop, `dispatch` and `pump` — and
//! asserts what the owner would see on the four screens.
//!
//! What it covers, in the order the owner meets them:
//!
//! * the status screen of a working pair: «Служба работает», both computers in the
//!   status and in the layout document, this computer owning input;
//! * the owner's first layout Apply after restoring revision 1: the link reaches both windows,
//!   listing pending edits leaves it applied, and a stale follower edit is refused;
//! * the pairing wizard: the address core pushes, the same six digits on both screens, both
//!   users confirming, both `trust.toml` records are written and the live link appears in each
//!   window without restarting either core;
//! * «Взять управление» on the follower, watched on its own status screen.
//!
//! Every wait is event-driven with a deadline and a message naming what was seen, never a
//! sleep that is hoped to be long enough.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use lws_gui::view_model::{boot_with_program, stop_core, Action, Bridge, CliArgs, ViewModel};

/// How long any one state the window has to reach may take.
///
/// Two cores, two agents and a TLS handshake on a shared CI runner: the same 20 s
/// `lws-core`'s own harness allows, and a wait that runs out says what the window showed.
const DEADLINE: Duration = Duration::from_secs(20);

/// How long between two frames while waiting. The window redraws every 250 ms.
const FRAME: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------- one test at a time

/// Serialises the scenarios in this binary.
///
/// Each one runs two cores and two agents, and a two-core runner would otherwise have four
/// processes per test at once. A test that panics while holding the lock must not poison the
/// rest of the run.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------- the binary

/// The `multideskflow` executable, which is every role: the daemon these tests start, its
/// agent, and the one the window itself starts through `boot_with_program`.
///
/// This test's own executable is `target/debug/deps/cores-<hash>` (`.exe` on Windows), so
/// `target/debug` is two levels up — the same place `cargo build --workspace` puts it. (Only
/// the package that owns a binary gets `CARGO_BIN_EXE_*`, and that is not this one.)
fn multideskflow() -> PathBuf {
    let exe = std::env::current_exe().expect("this test's own path");
    let directory = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/debug above target/debug/deps");
    let path = directory.join(if cfg!(windows) {
        "multideskflow.exe"
    } else {
        "multideskflow"
    });
    assert!(
        path.is_file(),
        "{} is missing: run `cargo build --workspace` first — it is what puts `multideskflow` \
         in target/debug, next to this test",
        path.display()
    );
    path
}

/// A directory no other test of this run shares, under the system temporary directory.
fn unique_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
        "lws-gui-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

/// `count` ports nothing is listening on.
///
/// The listeners are held open until the last port is read, so two calls cannot hand back the
/// same number: a node whose `port_input` and `port_clipboard` collide fails to bind its
/// second listener, and the suite would fail for a reason that has nothing to do with the
/// window.
fn free_ports(count: usize) -> Vec<u16> {
    let listeners: Vec<TcpListener> = (0..count)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("a free port"))
        .collect();
    listeners
        .iter()
        .map(|listener| listener.local_addr().expect("local address").port())
        .collect()
}

// ---------------------------------------------------------------- two cores on loopback

/// One running `lws-core run`, with the dry agent, killed when the test is done with it.
struct Core {
    root: PathBuf,
    log: PathBuf,
    child: Child,
}

impl Core {
    /// Starts the daemon with its own log beside its own root.
    fn start(root: PathBuf, label: &str, report_displays: bool) -> Core {
        let log = root.join(format!("{label}.log"));
        let out = std::fs::File::create(&log).expect("log file");
        let err = out.try_clone().expect("log file");
        let mut command = Command::new(multideskflow());
        command
            .arg("--root")
            .arg(&root)
            .args(["run", "--dry-inject", "--dry-input-events", "4"]);
        if report_displays {
            command.arg("--dry-displays");
        }
        let child = command
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("lws-core starts");
        Core { root, log, child }
    }

    /// Waits until an empty agent report has already driven a topology rebuild.
    fn wait_for_empty_report_rebuild(&self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let log = std::fs::read_to_string(&self.log).expect("core log");
            if let Some(report) = log.find("agent display report displays=0") {
                let after_report = &log[report..];
                let assembled = after_report.find("layout document assembled");
                let unusable = after_report.find("layout document unusable");
                if assembled.is_some() || unusable.is_some() {
                    assert!(
                        assembled.is_some_and(|assembled| {
                            unusable.is_none_or(|unusable| assembled < unusable)
                        }),
                        "an empty display report discarded the configured topology:\n{after_report}"
                    );
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the core did not report/rebuild its empty display set:\n{log}"
            );
            std::thread::sleep(FRAME);
        }
    }

    /// Kills the daemon and reaps it. Idempotent, so `Drop` may run after a caller stopped it.
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Two cores with their own roots, their own ports and their own config directories.
struct Lab {
    dir: PathBuf,
    a: Core,
    b: Core,
    a_id: String,
    b_id: String,
}

impl Lab {
    /// Two cores that are already paired, the way a working pair looks: the left computer
    /// claims on startup and owns input, the right one may own but does not claim.
    fn linked(label: &str) -> Lab {
        Lab::start(label, true, false, true)
    }
    /// A linked pair whose dry agents report no displays, exercising the configured fallback.
    fn linked_without_agent_displays(label: &str) -> Lab {
        Lab::start(label, true, false, false)
    }

    /// The coordinator restored revision 1; the follower starts with no applied document.
    fn restored_owner_layout(label: &str) -> Lab {
        Lab::start(label, true, true, true)
    }

    /// Two cores with no trust at all — what the pairing wizard starts from.
    fn unpaired(label: &str) -> Lab {
        Lab::start(label, false, false, true)
    }

    fn start(label: &str, pair_first: bool, owner_only_layout: bool, report_displays: bool) -> Lab {
        let dir = unique_dir(label);
        let a_root = dir.join("a");
        let b_root = dir.join("b");
        for root in [&a_root, &b_root] {
            std::fs::create_dir_all(root.join("config")).expect("config directory");
        }
        let ports = free_ports(4);
        write_config(&a_root, ports[0], ports[1], "left", true);
        write_config(&b_root, ports[2], ports[3], "right", false);
        if pair_first {
            // The CLI's own ceremony, before either core runs: a link is only ever dialled
            // from the peers read at startup (`ManagerConfig.peers`), so a pair that has to be
            // linked has to be paired first.
            pair(&a_root, &b_root, ports[0], ports[2]);
        }
        // `peers` loads-or-creates the identity, so the ids are the ones the cores will use.
        let a_id = node_id(&a_root);
        let b_id = node_id(&b_root);
        if pair_first {
            if owner_only_layout {
                write_topology(&a_root, &b_id, false);
                write_topology_revision(&b_root, &a_id, false, 0, "", 0, "");
            } else {
                write_topology(&a_root, &b_id, true);
                write_topology(&b_root, &a_id, true);
            }
        }
        Lab {
            dir,
            a: Core::start(a_root, "a", report_displays),
            b: Core::start(b_root, "b", report_displays),
            a_id,
            b_id,
        }
    }

    fn a_root(&self) -> &Path {
        &self.a.root
    }

    fn b_root(&self) -> &Path {
        &self.b.root
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // The daemons hold their own logs open, so they go before the directory can on
        // Windows. The directory itself goes only when the test passed: a failure keeps its
        // logs, and the message a reader gets points at them.
        self.a.stop();
        self.b.stop();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// The base configuration of one node, in the shape `lws-core`'s own harness writes.
fn write_config(root: &Path, port: u16, clipboard_port: u16, name: &str, claim_on_startup: bool) {
    let document = format!(
        "config_version = 2\n\
         \n\
         [node]\n\
         display_name = \"{name}\"\n\
         can_own = true\n\
         claim_on_startup = {claim_on_startup}\n\
         \n\
         [network]\n\
         listen_address = \"127.0.0.1\"\n\
         port_input = {port}\n\
         port_clipboard = {clipboard_port}\n\
         max_members = 3\n\
         \n\
         [input]\n\
         require_emergency_hotkey = false\n\
         \n\
         [log]\n\
         level = \"info\"\n",
    );
    std::fs::write(root.join("config/config.toml"), document).expect("configuration");
}

/// A revision-one workspace with one display per computer and an optional shared edge.
fn write_topology(root: &Path, peer: &str, with_link: bool) {
    write_topology_revision(root, peer, with_link, 1, "self", 1, "self");
}

/// A workspace snapshot whose applied pair is explicit; revision zero still seeds both displays.
fn write_topology_revision(
    root: &Path,
    peer: &str,
    with_link: bool,
    revision: u64,
    author: &str,
    max_seen_revision: u64,
    max_seen_author: &str,
) {
    let path = root.join("config/config.toml");
    let mut document = std::fs::read_to_string(&path).expect("configuration");
    document.push_str(&format!(
        "\n\
         [topology]\n\
         revision = {revision}\n\
         author = \"{author}\"\n\
         max_seen_revision = {max_seen_revision}\n\
         max_seen_author = \"{max_seen_author}\"\n\
         \n\
         [[topology.displays]]\n\
         node = \"self\"\n\
         id = \"dry-0\"\n\
         logical = [0.0, 0.0, 1920.0, 1080.0]\n\
         primary = true\n\
         \n\
         [[topology.displays]]\n\
         node = \"peer:{peer}\"\n\
         id = \"dry-0\"\n\
         logical = [0.0, 0.0, 1920.0, 1080.0]\n\
         primary = true\n"
    ));
    if with_link {
        document.push_str(&format!(
            "\n\
             [[topology.links]]\n\
             id = \"right-to-left\"\n\
             bidirectional = true\n\
             \n\
             [topology.links.from]\n\
             node = \"self\"\n\
             display = \"dry-0\"\n\
             edge = \"right\"\n\
             start = 0.0\n\
             end = 1080.0\n\
             \n\
             [topology.links.to]\n\
             node = \"peer:{peer}\"\n\
             display = \"dry-0\"\n\
             edge = \"left\"\n\
             start = 0.0\n\
             end = 1080.0\n"
        ));
    }
    std::fs::write(&path, document).expect("topology document");
}

/// Pairs two roots through the CLI's own ceremony, answering «y» on both sides.
///
/// `apps/lws-core/tests/common/mod.rs::pair` is the same sequence, and that module is in
/// another package, so it cannot be imported: the opener prints the address and waits, the
/// dialer types it, and each side answers «yes» to the six digits. The wait for the opener's
/// own line is deliberate — a fixed pause loses that race on a loaded machine, and the dialer
/// then arrives before the listener is up.
fn pair(a_root: &Path, b_root: &Path, a_port: u16, b_port: u16) {
    let listen = format!("127.0.0.1:{b_port}");
    let mut opener = Command::new(multideskflow())
        .arg("--root")
        .arg(b_root)
        .args(["pair", "--open", "--listen", listen.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the opener starts");
    opener
        .stdin
        .take()
        .expect("the opener's stdin")
        .write_all(b"y\n")
        .expect("the opener is answered");

    let opener_stdout = opener.stdout.take().expect("the opener's stdout");
    let opened = Arc::new(Mutex::new(String::new()));
    let (announced_tx, announced_rx) = std::sync::mpsc::channel();
    let reader = {
        let text = Arc::clone(&opened);
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(opener_stdout).lines() {
                let Ok(line) = line else { break };
                let mut collected = text.lock().expect("the opener's output");
                collected.push_str(&line);
                collected.push('\n');
                drop(collected);
                if line.contains("waiting for a peer to dial") {
                    let _ = announced_tx.send(());
                }
            }
        })
    };
    announced_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the pairing window must open before the other side dials it");

    let dial = format!("127.0.0.1:{b_port}");
    let mut dialer = Command::new(multideskflow())
        .arg("--root")
        .arg(a_root)
        .args(["pair", "--dial", dial.as_str()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the dialer starts");
    dialer
        .stdin
        .take()
        .expect("the dialer's stdin")
        .write_all(b"y\n")
        .expect("the dialer is answered");

    let dialed = dialer.wait_with_output().expect("the dialer finishes");
    let status = opener.wait().expect("the opener finishes");
    let _ = reader.join();
    let text = format!(
        "{}{}",
        opened.lock().expect("the opener's output"),
        String::from_utf8_lossy(&dialed.stdout)
    );
    assert!(
        text.matches("paired with").count() == 2,
        "both sides must pair:\n{text}\n{}",
        String::from_utf8_lossy(&dialed.stderr)
    );
    assert!(status.success(), "the opener must exit cleanly");

    // Both records carry an address a restart can dial: the dialer typed the opener's, and
    // the opener learned the dialer's from `PairHello.listen_port` — the port the dialer
    // really listens on, not the documented default.
    let a_trust = std::fs::read_to_string(a_root.join("config/trust.toml")).expect("trust");
    let b_trust = std::fs::read_to_string(b_root.join("config/trust.toml")).expect("trust");
    assert!(
        a_trust.contains(&format!("127.0.0.1:{b_port}")),
        "the dialer must record the opener's port:\n{a_trust}"
    );
    assert!(
        b_trust.contains(&format!("127.0.0.1:{a_port}")),
        "the opener must record the dialer's own port:\n{b_trust}"
    );
}

/// One node's own id, from the `peers` subcommand (`node <64 hex>`).
fn node_id(root: &Path) -> String {
    let output = Command::new(multideskflow())
        .arg("--root")
        .arg(root)
        .arg("peers")
        .output()
        .expect("`lws-core peers` runs");
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("node ").map(str::trim).map(str::to_owned))
        .unwrap_or_else(|| panic!("no node id in `lws-core peers`:\n{text}"))
}

/// One control-channel request to a running core, retried as that CLI is retried.
///
/// The reply is one line (`OK focus=… members=… owner=…`); a failure to read it is a
/// transport flake, not an FSM one.
fn ctl(root: &Path, request: &str) -> String {
    let mut last = String::new();
    for attempt in 0..5 {
        let output = Command::new(multideskflow())
            .arg("--root")
            .arg(root)
            .args(request.split(' '))
            .output()
            .expect("the control channel answers");
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if stdout.starts_with("OK ") {
            return stdout;
        }
        last = if stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
        } else {
            stdout
        };
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    panic!(
        "`lws-core {request}` never answered for {}: {last}",
        root.display()
    );
}

/// How many links a running core holds, from its own control channel.
///
/// This is the number the window's `Status.members` carries, read from the process instead of
/// from the window's copy of it.
fn links(root: &Path) -> usize {
    let reply = ctl(root, "status");
    reply
        .split(' ')
        .find_map(|token| token.strip_prefix("members="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no members= in `lws-core status`: {reply}"))
}

// ---------------------------------------------------------------- the window

/// A window on `root`, built exactly as `GuiApp::new` builds it.
///
/// `boot` resolves `config/`, `data/`, `state/` and `run/` from `--root`, finds the GUI
/// endpoint of that core and opens the client thread with its own 2 s reconnect loop; the
/// `Bridge` is what the window then owns and draws.
fn bridge_for(root: &Path) -> Bridge {
    let (view, client) = boot_with_program(
        CliArgs::parse(["--root".to_owned(), root.display().to_string()]),
        multideskflow(),
    );
    Bridge::new(view, client)
}

/// Pumps the window until `ready` answers, or fails saying what the window showed.
///
/// One iteration is one frame: drain the client, age the request deadlines, look at the
/// state. Nothing here sleeps and hopes — a wait has a deadline and a message.
fn wait_for<T>(
    bridge: &mut Bridge,
    what: &str,
    mut ready: impl FnMut(&ViewModel) -> Option<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    loop {
        bridge.pump();
        bridge.view_mut().tick(Instant::now());
        if let Some(value) = ready(bridge.view()) {
            return value;
        }
        if Instant::now() >= deadline {
            let view = bridge.view();
            panic!(
                "{what}: not within {DEADLINE:?}. The window showed: endpoint={:?}, \
                 connection={:?}, status={:?}, pairing={:?}, outcome={:?}, log={:?}",
                view.endpoint, view.connection, view.status, view.pairing, view.outcome, view.log,
            );
        }
        std::thread::sleep(FRAME);
    }
}

/// How many of the window's log lines mention `needle`.
fn count_lines(view: &ViewModel, needle: &str) -> usize {
    view.log.iter().filter(|line| line.contains(needle)).count()
}

/// A file's text, once the process that owns it has put the expected content there.
///
/// A core writes `trust.toml` through `config::save::save_atomic`, which renames the old file
/// to `.bak` before the new one takes its place, and the window's `OpResult` for
/// `PairingAnswer` can reach the test before that write is on disk. So a single read can land
/// in the gap between the two renames, or before the write at all, and neither is a failure of
/// anything this test is about: the wait is bounded, and a wait that runs out says what the
/// file held.
fn read_when_ready(path: &Path, what: &str, ready: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + DEADLINE;
    loop {
        // What the file held, for the panic below: the read itself is the loop's only work,
        // so there is nothing to carry between turns.
        let last = match std::fs::read_to_string(path) {
            Ok(text) if ready(&text) => return text,
            Ok(text) => format!("it holds:\n{text}"),
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            panic!(
                "{what}: {} never held it within {DEADLINE:?} — {last}",
                path.display()
            );
        }
        std::thread::sleep(FRAME);
    }
}

/// Why this host cannot own input, when it cannot.
///
/// A host with no session at all — a headless Linux runner — has no X display and no Wayland
/// compositor, so the agent advertises no capabilities, core refuses to take ownership, and
/// the status screen's own «Ограничения и деградации» block says so. That is the product
/// behaving correctly on a machine that cannot be a KVM, and it is not what this test is
/// about: it is reported, with what to do about it, instead of being asserted away.
fn missing_session(status: &protocol::ipc::Status) -> Option<&'static str> {
    let absent = |bit: &str| {
        status
            .capabilities_unavailable
            .iter()
            .any(|name| name == bit)
    };
    (absent("CAPTURE") || absent("INJECT")).then_some(
        "this host advertises no capture/inject capability, so no node may own input here: a \
         headless Linux runner has no session, and `.github/workflows/ci.yml` runs this suite \
         under `xvfb-run` for exactly this reason",
    )
}

// ---------------------------------------------------------------- the scenarios

/// The status screen of a working pair, built from what core pushes and nothing else.
#[test]
fn the_status_screen_holds_the_linked_workspace() {
    let _serial = serial();
    let lab = Lab::linked_without_agent_displays("status");
    // Make the empty dry-agent report happen before the GUI connects, so this exercises the
    // snapshot fallback rather than depending on which startup message wins the race.
    lab.a.wait_for_empty_report_rebuild();
    lab.b.wait_for_empty_report_rebuild();
    let mut window = bridge_for(lab.a_root());

    // The client thread connects on its own, and a fresh link asks for `Status` **and**
    // `TopologyDoc` (§4 of the build spec): nothing here sends a request to get them.
    wait_for(&mut window, "the window to reach the core", |view| {
        (view.connection_label() == "Служба работает").then_some(())
    });

    // «Подключиться снова» is one of the two buttons that need no request of their own: it
    // goes through the client half, which drops the link and dials again.
    let before = count_lines(window.view(), "Служба работает");
    assert!(
        window.client().connect_again(),
        "the client thread is alive"
    );
    wait_for(&mut window, "the client to dial the core again", |view| {
        (count_lines(view, "Служба работает") > before).then_some(())
    });

    // Peer formation precedes the asynchronous startup claim. A linked follower snapshot is
    // valid intermediate state, not the status this scenario is asserting.
    let status = wait_for(
        &mut window,
        "the linked workspace's startup ownership",
        |view| {
            view.status
                .as_ref()
                .filter(|status| {
                    !status.members.is_empty()
                        && (status.is_owner || missing_session(status).is_some())
                })
                .cloned()
        },
    );
    assert!(
        status.connected,
        "two linked cores are a workspace: {status:?}"
    );
    assert_eq!(status.members.len(), 1, "one link, one member: {status:?}");
    assert_eq!(status.members[0].node_id.as_str(), lab.b_id.as_str());
    assert!(status.members[0].online, "a member of `members()` is up");
    // The name is the non-authoritative label the peer advertised
    // (`docs/05-protocol.md`: «Название устройства/адрес не являются идентичностью»), so it
    // is asserted by shape and not by value — it comes from the other core's `PairHello`.
    assert!(
        !status.members[0].display_name.is_empty(),
        "a member carries a name: {status:?}"
    );
    assert_eq!(status.focus, "LOCAL", "nothing is remote yet");
    if let Some(why) = missing_session(&status) {
        panic!("this computer does not own input: {why} ({status:?})");
    }
    assert!(
        status.is_owner,
        "the left computer claims on startup: {status:?}"
    );
    assert_eq!(status.role, "owner");

    // The document arrived with the status: the editor holds both computers, each with the
    // display its configuration declares.
    let nodes = wait_for(&mut window, "the document of the workspace", |view| {
        (view.editor.nodes.len() == 2).then(|| view.editor.nodes.clone())
    });
    let ids: Vec<&str> = nodes.iter().map(|node| node.node_id.as_str()).collect();
    assert!(
        ids.contains(&lab.a_id.as_str()) && ids.contains(&lab.b_id.as_str()),
        "the document must hold both computers: {ids:?}"
    );
    assert!(
        nodes.iter().all(|node| !node.displays.is_empty()),
        "every member of the document has a display: {nodes:?}"
    );
}

/// The owner's restored revision is the CAS base for the first edit from the layout screen.
#[test]
fn the_first_owner_layout_apply_reaches_both_windows() {
    let _serial = serial();
    let lab = Lab::restored_owner_layout("layout-first-apply");
    let mut owner = bridge_for(lab.a_root());
    let mut follower = bridge_for(lab.b_root());

    let owner_status = wait_for(&mut owner, "the owner to restore revision 1", |view| {
        view.status
            .as_ref()
            .filter(|status| status.connected && status.is_owner && status.layout_revision == 1)
            .cloned()
    });
    let follower_status = wait_for(&mut follower, "the follower to adopt revision 1", |view| {
        view.status
            .as_ref()
            .filter(|status| {
                status.connected
                    && !status.is_owner
                    && status.owner_node_id == lab.a_id
                    && status.layout_revision == 1
            })
            .cloned()
    });
    assert_eq!(owner_status.owner_node_id, lab.a_id);
    assert_eq!(follower_status.layout_author, owner_status.layout_author);

    wait_for(
        &mut owner,
        "the owner's initial two-display document",
        |view| (view.editor.nodes.len() == 2).then_some(()),
    );
    wait_for(
        &mut follower,
        "the follower's initial two-display document",
        |view| (view.editor.nodes.len() == 2).then_some(()),
    );
    for (name, view) in [("owner", owner.view()), ("follower", follower.view())] {
        assert_eq!(view.editor.revision, 1, "{name} editor: {:?}", view.editor);
        assert!(
            view.editor.links.is_empty(),
            "{name} starts with no edge: {:?}",
            view.editor.links
        );
    }

    let stale_follower_request = {
        let view = follower.view();
        Action::UpdateTopology {
            layout_toml: view.layout_toml().into_bytes(),
            expected_revision: view.editor.revision,
            expected_author: lws_gui::view_model::node_id_bytes(&view.editor.author),
        }
    };

    // Only a map drag changes this draft; the document is sent on explicit Apply.
    let target = {
        let editor = &owner.view().editor;
        let this = editor
            .nodes
            .iter()
            .position(|node| node.node_id == lab.a_id)
            .expect("local node");
        let peer = editor
            .nodes
            .iter()
            .position(|node| node.node_id == lab.b_id)
            .expect("peer node");
        lws_gui::map::adjacent(
            &editor.shapes[this],
            editor.placement[this],
            &editor.shapes[peer],
            lws_gui::view_model::Side::Right,
        )
    };
    owner.dispatch(
        Action::MoveNode {
            node_id: lab.b_id.clone(),
            x: target.x,
            y: target.y,
        },
        Instant::now(),
    );
    let expected_author = {
        let view = owner.view();
        let request = Action::UpdateTopology {
            layout_toml: view.layout_toml().into_bytes(),
            expected_revision: view.editor.revision,
            expected_author: lws_gui::view_model::node_id_bytes(&view.editor.author),
        };
        let Action::UpdateTopology { layout_toml, .. } = &request else {
            unreachable!();
        };
        let parsed: config::TopologyConfig =
            toml::from_str(std::str::from_utf8(layout_toml).expect("text"))
                .expect("the layout editor emits a topology document");
        assert_eq!(
            parsed.links.len(),
            1,
            "first Apply creates the GUI edge; self={:?}, other={:?}, displays={:?}",
            view.self_node(),
            view.editor
                .nodes
                .iter()
                .map(|node| &node.node_id)
                .collect::<Vec<_>>(),
            parsed.displays
        );
        lws_gui::view_model::node_id_bytes(&view.editor.author)
    };
    assert_eq!(
        expected_author,
        lws_gui::view_model::node_id_bytes(&lab.a_id)
    );

    owner.view_mut().outcome = None;
    owner.dispatch(Action::DropNode(lab.b_id.clone()), Instant::now());
    wait_for(&mut owner, "the first owner drop to be saved", |view| {
        (view.editor.revision == 2 && view.editor.save.sent.is_none()).then_some(())
    });

    let owner_status = wait_for(&mut owner, "the owner to apply revision 2", |view| {
        view.status
            .as_ref()
            .filter(|status| status.layout_revision == 2)
            .cloned()
    });
    assert_eq!(owner_status.layout_author, follower_status.layout_author);
    assert_eq!(
        owner.view().editor.links.len(),
        1,
        "the owner's applied document must contain the first edge: {:?}",
        owner.view().editor
    );
    let follower_doc = wait_for(
        &mut follower,
        "the follower to receive revision 2 and its edge",
        |view| {
            (view.editor.revision == 2 && !view.editor.links.is_empty())
                .then(|| view.editor.clone())
        },
    );
    assert!(
        owner.view().editor.save.sent.is_none(),
        "the accepted document leaves no layout save awaiting a verdict"
    );
    assert_eq!(owner.view().editor.revision, 2);
    assert_eq!(owner.view().editor.links.len(), 1);
    for root in [lab.a_root(), lab.b_root()] {
        let saved = config::Config::load(root.join("config/config.toml"))
            .expect("the applied document remains valid configuration");
        assert_eq!(saved.topology.revision, 2);
        assert_eq!(saved.topology.links.len(), 1);
    }
    follower.view_mut().outcome = None;
    follower.dispatch(stale_follower_request, Instant::now());
    wait_for(
        &mut follower,
        "the owner to refuse the stale follower edit",
        |view| view.outcome.as_ref().filter(|outcome| !outcome.ok).cloned(),
    );
    let refusal = follower
        .view()
        .outcome
        .as_ref()
        .expect("the refusal is shown");
    assert!(
        !refusal.ok,
        "the stale follower edit is refused: {refusal:?}"
    );
    assert_eq!(owner.view().editor.revision, 2);
    assert_eq!(owner.view().editor.links.len(), 1);
    assert_eq!(follower.view().editor.revision, 2);
    assert_eq!(follower.view().editor.links.len(), 1);
    assert_eq!(follower_doc.links.len(), 1, "{follower_doc:?}");
    assert_eq!(follower_doc.revision, owner.view().editor.revision);
}

/// The pairing wizard, driven through the view model, between two unpaired cores.
#[test]
fn the_pairing_wizard_pairs_two_cores_through_the_view_model() {
    let _serial = serial();
    let lab = Lab::unpaired("pairing");
    let mut left = bridge_for(lab.a_root());
    let mut right = bridge_for(lab.b_root());

    // Both windows are on their own core before the wizard starts.
    wait_for(&mut left, "the left window to reach its core", |view| {
        (view.connection_label() == "Служба работает").then_some(())
    });
    wait_for(&mut right, "the right window to reach its core", |view| {
        (view.connection_label() == "Служба работает").then_some(())
    });

    // 1. «Открыть окно сопряжения» on the left computer. The address is core's own — the
    //    window cannot guess it, which is why it arrives as a push.
    left.dispatch(Action::StartPairing { window_s: 120 }, Instant::now());
    let address = wait_for(
        &mut left,
        "the address to type on the other computer",
        |view| {
            view.pairing
                .window
                .as_ref()
                .map(|window| window.address.clone())
        },
    );
    assert!(address.starts_with("127.0.0.1:"), "{address}");

    // 2. «Подключиться по адресу» on the right one.
    right.dispatch(Action::PairWith(address), Instant::now());

    // 3. Both screens show six digits, and they are the same six.
    // PairWith's success only acknowledges reaching the prompt. Core may deliver that
    // reply before or after the pushed code; neither order completes the ceremony or
    // removes the code before the users answer.
    let left_code = wait_for(&mut left, "the left screen's six digits", |view| {
        view.pairing.code()
    });
    let right_code = wait_for(&mut right, "the right screen's six digits", |view| {
        view.pairing.code()
    });
    assert_eq!(left_code, right_code, "the two screens must agree");
    assert_eq!(
        left_code.replace(' ', "").len(),
        6,
        "six digits: {left_code}"
    );
    assert!(
        left_code
            .chars()
            .all(|digit| digit.is_ascii_digit() || digit == ' '),
        "the code is digits, grouped: {left_code}"
    );
    // The prompt names the other computer. The name is a label, not an identity
    // (`docs/05-protocol.md`), and it is the peer's own `PairHello.display_name`: asserted by
    // shape here, because what the *window* must get right is the code below.
    let peer_name = |view: &ViewModel| {
        view.pairing
            .prompt
            .as_ref()
            .map(|prompt| prompt.peer_name.clone())
    };
    for (view, side) in [(&left, "left"), (&right, "right")] {
        let name = peer_name(view.view());
        assert!(
            name.as_deref().is_some_and(|name| !name.is_empty()),
            "the {side} screen must name the other computer: {name:?}"
        );
    }
    let left_attempt = left
        .view()
        .pairing
        .attempt_id()
        .expect("the prompt carries its attempt");
    let right_attempt = right
        .view()
        .pairing
        .attempt_id()
        .expect("the prompt carries its attempt");
    assert_eq!(left_attempt.len(), 16, "an attempt_id is 16 bytes");

    // 4. Both users confirm. A ceremony has no verdict until **both** sides answered, and the
    //    answer to `PairingAnswer` *is* that verdict — so both answers are dispatched before
    //    either is waited for. The bridge is fire-and-forget: dispatching never blocks, which
    //    is exactly how two windows on two computers behave.
    left.dispatch(
        Action::Answer {
            attempt_id: left_attempt,
            approve: true,
        },
        Instant::now(),
    );
    right.dispatch(
        Action::Answer {
            attempt_id: right_attempt,
            approve: true,
        },
        Instant::now(),
    );

    let verdict = |view: &ViewModel| {
        view.outcome
            .as_ref()
            .filter(|outcome| outcome.ok && outcome.text.contains("сопряжён"))
            .map(|outcome| outcome.line(ui_text::Lang::Ru))
    };
    let left_line = wait_for(&mut left, "the left window's pairing verdict", verdict);
    let right_line = wait_for(&mut right, "the right window's pairing verdict", verdict);
    for line in [&left_line, &right_line] {
        assert!(line.starts_with("Готово: "), "the outcome line: {line}");
    }

    // Both cores wrote the record the ceremony exists for: the other computer, by id. The
    // record's `display_name` is the label the peer advertised in `PairHello` — never an
    // identity (`docs/05-protocol.md`) — so it is asserted only as present.
    let a_trust = read_when_ready(
        &lab.a_root().join("config/trust.toml"),
        "the left core's record of the right computer",
        |text| text.contains(&lab.b_id),
    );
    let b_trust = read_when_ready(
        &lab.b_root().join("config/trust.toml"),
        "the right core's record of the left computer",
        |text| text.contains(&lab.a_id),
    );
    for (store, side) in [(&a_trust, "left"), (&b_trust, "right")] {
        assert!(
            store.contains("display_name = \""),
            "the {side} record keeps a name for the peer:\n{store}"
        );
        assert!(
            store.contains("last_address = \"127.0.0.1:"),
            "the {side} record keeps the address a restart dials:\n{store}"
        );
    }

    // The trust records and live link both appear without restarting either core.
    let live = |view: &ViewModel| {
        view.status
            .as_ref()
            .filter(|status| status.connected && status.members.len() == 1)
            .cloned()
    };
    let left_status = wait_for(&mut left, "the left window's live link", live);
    let right_status = wait_for(&mut right, "the right window's live link", live);
    assert_eq!(left_status.members[0].display_name, "right");
    assert_eq!(right_status.members[0].display_name, "left");
    for (root, side) in [(lab.a_root(), "left"), (lab.b_root(), "right")] {
        assert_eq!(links(root), 1, "the {side} core links without a restart");
    }
}

/// «Взять управление» on the follower, watched on its own status screen.
#[test]
fn the_claim_button_makes_the_follower_the_owner() {
    let _serial = serial();
    let lab = Lab::linked("claim");
    let mut follower = bridge_for(lab.b_root());

    // The workspace has to be settled before the button is pressed: the left computer claims
    // on startup, and a claim sent while *that* claim is still in flight is refused.
    let settled = wait_for(
        &mut follower,
        "the other computer to hold the workspace",
        |view| {
            view.status
                .as_ref()
                .filter(|status| {
                    status.connected
                        && status.owner_node_id.as_str() == lab.a_id.as_str()
                        && !status.is_owner
                })
                .cloned()
        },
    );
    assert_eq!(settled.role, "follower");
    assert_eq!(settled.members.len(), 1, "{settled:?}");
    if let Some(why) = missing_session(&settled) {
        panic!("this computer cannot take ownership: {why} ({settled:?})");
    }

    // `can_own = true` and `claim_on_startup = false` on this node, so the button has
    // something to change — and the status screen is where it shows.
    follower.dispatch(Action::Claim, Instant::now());
    let owned = wait_for(&mut follower, "this computer to become the owner", |view| {
        view.status
            .as_ref()
            .filter(|status| status.is_owner)
            .cloned()
    });
    assert_eq!(owned.role, "owner");
    assert_eq!(owned.owner_node_id.as_str(), lab.b_id.as_str());
    assert_eq!(owned.focus, "LOCAL", "ownership is not a transition");

    // The other computer really gave it up: its own control channel names the new owner.
    let a_status = ctl(lab.a_root(), "status");
    assert!(
        a_status.contains(&format!("owner={}", lab.b_id)),
        "the left core must report the new owner: {a_status}"
    );
}

#[test]
fn the_window_restarts_its_core_and_applies_a_lan_address() {
    let _serial = serial();
    let root = unique_dir("gui-restart");
    std::fs::create_dir_all(root.join("config")).expect("config directory");
    let ports = free_ports(2);
    write_config(&root, ports[0], ports[1], "restart", false);
    let mut bridge = bridge_for(&root);

    bridge.dispatch(Action::StartCore, Instant::now());
    let first_pid = wait_for(&mut bridge, "initial service and Status", |view| {
        (view.connection.is_connected() && view.status.is_some() && view.inflight.is_empty())
            .then(|| view.core.as_ref()?.pid)
            .flatten()
    });

    bridge.dispatch(Action::RestartCore, Instant::now());
    let second_pid = wait_for(
        &mut bridge,
        "service after restart and fresh Status",
        |view| {
            (view.connection.is_connected()
                && view.status.is_some()
                && view.inflight.is_empty()
                && count_lines(view, "Служба работает") >= 2)
                .then(|| view.core.as_ref()?.pid.filter(|pid| *pid != first_pid))
                .flatten()
        },
    );
    assert_ne!(
        second_pid, first_pid,
        "restart must launch a new core process"
    );

    let address = config::first_run::lan_addresses()
        .into_iter()
        .next()
        .expect("the host has a LAN address for the apply scenario");
    assert!(!address.is_loopback(), "the LAN candidate is not loopback");
    bridge.dispatch(
        Action::SelectLanAddress(address.to_string()),
        Instant::now(),
    );
    bridge.dispatch(Action::ApplyLanAddress, Instant::now());
    let saved =
        config::Config::load(root.join("config/config.toml")).expect("Apply writes a valid config");
    assert_eq!(saved.network.listen_address, address.to_string());

    let third_pid = wait_for(
        &mut bridge,
        "service after Apply and fresh Status",
        |view| {
            (view.connection.is_connected()
                && view.status.is_some()
                && view.inflight.is_empty()
                && count_lines(view, "Служба работает") >= 3)
                .then(|| view.core.as_ref()?.pid.filter(|pid| *pid != second_pid))
                .flatten()
        },
    );
    assert_ne!(third_pid, second_pid, "Apply must restart the core");
    let listener = SocketAddr::from((address, ports[0]));
    assert!(
        TcpStream::connect_timeout(&listener, Duration::from_secs(2)).is_ok(),
        "the restarted core must bind the newly saved address {listener}"
    );

    stop_core(third_pid).expect("test stops its own core");
    drop(bridge);
    let _ = std::fs::remove_dir_all(root);
}
#[test]
fn the_window_stops_a_core_it_did_not_start() {
    let _serial = serial();
    let mut lab = Lab::unpaired("gui-shutdown");
    let mut window = bridge_for(lab.a_root());

    wait_for(&mut window, "the independently started core", |view| {
        view.connection.is_connected().then_some(())
    });
    assert!(
        window.view().core.is_none(),
        "the window did not start this core"
    );

    window.dispatch(Action::StopCore, Instant::now());
    wait_for(&mut window, "the stopped service status", |view| {
        (view.connection_label() == "Служба не запущена").then_some(())
    });

    let deadline = Instant::now() + DEADLINE;
    let exit = loop {
        if let Some(exit) = lab.a.child.try_wait().expect("check the core process") {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "the core process did not exit after the GUI shutdown request"
        );
        std::thread::sleep(FRAME);
    };
    assert!(exit.success(), "the core exited cleanly: {exit}");
}
