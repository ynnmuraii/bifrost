//! The two-process harness the fault-injection tests drive.
//!
//! `docs/06-development-plan.md` («Рекомендуемая структура repository») puts fault
//! injection in `tests/fault-injection`; that directory is **not** a workspace member
//! (`members = ["apps/*", "crates/*"]`, and E0 deliberately never lists members), and
//! this harness is about `lws-core`'s own binaries, so it lives next to them as an
//! integration test of the `lws-core` package — the same shape `crates/transport` uses
//! for its mesh tests. `cargo test --workspace` picks it up; `CARGO_BIN_EXE_lws-core`
//! hands us the binary under test, and the agent binary sits next to it.
//!
//! Two real `lws-core` processes, separate `--root`, dry agents, driven through the
//! control channel: that is the E2 slice as a user runs it, minus the OS input. Every
//! wait is event-driven (`status` polling with generous timeouts) so the harness never
//! depends on a sleep being long enough; latencies are read back from the cores' own
//! logs, whose `tracing` timestamps are UTC to the microsecond.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The documented lease budget the fault tests measure against.
pub const LEASE_TTL_MS: u64 = 500;
/// `docs/02-product-requirements.md`, AC-02: the budget plus the scheduler's slack.
pub const AC_02_BUDGET_MS: u64 = LEASE_TTL_MS + 100;
/// What a *shared* CI host is allowed to add on top: the budget above is the product's
/// promise on an unloaded machine, and a runner is not one. The measured value is
/// reported either way, so a real regression is visible even when the assert passes.
pub const HOST_SLACK_MS: u64 = 100;

/// The `lws-core` binary under test.
fn core_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_lws-core"))
}

/// The agent binary, which `lws-core` spawns for every launch.
///
/// `--agent` names it explicitly, so the harness does not depend on the default lookup
/// (`the_default_agent_program_sits_next_to_core`) being right in a test profile.
fn agent_exe() -> PathBuf {
    let name = if cfg!(windows) {
        "lws-agent.exe"
    } else {
        "lws-agent"
    };
    let path = core_exe().with_file_name(name);
    assert!(
        path.is_file(),
        "{} is missing: run `cargo test --workspace` (or `cargo build -p lws-agent`) first",
        path.display()
    );
    path
}

/// Removes ANSI CSI sequences (the `ESC[…m` colour codes) from a log.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(current) = chars.next() {
        if current == '\u{1b}' {
            for code in chars.by_ref() {
                if code == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(current);
    }
    out
}

/// Milliseconds since the Unix epoch, from the harness's own clock.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as i64
}

/// Epoch milliseconds of a `tracing` UTC timestamp prefix (`2026-10-12T00:03:48.387695Z`).
///
/// The cores log on the same wall clock the harness reads, so a measurement never has to
/// line up two different clocks — it only has to subtract two of these.
pub fn utc_millis(line: &str) -> Option<i64> {
    let (date, rest) = line.split_once('T')?;
    let (time, _) = rest.split_once('Z')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: f64 = time.next()?.parse().ok()?;
    let whole = second.trunc() as i64;
    let fraction_ms = ((second - second.trunc()) * 1000.0).round() as i64;
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + whole) * 1000 + fraction_ms)
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// One `status` answer, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub focus: String,
    pub role: String,
    pub peer: String,
    pub members: usize,
    pub agent: String,
    pub backend: String,
    pub node: String,
    pub owner: String,
    pub serial: u64,
    pub pressed: usize,
    pub refusal: String,
}

