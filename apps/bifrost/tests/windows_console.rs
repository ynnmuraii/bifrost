//! Real CLI launches only: never start the GUI, a core, an agent, or native input.
#![cfg(windows)]

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::mem::{size_of, size_of_val};
use std::net::TcpListener;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, DETACHED_PROCESS, EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTUPINFOEXW,
};

const EXE: &str = env!("CARGO_BIN_EXE_bifrost");
const TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT: &str = "codes match? [y/N]";

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "bifrost-console-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("isolated CLI root");
        Self(path)
    }

    fn args(&self, rest: &[&str]) -> Vec<String> {
        let root = self.0.to_str().expect("UTF-8 root").to_owned();
        if rest == ["--version"] {
            return vec!["--version".to_owned(), "--root".to_owned(), root];
        }
        let mut args = vec!["--root".to_owned(), root];
        args.extend(rest.iter().map(|arg| (*arg).to_owned()));
        args
    }

    fn configure_pair(&self, port: u16, clipboard_port: u16) {
        std::fs::create_dir_all(self.0.join("config")).expect("config directory");
        // Match tests/common's CLI pairing configuration, without starting its core/agent.
        std::fs::write(
            self.0.join("config/config.toml"),
            format!(
                "config_version = 2\n\
                 [node]\n\
                 display_name = \"CONSOLE-TEST\"\n\
                 can_own = false\n\
                 claim_on_startup = false\n\
                 [network]\n\
                 listen_address = \"127.0.0.1\"\n\
                 port_input = {port}\n\
                 port_clipboard = {clipboard_port}\n\
                 max_members = 3\n"
            ),
        )
        .expect("pairing config");
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Capture {
    rx: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
}

impl Capture {
    fn new() -> (Self, Sender<Vec<u8>>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                rx,
                bytes: Vec::new(),
            },
            tx,
        )
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn until(&mut self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.text().contains(expected) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(bytes) => self.bytes.extend(bytes),
                Err(error) => panic!("waiting for {expected:?}: {error}\n{}", self.text()),
            }
        }
    }

    fn finish(&mut self) -> String {
        // Writers run concurrently, including during ClosePseudoConsole's final screen flush.
        loop {
            match self.rx.recv_timeout(TIMEOUT) {
                Ok(bytes) => self.bytes.extend(bytes),
                Err(mpsc::RecvTimeoutError::Disconnected) => return self.text(),
                Err(error) => panic!("output did not close: {error}\n{}", self.text()),
            }
        }
    }
}

fn drain(mut reader: impl Read + Send + 'static, tx: Sender<Vec<u8>>) {
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(len) => {
                    if tx.send(buffer[..len].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn pipe() -> (File, File) {
    let mut read = null_mut();
    let mut write = null_mut();
    // SAFETY: output pointers are valid; null security attributes make handles non-inheritable.
    assert_ne!(
        unsafe { CreatePipe(&mut read, &mut write, null(), 0) },
        0,
        "CreatePipe: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: CreatePipe transferred two distinct owned handles to this caller.
    unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) }
}

struct PseudoConsole(HPCON);

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner; the output reader remains active until EOF.
        unsafe { ClosePseudoConsole(self.0) };
    }
}

struct PtyChild {
    process: OwnedHandle,
    console: Option<PseudoConsole>,
    input: File,
    output: Capture,
    exited: bool,
}

