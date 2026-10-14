// `fault_injection.rs`, `layout.rs` and `three_nodes.rs` all include this module, so a
// helper one binary does not use is not dead code — it is unused *here*. That is the one
// allowance a shared test-support module needs; everything else stays lint-clean.
#![allow(dead_code)]

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

/// The `multideskflow` binary under test: every role of it — the daemon, and the agent the
/// daemon spawns as `multideskflow agent …` from its own executable.
fn core_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_multideskflow"))
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

    /// Recovery is transient, unlike a semantic switch refusal. Wait for a ready agent
    /// before asking, and retry only AgentBlocked if recovery started between the poll
    /// and the request. Fault tests still observe the actual recovery through status.
    pub fn switch(&self, peer: &str) -> Result<String, String> {
        let timeout = Duration::from_secs(15);
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "the agent did not recover before switching within {timeout:?}"
                ));
            }
            self.wait_for("the agent is ready before switching", remaining, |status| {
                status.agent == "ready" && status.focus != "Blocked"
            })?;
            match self.ctl(&format!("switch {peer}")) {
                Err(error) if error == "ERR SWITCH_REFUSED AgentBlocked" => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                }
                reply => return reply,
            }
        }
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

    /// The core's process id, for a scenario that kills it.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The whole log so far.
    ///
    /// Plain text by construction: core and the agent colour their logs only when stderr
    /// is a terminal (`with_ansi(std::io::stderr().is_terminal())`), and this harness
    /// always redirects them into a file. The harness used to strip `ESC[…m` here; that
    /// is gone, so a colour regression fails the log assertions instead of being hidden.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Where the dry agent mirrors its clipboard (`--dry-clipboard-dir`).
    ///
    /// Core owns the directory beside the launch token, and `--root DIR` puts the runtime
    /// directory under it (`config::Paths`: `run/`), so this is where the files are for every
    /// node the harness starts — on Windows as well, where `runtime_dir` is otherwise absent.
    pub fn dry_clipboard_dir(&self) -> PathBuf {
        self.root.join("run").join("clipboard-dry")
    }

    /// Writes text into the dry clipboard's `in.txt`, which the agent reads as a **local user
    /// copy** — the hook the clipboard suite drives the OS layer with, since the runner has no
    /// clipboard of its own.
    pub fn copy_to_clipboard(&self, text: &str) {
        let dir = self.dry_clipboard_dir();
        std::fs::create_dir_all(&dir).expect("dry clipboard directory");
        // A plain copy is plain: the markers belong to one copy, so a file left behind by an
        // earlier `copy_to_clipboard_marked` must not leak into this one.
        let _ = std::fs::remove_file(dir.join("in.markers"));
        std::fs::write(dir.join("in.txt"), text).expect("dry clipboard in.txt");
    }

    /// What the dry clipboard currently holds (`out.txt`), once the agent has written it.
    pub fn clipboard_text(&self) -> Option<String> {
        std::fs::read_to_string(self.dry_clipboard_dir().join("out.txt")).ok()
    }

    /// A local copy a password manager would have marked: `in.txt` plus the marker names the
    /// platform adapter reports (`AC-15`).
    pub fn copy_to_clipboard_marked(&self, text: &str, markers: &[&str]) {
        let dir = self.dry_clipboard_dir();
        std::fs::create_dir_all(&dir).expect("dry clipboard directory");
        // The markers go first: the agent reads the pair when `in.txt` changes.
        std::fs::write(dir.join("in.markers"), markers.join(",")).expect("dry clipboard markers");
        std::fs::write(dir.join("in.txt"), text).expect("dry clipboard in.txt");
    }

    /// Waits until the dry clipboard holds exactly `expected`.
    pub fn wait_for_clipboard(&self, expected: &str, timeout: Duration) -> Result<String, String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut last = String::new();
        while std::time::Instant::now() < deadline {
            if let Some(text) = self.clipboard_text() {
                if text == expected {
                    return Ok(text);
                }
                last = text;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Err(format!(
            "the dry clipboard never held {} bytes of the expected text; last seen: {} bytes",
            expected.len(),
            last.len()
        ))
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

    /// How many times this core asked its agent to observe the local pointer.
    ///
    /// The owner arms the observation once, and must re-arm it after every return to
    /// `Local` (the capture teardown destroys it), or a second crossing can never start.
    pub fn observe_arms(&self) -> usize {
        self.log()
            .matches("asked the agent to observe the local pointer")
            .count()
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

    /// Fires the emergency combination on this node's dry agent, with no desktop.
    ///
    /// The dry agent watches the path `LWS_DRY_EMERGENCY_FILE` names and reports
    /// `EmergencyFired` once when the marker appears. A real agent gets the emergency from
    /// its capture hook, which a dry session does not have.
    pub fn trigger_emergency(&self) {
        let dir = self.root.join("run");
        std::fs::create_dir_all(&dir).expect("run directory");
        std::fs::write(dir.join("emergency-dry"), b"emergency")
            .expect("write the emergency marker");
    }

    /// File-only controls consumed by the dry backend, never desktop input.
    pub fn dry_control_dir(&self) -> PathBuf {
        self.root.join("run").join("control-dry")
    }

    pub fn hold_dry_pointer(&self, x: f64, y: f64) {
        let sample = format!("{x} {y}");
        std::fs::write(self.dry_control_dir().join("held-pointer.txt"), &sample)
            .expect("hold the synthetic pointer");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let observed =
                std::fs::read_to_string(self.dry_control_dir().join("pointer-observed.txt"));
            if observed.as_deref().is_ok_and(|value| value == sample) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "dry pointer sample was not consumed: {sample}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Waits until core has processed the in-display seed, not merely until
    /// the dry agent has generated it. IPC batching must not erase the crossing.
    pub fn seed_dry_pointer(&self, x: f64, y: f64) {
        const PROCESSED: &str = "captured input outside a remote source lease";
        let before = self.marked(PROCESSED).len();
        self.hold_dry_pointer(x, y);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.marked(PROCESSED).len() <= before {
            assert!(
                Instant::now() < deadline,
                "core never processed the dry pointer seed at {x}, {y}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn trigger_dry_click(&self) {
        std::fs::write(self.dry_control_dir().join("click"), b"click")
            .expect("write the synthetic click");
    }

    /// A dry wheel turn of `detents` signed notches, the way [`Self::trigger_dry_click`] is one
    /// dry click: the dry source turns each notch into its own event, never one merged batch.
    pub fn trigger_dry_wheel(&self, detents: i32) {
        std::fs::write(self.dry_control_dir().join("wheel"), detents.to_string())
            .expect("write the synthetic wheel turn");
    }

    /// The `h120 v120` of every wheel event this node applied, in the order it applied them.
    /// An absent file is "nothing applied yet", which is what a poll loop has to be able to
    /// see without a panic.
    pub fn dry_injected_wheel(&self) -> Vec<String> {
        match std::fs::read_to_string(self.dry_control_dir().join("injected-wheel.txt")) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read dry injected wheel: {error}"),
        }
    }

    pub fn dry_injected_buttons(&self) -> Vec<String> {
        match std::fs::read_to_string(self.dry_control_dir().join("injected-buttons.txt")) {
            Ok(value) => value.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read dry injected buttons: {error}"),
        }
    }

    pub fn dry_local_passthrough(&self) -> u64 {
        match std::fs::read_to_string(self.dry_control_dir().join("local-passthrough.txt")) {
            Ok(value) => value.trim().parse().expect("dry local passthrough counter"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("read dry local passthrough counter: {error}"),
        }
    }

    /// Durable capture evidence, scoped to this action rather than an earlier capture.
    pub fn wait_dry_capture_started(&self, expected: &str, since: i64) -> i64 {
        let path = self.root.join("state/logs/lws-agent.log");
        let deadline = Instant::now() + Duration::from_secs(10);
        let marker = format!("dry mode: capture begins (no OS hooks) kind={expected}");
        loop {
            let log = std::fs::read_to_string(&path).unwrap_or_default();
            if let Some(at) = log.lines().find_map(|line| {
                let at = utc_millis(line)?;
                (at >= since && line.contains(&marker)).then_some(at)
            }) {
                return at;
            }
            assert!(
                Instant::now() < deadline,
                "agent never logged {expected} capture since {since}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn wait_dry_capture(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let actual = std::fs::read_to_string(self.dry_control_dir().join("capture-mode.txt"));
            if actual
                .as_deref()
                .is_ok_and(|value| value.trim() == expected)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "dry capture never became {expected}: {actual:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
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
            // The tail expression, not a `return`: this block is the whole body on Unix,
            // and clippy rejects the redundant `return` (it did not see it on Windows,
            // where the branch is compiled out).
            Err(format!("no agent under core {}", self.pid))
        }
        #[cfg(windows)]
        {
            let script = format!(
                "(Get-CimInstance Win32_Process -Filter \"Name='multideskflow.exe'\" | \
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
/// **Both** directions go through it. Every node dials every trusted peer, so a single
/// tunnel would leave a second, direct link for the dedup rule to choose between — and
/// which one survives depends on the node ids of that run. One listener per side, one
/// shared switch, and the fault always lands in the middle of the surviving link.
pub struct Proxy {
    /// The address A has to record for B.
    pub toward_b: String,
    /// The address B has to record for A.
    pub toward_a: String,
    mode: Arc<Mutex<Mode>>,
    live: Arc<Mutex<Vec<TcpStream>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
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
    /// Two tunnels: the first lands on A, the second on B.
    pub fn start(a_port: u16, b_port: u16) -> Result<Proxy, String> {
        let mode = Arc::new(Mutex::new(Mode::Forward));
        let live: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (toward_a, first) = tunnel(a_port, &mode, &live, &stop)?;
        let (toward_b, second) = tunnel(b_port, &mode, &live, &stop)?;
        Ok(Proxy {
            toward_b,
            toward_a,
            mode,
            live,
            stop,
            threads: vec![first, second],
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
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// One tunnel: accepts, dials `target`, and bridges both directions.
fn tunnel(
    target: u16,
    mode: &Arc<Mutex<Mode>>,
    live: &Arc<Mutex<Vec<TcpStream>>>,
    stop: &Arc<AtomicBool>,
) -> Result<(String, JoinHandle<()>), String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("proxy bind: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let mode = Arc::clone(mode);
    let live = Arc::clone(live);
    let stop = Arc::clone(stop);
    let thread = std::thread::spawn(move || {
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
                            (Ok(from_upstream), Ok(to_client)) => (from_upstream, to_client),
                            _ => continue,
                        };
                    // Both directions, or the link would be half-duplex: the protocol
                    // needs the peer's answers to travel back.
                    let forward_mode = Arc::clone(&mode);
                    std::thread::spawn(move || bridge(stream, upstream, forward_mode));
                    let backward_mode = Arc::clone(&mode);
                    std::thread::spawn(move || bridge(backward, backward_target, backward_mode));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    Ok((format!("127.0.0.1:{port}"), thread))
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
                // A read may have begun before blackhole() changed the mode. Check
                // after it returns, and keep the lock through forwarding so a
                // completed mode change cannot leak a prepare/ack already read.
                let current = mode.lock().expect("proxy");
                if *current == Mode::Forward && to.write_all(&buffer[..read]).is_err() {
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
    /// the synthetic edge walk and display, and writes the `[topology]` document it needs.
    pub fn start(proxied: bool, dry_motion: bool) -> Harness {
        Harness::start_with(proxied, dry_motion, |_, _| ()).0
    }
    /// Starts two dry agents with synthetic displays for topology-sharing tests.
    pub fn start_with_displays() -> Harness {
        Harness::start_with_options(false, false, true, |_, _| ()).0
    }

    /// A manually driven dry edge; no scheduled pointer walk races the fault.
    pub fn start_fault_controls(proxied: bool, refuse_prepare: bool) -> Harness {
        Harness::start_with(proxied, true, |a_root, b_root| {
            for root in [a_root, b_root] {
                std::fs::create_dir_all(root.join("run/control-dry"))
                    .expect("dry controls directory");
            }
            if refuse_prepare {
                std::fs::write(b_root.join("run/refuse-prepare-dry"), b"refuse")
                    .expect("dry prepare refusal marker");
            }
        })
        .0
    }

    /// The same, with a hook that runs once both roots are paired and the topology is
    /// written, but before either core starts.
    ///
    /// That window is what a scenario needing a different `config.toml` uses: the clipboard
    /// suite turns `[clipboard]` on there, and nothing that is already running has read the
    /// file yet.
    pub fn start_with<T>(
        proxied: bool,
        dry_motion: bool,
        before_run: impl FnOnce(&PathBuf, &PathBuf) -> T,
    ) -> (Harness, T) {
        Harness::start_with_options(proxied, dry_motion, dry_motion, before_run)
    }

    fn start_with_options<T>(
        proxied: bool,
        dry_motion: bool,
        dry_displays: bool,
        before_run: impl FnOnce(&PathBuf, &PathBuf) -> T,
    ) -> (Harness, T) {
        let dir = unique_dir("fault-injection");
        let a_root = dir.join("a");
        let b_root = dir.join("b");
        let a_port = free_port();
        let b_port = free_port();
        let a_clipboard_port = free_port();
        let b_clipboard_port = free_port();
        for root in [&a_root, &b_root] {
            std::fs::create_dir_all(root.join("config")).expect("config directory");
        }
        write_config(
            &a_root,
            a_port,
            a_clipboard_port,
            "source",
            true,
            dry_motion,
        );
        write_config(
            &b_root,
            b_port,
            b_clipboard_port,
            "receiver",
            false,
            dry_motion,
        );

        // Pairing happens on the real ports: it is the ceremony that writes the records.
        pair(&a_root, &b_root, a_port, b_port);
        let proxy = if proxied {
            let proxy = Proxy::start(a_port, b_port).expect("proxy");
            // Both records move behind the proxy. Leaving one direct would let the dedup
            // rule keep a link the fault tests cannot reach.
            retarget_trust(&a_root, &proxy.toward_b);
            retarget_trust(&b_root, &proxy.toward_a);
            Some(proxy)
        } else {
            None
        };
        let (a_id, b_id) = peer_ids(&a_root, &b_root);
        if dry_motion {
            // The link document needs the peer's id, which only exists after pairing.
            // Each node describes the same shared boundary from its own side.
            write_topology(&a_root, &b_id, "right", "left");
            write_topology(&b_root, &a_id, "left", "right");
        }
        let attached = before_run(&a_root, &b_root);

        let a = spawn_node(a_root, a_id, dry_motion, dry_displays, "a");
        let b = spawn_node(b_root, b_id, dry_motion, dry_displays, "b");
        (Harness { dir, a, b, proxy }, attached)
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

/// Prints p50/p95/p99 (and the maximum) of one latency sample.
pub fn report_latencies(label: &str, values: &[f64]) {
    if values.is_empty() {
        println!("{label}: no samples");
        return;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let pick = |q: f64| sorted[((sorted.len() - 1) as f64 * q).round() as usize];
    println!(
        "{label}: n={} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
        sorted.len(),
        pick(0.50),
        pick(0.95),
        pick(0.99),
        sorted[sorted.len() - 1],
    );
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
fn write_config(
    root: &Path,
    port: u16,
    clipboard_port: u16,
    name: &str,
    can_own: bool,
    _dry_motion: bool,
) {
    // The two-node harness couples them: the source owns and claims, the receiver does
    // neither. `write_config_sides` is for the cluster, where every node may own but only
    // one claims on startup.
    write_config_sides(root, port, clipboard_port, name, can_own, can_own);
}

/// The base configuration with `can_own` and `claim_on_startup` chosen separately.
///
/// `port_clipboard` is a port of its own and *not* `port + 1`: two nodes whose input ports
/// happen to be adjacent (`free_port` hands out whatever the OS has) would otherwise collide,
/// one of them would fail to bind its clipboard listener, and the suite would fail for a
/// reason that has nothing to do with the clipboard.
fn write_config_sides(
    root: &Path,
    port: u16,
    clipboard_port: u16,
    name: &str,
    can_own: bool,
    claim_on_startup: bool,
) {
    let document = format!(
        "config_version = 2\n\
         \n\
         [node]\n\
         display_name = \"{name}\"\n\
         can_own = {can_own}\n\
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

/// Turns `[clipboard]` on in a node's `config.toml` before it starts.
///
/// ADR-008 makes the clipboard opt-in, and off by default, so the suite that needs the second
/// channel turns it on explicitly instead of making every other test carry the setting.
pub fn enable_clipboard(root: &Path) {
    append_config(
        root,
        "\n[clipboard]\nenabled = true\nskip_secret_marked = true\n",
    );
}

/// The same, with an explicit per-channel rate.
///
/// `KVM-042` needs a transfer that is slow enough to measure against: at the default 256 KiB/s
/// a 1 MiB payload takes four seconds, and the test that measures a transition against it wants
/// the clipboard busy for longer than that.
pub fn write_clipboard_rate(root: &Path, rate_bytes_per_s: u64) {
    append_config(
        root,
        &format!(
            "\n[clipboard]\nenabled = true\nskip_secret_marked = true\nrate_bytes_per_s = {rate_bytes_per_s}\n"
        ),
    );
}

/// Appends a TOML fragment to a node's configuration.
pub fn append_config(root: &Path, fragment: &str) {
    let path = root.join("config/config.toml");
    let mut document = std::fs::read_to_string(&path).expect("configuration");
    document.push_str(fragment);
    std::fs::write(&path, document).expect("configuration");
}

/// Seeds divergent remembered pairs before either paired core starts.
///
/// Both name the source as owner, but the receiver remembers a newer serial. The source
/// must recover automatically through its startup claim and the real peer handshake.
pub fn seed_restored_source_owner(
    a_root: &PathBuf,
    b_root: &PathBuf,
    source_serial: u64,
    receiver_serial: u64,
) {
    let (source_id, _) = peer_ids(a_root, b_root);
    for (root, serial, claim_on_startup) in [
        (a_root, source_serial, true),
        (b_root, receiver_serial, false),
    ] {
        let path = root.join("config/config.toml");
        let mut config = config::Config::load(&path).expect("paired configuration");
        config.node.claim_on_startup = claim_on_startup;
        config.workspace.owner_claim_serial = serial;
        config.workspace.owner_node_id = source_id.clone();
        config
            .save(&path)
            .expect("remembered ownership configuration");
    }
}

/// The `[topology]` document `--dry-motion` walks across.
///
/// One 1920×1080 display per node; each side describes the same bidirectional boundary
/// in its own direction — the shape `docs/work/e3-smoke-transcript.md` proved.
fn write_topology(root: &Path, peer: &str, from_edge: &str, to_edge: &str) {
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
         edge = \"{from_edge}\"\n\
         start = 0.0\n\
         end = 1080.0\n\
         \n\
         [topology.links.to]\n\
         node = \"peer:{peer}\"\n\
         display = \"dry-0\"\n\
         edge = \"{to_edge}\"\n\
         start = 0.0\n\
         end = 1080.0\n"
    ));
    std::fs::write(&path, document).expect("topology document");
}

/// Runs the pairing ceremony, answering `y` on both sides.
fn pair(a_root: &Path, b_root: &Path, a_port: u16, b_port: u16) {
    let listen = b_port;
    let dial = format!("127.0.0.1:{b_port}");
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

    // Wait for the window, do not sleep for it. The opener prints this line once its gate is
    // installed, and a fixed pause loses that race on a loaded machine (two test processes on two
    // CPUs): the dialer then arrives before the window is open, the listener drops it *before*
    // the handshake («peer closed the connection without sending TLS close_notify»), and the
    // opener sits out its whole 120 s window («the pairing window ran out») — which is how the
    // layout tests failed in `CI` with a pairing error instead of the layout assertion they are
    // about. Same rule as everywhere else in this harness: wait for the event.
    let opener_stdout = opener.stdout.take().expect("the opener's stdout");
    let opener_text = Arc::new(std::sync::Mutex::new(String::new()));
    let (opened_tx, opened_rx) = std::sync::mpsc::channel();
    let reader = {
        let text = Arc::clone(&opener_text);
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(opener_stdout).lines() {
                let Ok(line) = line else { break };
                let mut collected = text.lock().expect("the opener's output");
                collected.push_str(&line);
                collected.push('\n');
                drop(collected);
                if line.contains("waiting for a peer to dial") {
                    let _ = opened_tx.send(());
                }
            }
        })
    };
    opened_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the opener must announce that its pairing window is waiting");

    let mut dialer = Command::new(core_exe())
        .arg("--root")
        .arg(a_root)
        .args(["pair", "--dial", &dial])
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
    let opener_status = opener.wait().expect("the opener finishes");
    let _ = reader.join();
    let opener_stdout = opener_text.lock().expect("the opener's output").clone();
    let mut opener_stderr = String::new();
    if let Some(mut stderr) = opener.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut stderr, &mut opener_stderr);
    }
    let text = format!(
        "{}{opener_stdout}",
        String::from_utf8_lossy(&dialer_out.stdout)
    );
    assert!(
        text.matches("paired with").count() == 2,
        "both sides must pair:\n{text}\n{}{}",
        String::from_utf8_lossy(&dialer_out.stderr),
        opener_stderr,
    );
    assert!(
        opener_status.success(),
        "the opener must exit cleanly:\n{opener_stderr}"
    );

    // `PairHello.listen_port` is what makes both records usable: the dialer records the
    // address it typed, and the opener records the dialer's IP with the port the dialer
    // *listens* on instead of the documented 47901. Before that field existed, B's record
    // named a port A does not listen on and B could never dial back.
    let a_trust = std::fs::read_to_string(a_root.join("config/trust.toml")).expect("trust");
    let b_trust = std::fs::read_to_string(b_root.join("config/trust.toml")).expect("trust");
    assert!(
        a_trust.contains(&format!("127.0.0.1:{b_port}")),
        "A must record B's real port:\n{a_trust}"
    );
    assert!(
        b_trust.contains(&format!("127.0.0.1:{a_port}")),
        "B must record A's real port, which only `PairHello.listen_port` can tell it:\n{b_trust}"
    );
}

/// Points one node's trust entry at `address`, the way a proxy in the middle requires.
fn retarget_trust(root: &Path, address: &str) {
    let path = root.join("config/trust.toml");
    let text = std::fs::read_to_string(&path).expect("trust store");
    let mut out = String::with_capacity(text.len());
    let mut replaced = false;
    for line in text.lines() {
        if line.starts_with("last_address") {
            out.push_str(&format!("last_address = \"{address}\"\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(replaced, "the trust store must carry an address:\n{text}");
    std::fs::write(&path, out).expect("trust store");
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
fn spawn_node(
    root: PathBuf,
    id: String,
    dry_motion: bool,
    dry_displays: bool,
    label: &str,
) -> Node {
    let log = root.join(format!("{label}.log"));
    let file = std::fs::File::create(&log).expect("log file");
    let err = file.try_clone().expect("log file");
    let mut command = Command::new(core_exe());
    command
        .arg("--root")
        .arg(&root)
        .args(["run", "--dry-inject", "--dry-input-events", "4"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err));
    let dry_control_dir = root.join("run/control-dry");
    if dry_motion && label == "a" && !dry_control_dir.is_dir() {
        command.arg("--dry-motion");
    }
    if dry_displays {
        command.arg("--dry-displays");
    }
    // The dry emergency aid: the agent inherits this path and reports `EmergencyFired`
    // once when the file appears (`Node::trigger_emergency`). Harmless until it does.
    command.env(
        "LWS_DRY_EMERGENCY_FILE",
        root.join("run").join("emergency-dry"),
    );
    if dry_control_dir.is_dir() {
        command.env("LWS_DRY_CONTROL_DIR", dry_control_dir);
        command.env("LWS_LOG", "debug");
    }
    if root.join("run/refuse-prepare-dry").is_file() {
        command.env("LWS_DRY_REFUSE_PREPARE", "1");
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

/// Three cores on loopback, paired pairwise.
///
/// `docs/04-decisions.md` ADR-011: trust is pairwise — no shared secret, no `workspace_id`
/// — so three nodes need three ceremonies, and each ceremony records a usable address on
/// both sides (`PairHello.listen_port`). The nodes sit in a line `w` — `a` — `b`, which is
/// what the chain in KVM-074 walks.
pub struct Cluster {
    pub dir: PathBuf,
    /// `w`, `a`, `b` in that order.
    pub nodes: Vec<Node>,
    /// Their listening ports, for diagnostics and for a proxy a scenario may need.
    pub ports: Vec<u16>,
}

impl Cluster {
    /// Starts the three nodes with no proxy in the middle.
    pub fn start(label: &str) -> Cluster {
        Cluster::start_with(label, |_, _, _| ()).0
    }

    /// Starts the three nodes, calling `before_run` once the roots are *paired* and the
    /// topology is written, but before anything dials.
    ///
    /// That window is what a partition scenario needs: the hook installs a proxy and
    /// points the trust records at it, and whatever it returns travels back to the caller
    /// so the tunnels stay alive.
    pub fn start_with<T>(
        label: &str,
        before_run: impl FnOnce(&[PathBuf], &[u16], &[String]) -> T,
    ) -> (Cluster, T) {
        let (dir, roots, ports, ids) = Cluster::prepare(label);
        let attached = before_run(&roots, &ports, &ids);
        let nodes = (0..3)
            .map(|index| {
                spawn_node(
                    roots[index].clone(),
                    ids[index].clone(),
                    false,
                    false,
                    ["w", "a", "b"][index],
                )
            })
            .collect();
        (Cluster { dir, nodes, ports }, attached)
    }

    /// Pairs three fresh roots and writes their topology: `w` claims on startup
    /// (`claim_on_startup`), `a` and `b` follow.
    fn prepare(label: &str) -> (PathBuf, Vec<PathBuf>, Vec<u16>, Vec<String>) {
        let dir = unique_dir(label);
        let names = ["w", "a", "b"];
        let mut roots = Vec::new();
        let mut ports = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let root = dir.join(name);
            std::fs::create_dir_all(root.join("config")).expect("config directory");
            let port = free_port();
            let clipboard_port = free_port();
            // `AC-21`: «владение можно заявить с любого узла» — every node in the cluster
            // may own; only `w` claims on startup, the other two follow until asked.
            write_config_sides(
                &root,
                port,
                clipboard_port,
                &format!("node-{name}"),
                true,
                index == 0,
            );
            roots.push(root);
            ports.push(port);
        }
        // Three ceremonies, one per unordered pair.
        for (first, second) in [(0usize, 1usize), (0, 2), (1, 2)] {
            pair(&roots[first], &roots[second], ports[first], ports[second]);
        }
        let ids: Vec<String> = roots.iter().map(|root| peers(root)).collect();
        for (index, root) in roots.iter().enumerate() {
            write_line_topology(root, &ids, index);
        }
        (dir, roots, ports, ids)
    }

    pub fn node(&self, index: usize) -> &Node {
        &self.nodes[index]
    }

    /// Every node sees both peers, each agent is ready, and `w` owns the workspace.
    pub fn wait_mesh(&self, timeout: Duration) {
        for (index, node) in self.nodes.iter().enumerate() {
            node.wait_for(
                &format!("node {} joins the mesh", index),
                timeout,
                |status| {
                    status.members == 2
                        && status.agent == "ready"
                        && status.owner == self.nodes[0].id
                },
            )
            .unwrap_or_else(|error| panic!("node {index} never joined: {error}"));
        }
    }

    /// The owner every node reports, as one vector, so a disagreement is visible.
    pub fn owners(&self) -> Vec<String> {
        self.nodes
            .iter()
            .map(|node| node.status().map(|status| status.owner).unwrap_or_default())
            .collect()
    }

    /// Waits until every node names `index` as the owner.
    pub fn wait_owner(&self, index: usize, timeout: Duration) {
        let expected = self.nodes[index].id.clone();
        for (position, node) in self.nodes.iter().enumerate() {
            node.wait_for(
                &format!("node {position} sees {} as the owner", index),
                timeout,
                |status| status.owner == expected,
            )
            .unwrap_or_else(|error| panic!("node {position}: {error}"));
        }
    }

    pub fn claim(&self, index: usize) -> Result<String, String> {
        self.nodes[index].ctl("claim")
    }

    pub fn abandon(&self, index: usize) -> Result<String, String> {
        self.nodes[index].ctl("abandon")
    }

    pub fn switch(&self, from: usize, to: usize) -> Result<String, String> {
        let target = self.nodes[to].id.clone();
        self.nodes[from].switch(&target)
    }

    /// Every node is `Local`, holds no lease and no keys.
    pub fn assert_idle(&self) {
        for (index, node) in self.nodes.iter().enumerate() {
            let status = node.status().expect("status");
            assert!(status.is_idle(), "node {index} is not at rest: {status:?}");
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for node in &mut self.nodes {
            node.stop();
        }
        if std::env::var_os("LWS_KEEP_ROOTS").is_none() {
            let _ = std::fs::remove_dir_all(&self.dir);
        } else {
            println!("cluster roots kept at {}", self.dir.display());
        }
    }
}

/// The documented path roots of a `--root DIR` core.
///
/// Mirrors `apps/lws-core/src/main.rs::resolve_paths`: `config/`, `data/`, `state/` and
/// `run/` under the root. The GUI's endpoint is derived from these
/// (`ipc::transport::gui_endpoint`), which is the same call the daemon and `lws-gui` make.
pub fn paths_under(root: &Path) -> config::Paths {
    config::Paths::new(
        root.join("config"),
        root.join("data"),
        root.join("state"),
        Some(root.join("run")),
    )
}

/// The GUI endpoint of a core started with `--root DIR`.
pub fn gui_endpoint_of(root: &Path) -> ipc::transport::Endpoint {
    ipc::transport::gui_endpoint(&paths_under(root)).expect("the GUI endpoint of a --root core")
}

/// Two cores that are **not** paired with each other.
///
/// Every other harness pairs first, because the scenarios are about input. The pairing
/// ceremony itself — the one the GUI's wizard drives — must not be done for it, so this
/// starts two bare cores with no trust records and no `[[network.peers]]`.
pub struct Unpaired {
    /// The directory both roots live in.
    pub dir: PathBuf,
    /// This node opens the pairing window.
    pub a: Node,
    /// This node dials it.
    pub b: Node,
    /// Each node's node id, as the CLI prints it (64 hex).
    pub a_id: String,
    /// See [`Unpaired::a_id`].
    pub b_id: String,
    /// The port A listens on (`network.port_input`), which its pairing window answers on.
    pub a_port: u16,
    /// See [`Unpaired::a_port`].
    pub b_port: u16,
}

impl Unpaired {
    /// Starts the two cores, each with its own `--root`.
    pub fn start(label: &str) -> Unpaired {
        let dir = unique_dir(label);
        let a_root = dir.join("a");
        let b_root = dir.join("b");
        for root in [&a_root, &b_root] {
            std::fs::create_dir_all(root.join("config")).expect("config directory");
        }
        // Paired with nobody, so the identity has to be created before the id can be read:
        // `peers` loads-or-creates it, which is what the harness does for the paired cases too.
        let a_port = free_port();
        let b_port = free_port();
        write_config(&a_root, a_port, free_port(), "left", true, false);
        write_config(&b_root, b_port, free_port(), "right", true, false);
        let a_id = peers(&a_root);
        let b_id = peers(&b_root);
        let a = spawn_node(a_root, a_id.clone(), false, false, "a");
        let b = spawn_node(b_root, b_id.clone(), false, false, "b");
        Unpaired {
            dir,
            a,
            b,
            a_id,
            b_id,
            a_port,
            b_port,
        }
    }

    /// Waits until both cores answer on their control channel.
    pub fn wait_ready(&self) {
        for node in [&self.a, &self.b] {
            node.wait_for("the core to come up", Duration::from_secs(20), |status| {
                status.agent == "ready"
            })
            .expect("the core must answer `status`");
        }
    }
}

/// Points one node's trust record for `peer` at `address`.
///
/// The two-node `retarget_trust` above replaces the single `last_address`; with three nodes
/// a root holds two records, so the stanza is matched by `node_id` first.
pub fn retarget_trust_peer(root: &Path, peer: &str, address: &str) {
    let path = root.join("config/trust.toml");
    let text = std::fs::read_to_string(&path).expect("trust store");
    let mut out = String::with_capacity(text.len());
    let mut matching = false;
    let mut replaced = false;
    for line in text.lines() {
        if line.starts_with("[[peers]]") {
            matching = false;
        } else if let Some(id) = line.strip_prefix("node_id = ") {
            matching = id.trim().trim_matches('"') == peer;
        } else if matching && line.starts_with("last_address") {
            out.push_str(&format!("last_address = \"{address}\"\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(replaced, "no trust record for {peer}:\n{text}");
    std::fs::write(&path, out).expect("trust store");
}

/// The `[topology]` document of the three-node line: three screens side by side and the
/// edges this node owns.
fn write_line_topology(root: &Path, ids: &[String], index: usize) {
    let reference = |peer: usize| {
        if peer == index {
            "self".to_owned()
        } else {
            format!("peer:{}", ids[peer])
        }
    };
    let mut document =
        std::fs::read_to_string(root.join("config/config.toml")).expect("configuration");
    document.push_str(
        "\n[topology]\nrevision = 1\nauthor = \"self\"\nmax_seen_revision = 1\n\
         max_seen_author = \"self\"\n",
    );
    for peer in 0..3 {
        document.push_str(&format!(
            "\n[[topology.displays]]\nnode = \"{}\"\nid = \"dry-0\"\n\
             logical = [{}.0, 0.0, 1920.0, 1080.0]\nprimary = true\n",
            reference(peer),
            peer * 1920
        ));
    }
    if index > 0 {
        document.push_str(&link_block(
            "left",
            &reference(index),
            "left",
            &reference(index - 1),
            "right",
        ));
    }
    if index < 2 {
        document.push_str(&link_block(
            "right",
            &reference(index),
            "right",
            &reference(index + 1),
            "left",
        ));
    }
    std::fs::write(root.join("config/config.toml"), document).expect("topology document");
}

/// One bidirectional link, `from`'s edge to `to`'s edge.
fn link_block(id: &str, from: &str, from_edge: &str, to: &str, to_edge: &str) -> String {
    format!(
        "\n[[topology.links]]\nid = \"{id}\"\nbidirectional = true\n\
         \n[topology.links.from]\nnode = \"{from}\"\ndisplay = \"dry-0\"\nedge = \"{from_edge}\"\n\
         start = 0.0\nend = 1080.0\n\
         \n[topology.links.to]\nnode = \"{to}\"\ndisplay = \"dry-0\"\nedge = \"{to_edge}\"\n\
         start = 0.0\nend = 1080.0\n"
    )
}