impl Status {
    fn parse(line: &str) -> Result<Status, String> {
        let line = line.strip_prefix("OK ").ok_or_else(|| line.to_owned())?;
        // Values are space-free except `refusal=`, which is the last field and carries a
        // human sentence, so a token without '=' continues the previous value.
        let mut fields: Vec<(String, String)> = Vec::new();
        for token in line.split(' ') {
            match token.split_once('=') {
                Some((key, value)) => fields.push((key.to_owned(), value.to_owned())),
                None => match fields.last_mut() {
                    Some((_, value)) => {
                        value.push(' ');
                        value.push_str(token);
                    }
                    None => return Err(format!("status starts with a bare token: {line}")),
                },
            }
        }
        let get = |key: &str| {
            fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
                .ok_or_else(|| format!("status is missing {key}: {line}"))
        };
        Ok(Status {
            focus: get("focus")?.to_owned(),
            role: get("role")?.to_owned(),
            peer: get("peer")?.to_owned(),
            members: get("members")?.parse().map_err(|e| format!("{e}"))?,
            agent: get("agent")?.to_owned(),
            backend: get("backend")?.to_owned(),
            node: get("node")?.to_owned(),
            owner: get("owner")?.to_owned(),
            serial: get("serial")?.parse().map_err(|e| format!("{e}"))?,
            pressed: get("pressed")?.parse().map_err(|e| format!("{e}"))?,
            refusal: get("refusal").unwrap_or("").to_owned(),
        })
    }

    /// Both nodes are idle: nothing is leased, nothing is held.
    pub fn is_idle(&self) -> bool {
        self.focus == "Local" && self.peer == "-" && self.pressed == 0
    }
}

/// One running core.
pub struct Node {
    pub root: PathBuf,
    pub log: PathBuf,
    pub id: String,
    child: Child,
    pid: u32,
}