impl PtyChild {
    fn spawn(application: &str, command: &str) -> Self {
        let (input_read, input) = pipe();
        let (output_read, output_write) = pipe();
        let (output, tx) = Capture::new();
        drain(output_read, tx);
        let mut console = 0;
        // SAFETY: pipe ends are live and the HPCON output pointer is valid.
        let result = unsafe {
            CreatePseudoConsole(
                COORD { X: 240, Y: 40 },
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut console,
            )
        };
        assert_eq!(result, 0, "CreatePseudoConsole HRESULT {result:#x}");
        let console = PseudoConsole(console);
        drop(input_read);
        drop(output_write);

        let mut bytes = 0;
        // SAFETY: the initial call only queries the required allocation size.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        assert_ne!(bytes, 0, "attribute-list size");
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let attributes = storage.as_mut_ptr().cast();
        // SAFETY: storage is aligned and has the exact queried capacity or greater.
        assert_ne!(
            unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut bytes) },
            0,
            "InitializeProcThreadAttributeList: {}",
            std::io::Error::last_os_error()
        );
        // PSEUDOCONSOLE takes the HPCON value, not a pointer to that value.
        // SAFETY: attributes is initialized and console lives through CreateProcessW.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console.0 as *const _,
                size_of_val(&console.0),
                null_mut(),
                null(),
            )
        };
        if updated == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: attributes was initialized successfully.
            unsafe { DeleteProcThreadAttributeList(attributes) };
            panic!("UpdateProcThreadAttribute: {error}");
        }
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes;
        let application: Vec<u16> = OsStr::new(application)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut command: Vec<u16> = OsStr::new(command).encode_wide().chain(Some(0)).collect();
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: all startup buffers remain live. Only the pseudoconsole attribute is passed;
        // no inherited standard handles can mask the client's console initialization.
        let created = unsafe {
            CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup.StartupInfo,
                &mut process,
            )
        };
        let error = std::io::Error::last_os_error();
        // SAFETY: attributes is initialized, and CreateProcessW no longer needs it.
        unsafe { DeleteProcThreadAttributeList(attributes) };
        assert_ne!(created, 0, "CreateProcessW: {error}");
        // SAFETY: successful CreateProcessW transferred both owned handles.
        let process = unsafe {
            drop(OwnedHandle::from_raw_handle(process.hThread));
            OwnedHandle::from_raw_handle(process.hProcess)
        };
        Self {
            process,
            console: Some(console),
            input,
            output,
            exited: false,
        }
    }

    fn answer(&mut self, answer: &[u8]) {
        self.input.write_all(answer).expect("PTY stdin answer");
        self.input.flush().expect("flush PTY stdin");
    }

    fn finish(&mut self) -> (u32, String) {
        // SAFETY: process remains owned and valid throughout both calls.
        let wait = unsafe {
            WaitForSingleObject(self.process.as_raw_handle(), TIMEOUT.as_millis() as u32)
        };
        assert_eq!(
            wait,
            WAIT_OBJECT_0,
            "CLI did not exit: {}",
            self.output.text()
        );
        let mut code = 0;
        // SAFETY: the process has exited and code points to writable storage.
        assert_ne!(
            unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) },
            0
        );
        self.exited = true;
        drop(self.console.take());
        (code, self.output.finish())
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if !self.exited {
            // SAFETY: only the isolated fixture process is terminated, including on assertion failure.
            unsafe {
                TerminateProcess(self.process.as_raw_handle(), 1);
                WaitForSingleObject(self.process.as_raw_handle(), TIMEOUT.as_millis() as u32);
            }
        }
    }
}

fn quote_arg(arg: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
        } else {
            result.extend(std::iter::repeat_n(
                '\\',
                slashes * if ch == '"' { 2 } else { 1 },
            ));
            if ch == '"' {
                result.push('\\');
            }
            result.push(ch);
            slashes = 0;
        }
    }
    result.extend(std::iter::repeat_n('\\', slashes * 2));
    result.push('"');
    result
}

struct PipedChild {
    child: Child,
    output: Capture,
}

impl PipedChild {
    fn spawn(args: &[String]) -> Self {
        let mut child = Command::new(EXE)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("direct piped CLI");
        let (output, tx) = Capture::new();
        drain(child.stdout.take().expect("stdout"), tx.clone());
        drain(child.stderr.take().expect("stderr"), tx);
        Self { child, output }
    }

    fn answer(&mut self, answer: &[u8]) {
        self.child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(answer)
            .expect("piped answer");
    }

    fn finish(&mut self) -> (i32, String) {
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait for piped CLI") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not exit: {}",
                self.output.text()
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        (status.code().expect("CLI exit code"), self.output.finish())
    }
}

impl Drop for PipedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_version(text: &str) {
    assert!(
        text.contains(&format!("Bifrost {}", env!("CARGO_PKG_VERSION"))),
        "{text:?}"
    );
}

fn assert_missing_core(text: &str) {
    assert!(
        text.contains("bifrost:"),
        "missing CLI diagnostic: {text:?}"
    );
    assert!(
        text.contains("os error 2"),
        "expected isolated root's absent named pipe: {text:?}"
    );
}

fn native_command(application: &str, args: &[String]) -> String {
    std::iter::once(application)
        .chain(args.iter().map(String::as_str))
        .map(quote_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn spawn_shell(args: &[String]) -> PtyChild {
    let application = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
        .join("System32")
        .join("cmd.exe");
    let application = application.to_str().expect("UTF-8 shell path");
    let command = format!(
        "{} /d /s /c \"{}\"",
        quote_arg(application),
        native_command(EXE, args)
    );
    PtyChild::spawn(application, &command)
}

#[test]
fn direct_gui_subsystem_cli_without_parent_console_fails_explicitly() {
    const DETACHED: &str = "BIFROST_CONSOLE_TEST_DETACHED_PARENT";
    if std::env::var_os(DETACHED).is_none() {
        // The regular CUI test runner can own a console. A detached copy supplies the actual
        // unsupported parent topology without changing the runner's process-global console.
        let output = Command::new(std::env::current_exe().expect("integration test executable"))
            .args([
                "--exact",
                "direct_gui_subsystem_cli_without_parent_console_fails_explicitly",
                "--nocapture",
            ])
            .env(DETACHED, "1")
            .creation_flags(DETACHED_PROCESS)
            .output()
            .expect("detached integration test host");
        assert!(output.status.success(), "{output:?}");
        return;
    }
    for rest in [
        &["--version"][..],
        &["status"][..],
        &["pair", "--open", "--listen", "127.0.0.1:0"][..],
    ] {
        let root = Root::new();
        let args = root.args(rest);
        let mut child = PtyChild::spawn(EXE, &native_command(EXE, &args));
        let (code, text) = child.finish();
        assert_eq!(code, 69, "unsupported direct GUI-subsystem launch: {text}");
        let log = std::fs::read_to_string(root.0.join("state/logs/bifrost-core.log"))
            .expect("console diagnostic in isolated core log");
        assert!(
            log.contains("CLI command needs a console: run it from cmd/PowerShell or via cmd /c."),
            "{log}"
        );
        assert!(
            !root.0.join("config/identity.dpapi").exists(),
            "unattached CLI must not initialize identity"
        );
    }
}

#[test]
fn cmd_pty_version_and_status() {
    let root = Root::new();
    let mut version = spawn_shell(&root.args(&["--version"]));
    let (code, text) = version.finish();
    assert_eq!(code, 0, "{text}");
    assert_version(&text);
    let mut status = spawn_shell(&root.args(&["status"]));
    let (code, text) = status.finish();
    assert_eq!(code, 1, "{text}");
    assert_missing_core(&text);
}

#[test]
fn redirected_version_and_status_preserve_stdout_and_stderr() {
    let root = Root::new();
    let version = Command::new(EXE)
        .args(root.args(&["--version"]))
        .output()
        .expect("version");
    assert_eq!(version.status.code(), Some(0));
    assert_version(&String::from_utf8_lossy(&version.stdout));
    assert!(version.stderr.is_empty(), "{version:?}");
    let status = Command::new(EXE)
        .args(root.args(&["status"]))
        .output()
        .expect("status");
    assert_eq!(status.status.code(), Some(1));
    assert!(status.stdout.is_empty(), "{status:?}");
    assert_missing_core(&String::from_utf8_lossy(&status.stderr));
}

fn reject_pair(pty_dials: bool) {
    let opener_root = Root::new();
    let dialer_root = Root::new();
    // Hold every reservation together, avoiding both collisions and all installed-service ports.
    let reservations: Vec<_> = (0..4)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("reserve port"))
        .collect();
    let ports: Vec<_> = reservations
        .iter()
        .map(|listener| listener.local_addr().expect("address").port())
        .collect();
    assert!(ports
        .iter()
        .all(|port| ![47901, 47902, 47903].contains(port)));
    opener_root.configure_pair(ports[0], ports[1]);
    dialer_root.configure_pair(ports[2], ports[3]);
    let address = format!("127.0.0.1:{}", ports[0]);
    let open = opener_root.args(&["pair", "--open", "--listen", &address]);
    let dial = dialer_root.args(&["pair", "--dial", &address]);
    drop(reservations);

    if pty_dials {
        let mut opener = PipedChild::spawn(&open);
        opener.output.until("waiting for a peer to dial");
        let mut dialer = spawn_shell(&dial);
        opener.output.until(PROMPT);
        dialer.output.until(PROMPT);
        opener.answer(b"y\n");
        // Keep stdin open: EOF must not accidentally satisfy the negative-answer regression.
        dialer.answer(b"n\r");
        let (code, text) = dialer.finish();
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("pairing rejected: UserRejected"), "{text}");
        let (code, text) = opener.finish();
        assert_eq!(code, 1, "the approving peer must observe rejection: {text}");
    } else {
        let mut opener = spawn_shell(&open);
        opener.output.until("waiting for a peer to dial");
        let mut dialer = PipedChild::spawn(&dial);
        opener.output.until(PROMPT);
        dialer.output.until(PROMPT);
        opener.answer(b"y\r");
        dialer.answer(b"n\n");
        let (code, text) = dialer.finish();
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("pairing rejected: UserRejected"), "{text}");
        let (code, text) = opener.finish();
        assert_eq!(code, 1, "the approving peer must observe rejection: {text}");
    }
    for root in [&opener_root, &dialer_root] {
        assert!(
            !root.0.join("config/trust.toml").exists(),
            "declining must not write trust"
        );
    }
}

#[test]
fn cmd_pty_pair_prompt_accepts_negative_answer() {
    reject_pair(true);
}

#[test]
fn redirected_pair_prompt_accepts_negative_answer() {
    reject_pair(false);
}