impl Node {
    /// Runs one control request, retrying the transport flake the CLI can see.
    ///
    /// The reply is one line; a failure to read it is a transport problem, not an FSM
    /// one, so it is retried a few times — and counted, because a run that needs many
    /// retries is itself a finding.
    pub fn ctl(&self, request: &str) -> Result<String, String> {
        let mut last = String::new();
        for attempt in 0..5 {
            let output = Command::new(core_exe())
                .arg("--root")
                .arg(&self.root)
                .args(request.split(' '))
                .output()
                .map_err(|error| format!("cannot run the CLI: {error}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if stdout.starts_with("OK") {
                return Ok(stdout);
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
        Err(last)
    }

    pub fn status(&self) -> Result<Status, String> {
        Status::parse(&self.ctl("status")?)
    }

    pub fn switch(&self, peer: &str) -> Result<String, String> {
        self.ctl(&format!("switch {peer}"))
    }

    /// Waits for a `status` the predicate accepts, polling instead of sleeping blindly.
    pub fn wait_for<F>(&self, what: &str, timeout: Duration, predicate: F) -> Result<Status, String>
    where
        F: Fn(&Status) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let last = match self.status() {
                Ok(status) if predicate(&status) => return Ok(status),
                Ok(status) => format!("{status:?}"),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(format!("{what} did not happen within {timeout:?}: {last}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The whole log so far, with colour sequences removed.
    ///
    /// `tracing`'s fmt layer colours its output whenever the environment looks like a
    /// terminal, and on a Linux runner it does that even for a redirected file, so the
    /// bytes the harness parses carry `ESC[…m` around every field. The timestamps and the
    /// field names are what matter here.
    pub fn log(&self) -> String {
        strip_ansi(&std::fs::read_to_string(&self.log).unwrap_or_default())
    }

    /// Every `focus focus="…"` transition with its wall-clock millisecond.
    pub fn focus_events(&self) -> Vec<(i64, String)> {
        self.log()
            .lines()
            .filter_map(|line| {
                let value = line.split_once("focus focus=\"")?.1.split('"').next()?;
                Some((utc_millis(line)?, value.to_owned()))
            })
            .collect()
    }

    /// The focus transitions each `control request request=Switch` produced.
    ///
    /// One window per request: the states between it and the next request, each with the
    /// latency from the request's own timestamp. Both numbers are the core's, so the
    /// measurement is the FSM's time rather than the harness's process spawn — and a
    /// request produces more than one state (`Preparing` then `Remote`, `Releasing` then
    /// `Local`), which is why the caller picks the state it is measuring.
    pub fn switch_windows(&self) -> Vec<Vec<(String, f64)>> {
        let log = self.log();
        let mut windows: Vec<Vec<(String, f64)>> = Vec::new();
        let mut current: Option<(i64, Vec<(String, f64)>)> = None;
        for line in log.lines() {
            if line.contains("control request request=Switch") {
                if let Some((_, states)) = current.take() {
                    windows.push(states);
                }
                current = utc_millis(line).map(|at| (at, Vec::new()));
                continue;
            }
            let Some((started, states)) = current.as_mut() else {
                continue;
            };
            let Some((_, tail)) = line.split_once("focus focus=\"") else {
                continue;
            };
            let Some(at) = utc_millis(line) else { continue };
            states.push((
                tail.split('"').next().unwrap_or("").to_owned(),
                (at - *started) as f64,
            ));
        }
        if let Some((_, states)) = current {
            windows.push(states);
        }
        windows
    }

    /// Lines mentioning a marker, with their timestamps.
    pub fn marked(&self, marker: &str) -> Vec<(i64, String)> {
        self.log()
            .lines()
            .filter(|line| line.contains(marker))
            .filter_map(|line| Some((utc_millis(line)?, line.trim().to_owned())))
            .collect()
    }

    /// The timestamp of the last line the *core* wrote.
    ///
    /// The agent shares the core's redirected stderr, so its lines are in this file too
    /// — and a frozen core's agent keeps talking (`core silent past the limit`), which
    /// would place the "last line" long after the freeze. Only the core's own markers
    /// count: it emits one of them with every lease exchange, so the last one is within
    /// a renewal period of the moment it stopped.
    pub fn last_core_millis(&self) -> Option<i64> {
        const CORE_ONLY: [&str; 4] = [
            "focus focus=",
            "core -> peer",
            "peer -> core",
            "core -> agent",
        ];
        self.log()
            .lines()
            .rev()
            .filter(|line| CORE_ONLY.iter().any(|marker| line.contains(marker)))
            .find_map(utc_millis)
    }

    /// The PID of the agent this core spawned.
    pub fn agent_pid(&self) -> Result<u32, String> {
        #[cfg(unix)]
        {
            // `procfs` instead of `pgrep`: no extra tool in a slim container image, and
            // `/proc/<pid>/stat`'s second field is the parent's pid.
            for entry in std::fs::read_dir("/proc").map_err(|error| error.to_string())? {
                let entry = entry.map_err(|error| error.to_string())?;
                let Ok(candidate) = entry.file_name().to_string_lossy().parse::<u32>() else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{candidate}/stat")) else {
                    continue;
                };
                // The second field is `comm`, which may contain spaces and parentheses;
                // everything after its last ')' starts at the state field.
                let mut fields = stat.rsplit(')').next().unwrap_or("").split_whitespace();
                let _state = fields.next();
                let parent: u32 = fields
                    .next()
                    .and_then(|field| field.parse().ok())
                    .unwrap_or(0);
                if parent == self.pid {
                    return Ok(candidate);
                }
            }
            return Err(format!("no agent under core {}", self.pid));
        }
        #[cfg(windows)]
        {
            let script = format!(
                "(Get-CimInstance Win32_Process -Filter \"Name='lws-agent.exe'\" | \
                 Where-Object {{$_.ParentProcessId -eq {}}}).ProcessId",
                self.pid
            );
            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output()
                .map_err(|error| format!("cannot list the children of {}: {error}", self.pid))?;
            let text = String::from_utf8_lossy(&output.stdout);
            text.split_whitespace()
                .next()
                .and_then(|pid| pid.parse().ok())
                .ok_or_else(|| format!("no agent under core {}: {text}", self.pid))
        }
    }

    /// Kills a process abruptly, the way `taskkill /F` or `kill -9` does.
    pub fn kill_pid(pid: u32) -> Result<(), String> {
        #[cfg(windows)]
        let status = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output()
            .map_err(|error| format!("taskkill: {error}"))?;
        #[cfg(unix)]
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .map_err(|error| format!("kill: {error}"))?;
        if status.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&status.stderr).trim().to_owned())
        }
    }

    /// Freezes the process: no `Leave` can be sent and the socket stays open.
    pub fn suspend(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            // `NtSuspendProcess` is not in the `windows` crate's surface this workspace
            // pins, and a test must not add an FFI dependency to the product's graph, so
            // the one call goes through PowerShell — the same route the manual run used.
            let script = format!(
                "Add-Type -MemberDefinition '[DllImport(\"ntdll.dll\")] public static extern int \
                 NtSuspendProcess(IntPtr h);' -Name Nt -Namespace W | Out-Null; \
                 [W.Nt]::NtSuspendProcess((Get-Process -Id {}).Handle) | Out-Null",
                self.pid
            );
            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output()
                .map_err(|error| format!("powershell: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        }
        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .args(["-STOP", &self.pid.to_string()])
                .status()
                .map_err(|error| format!("kill -STOP: {error}"))?;
            status
                .success()
                .then_some(())
                .ok_or_else(|| "kill -STOP failed".to_owned())
        }
    }

    /// The counterpart of [`Node::suspend`].
    pub fn resume(&self) -> Result<(), String> {
        #[cfg(windows)]
        {
            let script = format!(
                "Add-Type -MemberDefinition '[DllImport(\"ntdll.dll\")] public static extern int \
                 NtResumeProcess(IntPtr h);' -Name Nt -Namespace W | Out-Null; \
                 [W.Nt]::NtResumeProcess((Get-Process -Id {}).Handle) | Out-Null",
                self.pid
            );
            let output = Command::new("powershell")
                .args(["-NoProfile", "-Command", &script])
                .output()
                .map_err(|error| format!("powershell: {error}"))?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
            }
        }
        #[cfg(unix)]
        {
            let status = Command::new("kill")
                .args(["-CONT", &self.pid.to_string()])
                .status()
                .map_err(|error| format!("kill -CONT: {error}"))?;
            status
                .success()
                .then_some(())
                .ok_or_else(|| "kill -CONT failed".to_owned())
        }
    }

    /// Stops the core the documented way (`quit`), falling back to a kill.
    pub fn stop(&mut self) {
        let _ = self.ctl("quit");
        std::thread::sleep(Duration::from_millis(200));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The link between the two nodes, with a switch for the fault tests.
///
/// One direction is enough: the dialer records the address it typed, so pointing *A* at
/// the proxy routes the whole link through it, while B's record of A is the documented
/// port and its dial simply fails (harmless, and the same asymmetry `pair` documents).
pub struct Proxy {
    pub addr: String,
    mode: Arc<Mutex<Mode>>,
    live: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Forward,
    /// Swallow every byte: the peer looks alive at the socket level and says nothing.
    Blackhole,
    /// Drop the connections with `SO_LINGER 0`, which is an RST, not a FIN.
    Reset,
}

impl Proxy {
    /// Listens on a free port and forwards to `target` until told otherwise.
    pub fn start(target: u16) -> Result<Proxy, String> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|error| format!("proxy bind: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let mode = Arc::new(Mutex::new(Mode::Forward));
        let live = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let mode = Arc::clone(&mode);
            let live = Arc::clone(&live);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let Ok(upstream) = TcpStream::connect(("127.0.0.1", target)) else {
                                continue;
                            };
                            let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
                            let _ = upstream.set_read_timeout(Some(Duration::from_millis(20)));
                            if let Ok(clone) = stream.try_clone() {
                                live.lock().expect("proxy").push(clone);
                            }
                            let (backward, backward_target) =
                                match (upstream.try_clone(), stream.try_clone()) {
                                    (Ok(from_upstream), Ok(to_client)) => {
                                        (from_upstream, to_client)
                                    }
                                    _ => continue,
                                };
                            // Both directions, or the link would be half-duplex: the
                            // protocol needs the peer's answers to travel back.
                            let forward_mode = Arc::clone(&mode);
                            std::thread::spawn(move || bridge(stream, upstream, forward_mode));
                            let backward_mode = Arc::clone(&mode);
                            std::thread::spawn(move || {
                                bridge(backward, backward_target, backward_mode)
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Ok(Proxy {
            addr: format!("127.0.0.1:{port}"),
            mode,
            live,
            stop,
            thread: Some(thread),
        })
    }

    fn set(&self, mode: Mode) {
        *self.mode.lock().expect("proxy") = mode;
    }

    /// Stops forwarding without closing anything.
    pub fn blackhole(&self) {
        self.set(Mode::Blackhole);
    }

    /// Forwards again.
    pub fn forward(&self) {
        self.set(Mode::Forward);
    }

    /// Resets every live connection (and any that is accepted while in this mode).
    pub fn reset(&self) {
        self.set(Mode::Reset);
        // The bridge threads notice within their read timeout and close with the linger
        // option set; these clones are the last handles, so they must go too or the
        // socket would stay open and the peer would see silence instead of an RST.
        std::thread::sleep(Duration::from_millis(100));
        for stream in self.live.lock().expect("proxy").drain(..) {
            let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO));
            drop(stream);
        }
        self.set(Mode::Forward);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for stream in self.live.lock().expect("proxy").drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One direction of a proxied connection.
fn bridge(mut from: TcpStream, mut to: TcpStream, mode: Arc<Mutex<Mode>>) {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let current = *mode.lock().expect("proxy");
        if current == Mode::Reset {
            // `SO_LINGER 0` turns the close into an RST instead of a FIN. Every handle
            // has to be closed with it, which is why `reset` also drops its own clones.
            for stream in [&from, &to] {
                let _ = socket2::SockRef::from(stream).set_linger(Some(Duration::ZERO));
            }
            return;
        }
        match from.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                if current == Mode::Forward && to.write_all(&buffer[..read]).is_err() {
                    return;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return,
        }
    }
}

/// Serialises the tests in this binary.
///
/// Every scenario runs two cores and two agents, so six of them in parallel would put
/// twelve processes on a two-core CI runner — and the fault budgets are wall-clock
/// numbers, which a starved machine inflates. One harness at a time keeps the
/// measurements meaningful; a test that panics while holding the lock must not poison the
/// rest of the run.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Two cores, their roots and their logs.
pub struct Harness {
    pub dir: PathBuf,
    pub a: Node,
    pub b: Node,
    pub proxy: Option<Proxy>,
}

impl Harness {
    /// Pairs two fresh roots and runs them.
    ///
    /// `proxied` puts the link through a [`Proxy`]; `dry_motion` starts the source with
    /// the synthetic edge walk and writes the `[topology]` document it needs.
    pub fn start(proxied: bool, dry_motion: bool) -> Harness {
        let dir = unique_dir("fault-injection");
        let a_root = dir.join("a");
        let b_root = dir.join("b");
        let a_port = free_port();
        let b_port = free_port();
        for root in [&a_root, &b_root] {
            std::fs::create_dir_all(root.join("config")).expect("config directory");
        }
        write_config(&a_root, a_port, "source", true, dry_motion);
        write_config(&b_root, b_port, "receiver", false, dry_motion);

        let proxy = if proxied {
            Some(Proxy::start(b_port).expect("proxy"))
        } else {
            None
        };
        let dial = proxy
            .as_ref()
            .map(|proxy| proxy.addr.clone())
            .unwrap_or_else(|| format!("127.0.0.1:{b_port}"));
        pair(&a_root, &b_root, b_port, &dial);
        let (a_id, b_id) = peer_ids(&a_root, &b_root);
        if dry_motion {
            // The link document needs the peer's id, which only exists after pairing.
            write_topology(&a_root, &b_id);
            write_topology(&b_root, &a_id);
        }

        let a = spawn_node(a_root, a_id, dry_motion, "a");
        let b = spawn_node(b_root, b_id, dry_motion, "b");
        Harness { dir, a, b, proxy }
    }

    /// Waits until both nodes see each other, both agents are ready, and the source owns
    /// the workspace.
    ///
    /// Ownership is part of "linked": `claim_on_startup` runs a claim that needs the link
    /// and the peer's answer, so a `switch` sent while it is in flight is refused
    /// `NotOwner` — a real behaviour, and not what a fault test is measuring.
    pub fn wait_linked(&self) {
        let timeout = Duration::from_secs(20);
        self.a
            .wait_for("A links to B and owns the workspace", timeout, |status| {
                status.members >= 1 && status.agent == "ready" && status.role == "owner"
            })
            .expect("A must link to B and become the owner");
        self.b
            .wait_for("B links to A", timeout, |status| {
                status.members >= 1 && status.agent == "ready" && status.owner == self.a.id
            })
            .expect("B must link to A and see the owner");
        // A transition needs capabilities, and the agent reports none when the host has no
        // session at all: on a headless Linux runner the probe answers `UnsupportedSession`
        // and every `switch` is refused `SourceCapabilityMissing`. That is the product
        // behaving correctly, and it is not what a fault test measures — so say what is
        // missing instead of failing somewhere deeper.
        for node in [&self.a, &self.b] {
            let status = node.status().expect("status");
            assert_ne!(
                status.backend,
                "-",
                "{} reported no platform backend, so this host cannot run the fault \
                 harness: the agent refuses every transition with SourceCapabilityMissing. \
                 A headless Linux runner needs a session — `.github/workflows/ci.yml` runs \
                 the suite under `xvfb-run` for exactly this reason",
                node.log.display()
            );
            assert!(
                !node.log().trim().is_empty(),
                "{} is empty: the harness reads the core's stderr, so the redirect or the \
                 log format changed",
                node.log.display()
            );
        }
    }

    /// One `switch` cycle: Remote on both, then Local on both, asserting idleness.
    pub fn cycle(&self) {
        self.a
            .switch(&self.b.id)
            .expect("the switch to the peer is accepted");
        self.a
            .wait_for("A is Remote", Duration::from_secs(10), |status| {
                status.focus == "Remote"
            })
            .expect("A must reach Remote");
        self.b
            .wait_for("B is Remote", Duration::from_secs(10), |status| {
                status.focus == "Remote"
            })
            .expect("B must reach Remote");
        self.assert_idle_or_remote("Remote");
        self.return_to_local();
    }

    /// The return edge, with the idleness assertions a cycle must end on.
    pub fn return_to_local(&self) {
        self.a
            .switch(&self.b.id)
            .expect("the switch back is accepted");
        self.a
            .wait_for("A is Local", Duration::from_secs(10), |status| {
                status.is_idle()
            })
            .expect("A must return to Local");
        self.b
            .wait_for("B is Local", Duration::from_secs(10), |status| {
                status.is_idle()
            })
            .expect("B must return to Local");
        self.assert_idle_or_remote("Local");
    }

    /// Both nodes are where they should be, with no lease and an empty pressed set.
    pub fn assert_idle_or_remote(&self, where_: &str) {
        for node in [&self.a, &self.b] {
            let status = node.status().expect("status");
            assert_eq!(
                status.focus,
                where_,
                "{} focus: {status:?}",
                node.log.display()
            );
            assert_eq!(
                status.pressed,
                0,
                "{} still holds input after {where_}: {status:?}",
                node.log.display()
            );
            if where_ == "Local" {
                assert_eq!(
                    status.peer,
                    "-",
                    "{} still leases a peer: {status:?}",
                    node.log.display()
                );
            }
            assert_eq!(
                status.agent,
                "ready",
                "{} agent: {status:?}",
                node.log.display()
            );
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.a.stop();
        self.b.stop();
        // `LWS_KEEP_ROOTS=1` leaves the logs behind: a failing fault test is diagnosed
        // from the cores' own lines, not from a rerun.
        if std::env::var_os("LWS_KEEP_ROOTS").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            println!("roots kept at {}", self.dir.display());
        }
    }
}

/// A directory no other test shares.
pub fn unique_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let dir = std::env::temp_dir().join(format!(
        "lws-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temporary directory");
    dir
}

/// A port nothing is listening on right now.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("a free port")
        .local_addr()
        .expect("local address")
        .port()
}

/// The base configuration, with the topology section left out.
fn write_config(root: &Path, port: u16, name: &str, can_own: bool, _dry_motion: bool) {
    let document = format!(
        "config_version = 2\n\
         \n\
         [node]\n\
         display_name = \"{name}\"\n\
         can_own = {can_own}\n\
         claim_on_startup = {can_own}\n\
         \n\
         [network]\n\
         listen_address = \"127.0.0.1\"\n\
         port_input = {port}\n\
         port_clipboard = {}\n\
         max_members = 3\n\
         \n\
         [input]\n\
         require_emergency_hotkey = false\n\
         \n\
         [log]\n\
         level = \"info\"\n",
        port + 1,
    );
    std::fs::write(root.join("config/config.toml"), document).expect("configuration");
}

/// The `[topology]` document `--dry-motion` walks across.
///
/// One 1920×1080 display per node and a bidirectional link from the source's right edge
/// to the peer's left edge — the shape `docs/work/e3-smoke-transcript.md` proved.
fn write_topology(root: &Path, peer: &str) {
    let path = root.join("config/config.toml");
    let mut document = std::fs::read_to_string(&path).expect("configuration");
    document.push_str(&format!(
        "\n\
         [topology]\n\
         revision = 1\n\
         author = \"self\"\n\
         max_seen_revision = 1\n\
         max_seen_author = \"self\"\n\
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
         primary = true\n\
         \n\
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
    std::fs::write(&path, document).expect("topology document");
}

/// Runs the pairing ceremony, answering `y` on both sides.
fn pair(a_root: &Path, b_root: &Path, listen: u16, dial: &str) {
    let mut opener = Command::new(core_exe())
        .arg("--root")
        .arg(b_root)
        .args(["pair", "--open", "--listen", &format!("127.0.0.1:{listen}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the opener starts");
    opener
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"y\n")
        .expect("the opener is answered");
    std::thread::sleep(Duration::from_millis(700));
    let mut dialer = Command::new(core_exe())
        .arg("--root")
        .arg(a_root)
        .args(["pair", "--dial", dial])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the dialer starts");
    dialer
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"y\n")
        .expect("the dialer is answered");

    let dialer_out = dialer.wait_with_output().expect("the dialer finishes");
    let opener_out = opener.wait_with_output().expect("the opener finishes");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&dialer_out.stdout),
        String::from_utf8_lossy(&opener_out.stdout)
    );
    assert!(
        text.matches("paired with").count() == 2,
        "both sides must pair:\n{text}\n{}{}",
        String::from_utf8_lossy(&dialer_out.stderr),
        String::from_utf8_lossy(&opener_out.stderr)
    );
}

/// The own and peer node ids, from `peers`.
fn peer_ids(a_root: &Path, b_root: &Path) -> (String, String) {
    let a = peers(a_root);
    let b = peers(b_root);
    (a, b)
}

/// One node's own id, from `peers`.
fn peers(root: &Path) -> String {
    let output = Command::new(core_exe())
        .arg("--root")
        .arg(root)
        .arg("peers")
        .output()
        .expect("peers runs");
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("node ").map(str::to_owned))
        .unwrap_or_else(|| panic!("no node id in `peers` output:\n{text}"))
}

/// Starts one core with its log beside the root.
fn spawn_node(root: PathBuf, id: String, dry_motion: bool, label: &str) -> Node {
    let log = root.join(format!("{label}.log"));
    let file = std::fs::File::create(&log).expect("log file");
    let err = file.try_clone().expect("log file");
    let mut command = Command::new(core_exe());
    command
        .arg("--root")
        .arg(&root)
        .args(["run", "--dry-inject", "--dry-input-events", "4", "--agent"])
        .arg(agent_exe())
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    if dry_motion && label == "a" {
        command.arg("--dry-motion");
    }
    let child = command.spawn().expect("the core starts");
    let pid = child.id();
    Node {
        root,
        log,
        id,
        child,
        pid,
    }
}
