use std::ffi::OsString;
use std::io;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Gui,
    Tray,
}
impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Gui => "gui",
            Self::Tray => "tray",
        }
    }
}

pub enum InstanceOutcome {
    Started(InstanceGuard),
    Existing,
}

/// Keep this guard alive for the entire role lifetime. OS locks survive neither
/// normal exit nor a crash; configuration paths never affect instance identity.
pub struct InstanceGuard {
    platform: platform::Guard,
    stop: Arc<AtomicBool>,
    listener: Mutex<Option<JoinHandle<()>>>,
}

/// An existing GUI is signalled to show before returning `Existing`.
pub fn acquire(role: Role) -> io::Result<InstanceOutcome> {
    // Keep the installed build's lock namespace so an old process is still detected.
    acquire_named(role, "multideskflow", true)
}
fn acquire_named(role: Role, namespace: &str, signal: bool) -> io::Result<InstanceOutcome> {
    match platform::acquire(role, namespace)? {
        Some(platform) => Ok(InstanceOutcome::Started(InstanceGuard {
            platform,
            stop: Arc::new(AtomicBool::new(false)),
            listener: Mutex::new(None),
        })),
        None => {
            if role == Role::Gui && signal {
                signal_named(namespace)?;
            }
            Ok(InstanceOutcome::Existing)
        }
    }
}
impl InstanceGuard {
    /// Install once after creating the GUI context. The callback runs on a
    /// background thread and must dispatch focus/show commands to that context.
    pub fn set_show_handler(&self, callback: impl Fn() + Send + 'static) -> io::Result<()> {
        let mut listener = self
            .listener
            .lock()
            .map_err(|_| io::Error::other("show listener poisoned"))?;
        if listener.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "show handler already installed",
            ));
        }
        *listener = Some(self.platform.listen(self.stop.clone(), callback)?);
        Ok(())
    }
}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.platform.wake();
        if let Ok(listener) = self.listener.get_mut() {
            if let Some(thread) = listener.take() {
                let _ = thread.join();
            }
        }
    }
}

/// The tray's "my icon is on screen" flag, raised by the tray itself once the desktop really
/// shows it (a StatusNotifierWatcher took the item, the taskbar accepted the icon) and lowered
/// when the icon goes away. A running tray process alone proves nothing: on a desktop whose
/// tray host is not up yet it runs and shows nothing.
#[derive(Clone)]
pub struct TrayReady(Arc<platform::Ready>);
impl TrayReady {
    pub fn set(&self, ready: bool) {
        self.0.set(ready);
    }
}
impl InstanceGuard {
    /// The readiness flag of a tray guard; `None` for any other role.
    pub fn tray_ready(&self) -> Option<TrayReady> {
        self.platform.ready().map(TrayReady)
    }
}

/// Whether a live tray of this user has its icon on screen right now.
pub fn tray_is_ready() -> bool {
    tray_is_ready_named("multideskflow")
}
fn tray_is_ready_named(namespace: &str) -> bool {
    platform::tray_is_ready(namespace).unwrap_or(false)
}

/// How long a launcher waits for the tray's icon before it tells the user it is missing.
pub const TRAY_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Makes sure the tray icon is on screen: starts the tray unless it is already showing, and
/// waits up to `timeout` for the icon. `Err` means there is no icon, and the caller must not
/// leave the user with nothing (keep its window, say why).
pub fn start_tray(args: &[OsString], timeout: Duration) -> io::Result<()> {
    start_tray_from(&std::env::current_exe()?, args, timeout)
}

/// Start the tray from a verified replacement path, even when the caller's image was unlinked.
pub fn start_tray_from(
    exe: &std::path::Path,
    args: &[OsString],
    timeout: Duration,
) -> io::Result<()> {
    if tray_is_ready() {
        return Ok(());
    }
    spawn_role_from(exe, Role::Tray, args)?;
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if tray_is_ready() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the tray icon did not appear",
    ))
}
/// Ask the old tray to remove its icon and exit before an updater restart.
pub fn stop_tray() -> io::Result<()> {
    platform::signal_role("multideskflow", Role::Tray)
}
/// Probe the instance lock, not icon readiness: an icon can vanish before its process exits.
pub fn tray_is_running() -> bool {
    !matches!(platform::acquire(Role::Tray, "multideskflow"), Ok(Some(_)))
}
/// Stop an existing tray and wait for its instance lock, not only its icon, to disappear.
pub fn stop_tray_and_wait(timeout: Duration) -> io::Result<bool> {
    if !tray_is_running() {
        return Ok(false);
    }
    if let Err(error) = stop_tray() {
        if tray_is_running() {
            return Err(error);
        }
    }
    let deadline = std::time::Instant::now() + timeout;
    while tray_is_running() {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the old tray did not exit",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(true)
}
pub fn signal_existing_gui() -> io::Result<()> {
    signal_named("multideskflow")
}
fn signal_named(namespace: &str) -> io::Result<()> {
    // A second launch can acquire the existence result before the first launch
    // finishes publishing its endpoint. Bound that startup race to half a second.
    for attempt in 0..50 {
        match platform::signal_role(namespace, Role::Gui) {
            Ok(()) => return Ok(()),
            Err(error) if attempt == 49 => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    unreachable!()
}

/// Start another role of the current executable without inheriting stdio.
/// `args` are the original global arguments, not a role subcommand.
pub fn spawn_role(role: Role, args: &[OsString]) -> io::Result<()> {
    spawn_role_from(&std::env::current_exe()?, role, args)
}

fn role_command(exe: &std::path::Path, role: Role, args: &[OsString]) -> Command {
    let mut command = Command::new(exe);
    command.arg(role.name()).args(args);
    command
}

fn spawn_role_from(exe: &std::path::Path, role: Role, args: &[OsString]) -> io::Result<()> {
    let mut command = role_command(exe, role, args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe libc calls run in the forked child; no Rust state is touched.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                match libc::fork() {
                    -1 => Err(io::Error::last_os_error()),
                    0 => Ok(()),
                    // The role is adopted by init. The launching core may exec immediately,
                    // so it must not own a long-lived child or a Rust reaper thread.
                    _ => libc::_exit(0),
                }
            });
        }
    }
    let mut child = command.spawn()?;
    #[cfg(unix)]
    {
        let status = child.wait()?;
        if !status.success() {
            return Err(io::Error::other(format!("role launcher exited: {status}")));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &mut child;
    }
    Ok(())
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    };
    use std::path::{Path, PathBuf};

    /// `$XDG_RUNTIME_DIR` (or its systemd default `/run/user/<uid>`) when it is there; `/tmp`
    /// only as the fallback. The runtime dir is private to the user and cleared at logout, so a
    /// stale lock or socket cannot outlive the session and nobody else can pre-create the name.
    pub(super) fn directory(namespace: &str) -> io::Result<PathBuf> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                // SAFETY: geteuid has no preconditions.
                let uid = unsafe { libc::geteuid() };
                Some(PathBuf::from(format!("/run/user/{uid}")))
            });
        directory_in(runtime.as_deref(), Path::new("/tmp"), namespace)
    }

    /// A runtime directory is trusted only when it is an absolute, real directory that this
    /// user owns and nobody else can enter; it is never created here.
    fn trusted_runtime_dir(path: &Path, uid: u32) -> bool {
        path.is_absolute()
            && fs::symlink_metadata(path).is_ok_and(|metadata| {
                metadata.is_dir()
                    && metadata.uid() == uid
                    && metadata.permissions().mode() & 0o077 == 0
            })
    }

    pub(super) fn directory_in(
        runtime: Option<&Path>,
        fallback: &Path,
        namespace: &str,
    ) -> io::Result<PathBuf> {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let path = match runtime.filter(|runtime| trusted_runtime_dir(runtime, uid)) {
            Some(runtime) => runtime.join(namespace),
            None => fallback.join(format!("{namespace}-{uid}")),
        };
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe instance directory",
            ));
        }
        Ok(path)
    }
    /// The tray's "icon is on screen" flag: a file holding the tray's pid.
    pub struct Ready {
        path: PathBuf,
    }
    impl Ready {
        pub fn set(&self, ready: bool) {
            if ready {
                let _ = fs::write(&self.path, std::process::id().to_string());
            } else {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
    pub struct Guard {
        _lock: File,
        socket: Option<(UnixListener, PathBuf)>,
        ready: Option<Arc<Ready>>,
    }
    pub fn acquire(role: Role, namespace: &str) -> io::Result<Option<Guard>> {
        let directory = directory(namespace)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(directory.join(format!("{}.lock", role.name())))?;
        // SAFETY: the fd is live and flock borrows it only for this call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error);
        }
        let socket = {
            let path = directory.join(format!("{}.sock", role.name()));
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let listener = UnixListener::bind(&path)?;
            Some((listener, path))
        };
        let ready = if role == Role::Tray {
            // Holding the lock, so whatever marker is here belongs to a dead tray.
            let ready = Ready {
                path: directory.join("tray.ready"),
            };
            ready.set(false);
            Some(Arc::new(ready))
        } else {
            None
        };
        Ok(Some(Guard {
            _lock: lock,
            socket,
            ready,
        }))
    }
    impl Guard {
        pub fn ready(&self) -> Option<Arc<Ready>> {
            self.ready.clone()
        }
        pub fn wake(&self) {
            if let Some((listener, _)) = &self.socket {
                // SAFETY: the listener owns this live fd. Shutdown wakes blocking
                // accept on its cloned fd without queueing a synthetic show.
                unsafe {
                    libc::shutdown(listener.as_raw_fd(), libc::SHUT_RDWR);
                }
            }
        }
        pub fn listen(
            &self,
            stop: Arc<AtomicBool>,
            callback: impl Fn() + Send + 'static,
        ) -> io::Result<JoinHandle<()>> {
            let listener = self
                .socket
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "tray has no show listener")
                })?
                .0
                .try_clone()?;
            thread::Builder::new()
                .name("gui-show".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                if stop.load(Ordering::Acquire) {
                                    break;
                                }
                                let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                                let mut message = [0; 5];
                                if stream.read_exact(&mut message).is_ok() && &message == b"show\n"
                                {
                                    callback();
                                }
                            }
                            Err(_) => break,
                        }
                    }
                })
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(ready) = &self.ready {
                ready.set(false);
            }
            if let Some((_, path)) = &self.socket {
                let _ = fs::remove_file(path);
            }
        }
    }
    /// The marker holds the tray's pid: a crash leaves the file behind, and a pid that is not
    /// alive any more must not make a launcher believe an icon exists. (No lock probing: a
    /// probe could make a tray that is starting at that moment see "already running".)
    pub fn tray_is_ready(namespace: &str) -> io::Result<bool> {
        let text = match fs::read_to_string(directory(namespace)?.join("tray.ready")) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let Ok(pid) = text.trim().parse::<libc::pid_t>() else {
            return Ok(false);
        };
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        Ok(alive && pid > 0)
    }
    pub fn signal_role(namespace: &str, role: Role) -> io::Result<()> {
        UnixStream::connect(directory(namespace)?.join(format!("{}.sock", role.name())))?
            .write_all(b"show\n")
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0},
        Security::{GetLengthSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER},
        System::Threading::{
            CreateEventW, CreateMutexW, GetCurrentProcess, OpenEventW, OpenProcessToken,
            ResetEvent, SetEvent, WaitForSingleObject, EVENT_ALL_ACCESS, EVENT_MODIFY_STATE,
            INFINITE,
        },
        UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY},
    };
    struct Handle(HANDLE);
    // SAFETY: these kernel handles can be waited/signalled/closed across threads.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn handle(raw: HANDLE) -> io::Result<Handle> {
        if raw.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(raw))
        }
    }
    pub(super) fn name(namespace: &str, role: Role, suffix: &str) -> io::Result<Vec<u16>> {
        let mut token = null_mut();
        // SAFETY: output storage is valid and the current process pseudo-handle is valid.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = handle(token)?;
        let mut size = 0;
        unsafe {
            GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut size);
        }
        // usize storage provides the alignment required by TOKEN_USER.
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                size,
                &mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        let sid = unsafe {
            std::slice::from_raw_parts(
                user.User.Sid.cast::<u8>(),
                GetLengthSid(user.User.Sid) as usize,
            )
        };
        let mut identity = String::new();
        use std::fmt::Write;
        for byte in sid {
            let _ = write!(identity, "{byte:02x}");
        }
        Ok(
            format!("Local\\{namespace}-{identity}-{}-{suffix}", role.name())
                .encode_utf16()
                .chain(Some(0))
                .collect(),
        )
    }
    /// The tray's "icon is on screen" flag.
    pub struct Ready(Handle);
    impl Ready {
        pub fn set(&self, ready: bool) {
            // SAFETY: the handle is live for as long as the marker is.
            unsafe {
                if ready {
                    SetEvent(self.0 .0);
                } else {
                    ResetEvent(self.0 .0);
                }
            }
        }
    }
    pub struct Guard {
        _mutex: Handle,
        event: Option<Arc<Handle>>,
        ready: Option<Arc<Ready>>,
    }
    pub fn acquire(role: Role, namespace: &str) -> io::Result<Option<Guard>> {
        let mutex_name = name(namespace, role, "instance")?;
        let raw = unsafe { CreateMutexW(null(), 0, mutex_name.as_ptr()) };
        let existing = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = handle(raw)?;
        if existing {
            return Ok(None);
        }
        let ready = if role == Role::Tray {
            // Manual reset: the tray raises it once its icon is on screen and lowers it when
            // the icon goes away; a launcher only ever looks at it.
            let ready_name = name(namespace, role, "ready")?;
            Some(Arc::new(Ready(handle(unsafe {
                CreateEventW(null(), 1, 0, ready_name.as_ptr())
            })?)))
        } else {
            None
        };
        let event = {
            let event_name = name(namespace, role, "show")?;
            Some(Arc::new(handle(unsafe {
                CreateEventW(null(), 0, 0, event_name.as_ptr())
            })?))
        };
        Ok(Some(Guard {
            _mutex: mutex,
            event,
            ready,
        }))
    }
    impl Guard {
        pub fn ready(&self) -> Option<Arc<Ready>> {
            self.ready.clone()
        }
        pub fn wake(&self) {
            if let Some(event) = &self.event {
                // SAFETY: the guard keeps the event handle live.
                unsafe {
                    SetEvent(event.0);
                }
            }
        }
        pub fn listen(
            &self,
            stop: Arc<AtomicBool>,
            callback: impl Fn() + Send + 'static,
        ) -> io::Result<JoinHandle<()>> {
            let event = self.event.clone().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "tray has no show listener")
            })?;
            thread::Builder::new()
                .name("gui-show".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if unsafe { WaitForSingleObject(event.0, INFINITE) } != WAIT_OBJECT_0 {
                            break;
                        }
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        callback();
                    }
                })
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            // Clones of the marker outlive the guard (the tray keeps one inside its icon
            // service); the flag must not.
            if let Some(ready) = &self.ready {
                ready.set(false);
            }
        }
    }
    pub fn tray_is_ready(namespace: &str) -> io::Result<bool> {
        let name = name(namespace, Role::Tray, "ready")?;
        // The event dies with the tray process, so a crashed tray is never "ready".
        let raw = unsafe { OpenEventW(EVENT_ALL_ACCESS, 0, name.as_ptr()) };
        if raw.is_null() {
            return Ok(false);
        }
        let event = Handle(raw);
        Ok(unsafe { WaitForSingleObject(event.0, 0) } == WAIT_OBJECT_0)
    }
    pub fn signal_role(namespace: &str, role: Role) -> io::Result<()> {
        let name = name(namespace, role, "show")?;
        let event = handle(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) })?;
        // A foreground launcher can grant the existing GUI activation rights.
        // Background launches may lack those rights; showing still must work.
        // SAFETY: ASFW_ANY is a documented process-id sentinel.
        unsafe {
            AllowSetForegroundWindow(ASFW_ANY);
        }
        if unsafe { SetEvent(event.0) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn kernel_names_are_per_session_not_global() {
        // `Global\` would make a second logon session of the same user believe the first one's
        // window and tray are its own, and start nothing.
        let name = platform::name("multideskflow", Role::Gui, "show").unwrap();
        let text = String::from_utf16(&name[..name.len() - 1]).unwrap();
        assert!(text.starts_with("Local\\multideskflow-"), "{text}");
        assert!(text.ends_with("-gui-show"), "{text}");
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("lws-dir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }

    #[test]
    fn the_instance_directory_lives_in_the_private_runtime_dir() {
        let runtime = scratch("runtime");
        let fallback = scratch("fallback-a");
        let dir = platform::directory_in(Some(&runtime), &fallback, "ns").unwrap();
        assert_eq!(dir, runtime.join("ns"));
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(std::fs::read_dir(&fallback).unwrap().next().is_none());
    }

    #[test]
    fn a_marker_left_by_a_dead_tray_is_not_readiness() {
        let namespace = format!("lws-stale-test-{}", std::process::id());
        let marker = platform::directory(&namespace).unwrap().join("tray.ready");
        // A pid that cannot be alive: what a crashed tray leaves behind.
        std::fs::write(&marker, i32::MAX.to_string()).unwrap();
        assert!(!tray_is_ready_named(&namespace));
        std::fs::write(&marker, std::process::id().to_string()).unwrap();
        assert!(tray_is_ready_named(&namespace));
        std::fs::remove_file(&marker).unwrap();
    }

    #[test]
    fn a_missing_or_untrusted_runtime_dir_falls_back_to_the_per_uid_tmp_directory() {
        let fallback = scratch("fallback-b");
        let missing = fallback.join("does-not-exist");
        let dir = platform::directory_in(Some(&missing), &fallback, "ns").unwrap();
        assert!(dir.starts_with(&fallback) && dir.file_name().unwrap() != "ns");
        assert!(!missing.exists(), "the runtime dir must never be created");

        let shared = scratch("shared");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = platform::directory_in(Some(&shared), &fallback, "ns").unwrap();
        assert!(
            dir.starts_with(&fallback),
            "a group/world-accessible dir is not trusted"
        );
        assert!(!shared.join("ns").exists());

        let none = platform::directory_in(None, Path::new(&fallback), "ns").unwrap();
        assert_eq!(none, dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_role_command_uses_verified_exe_and_preserves_path_arguments() {
        let exe = std::path::Path::new("new installation/bifrost");
        let args = [OsString::from("--root"), OsString::from("root folder")];
        let command = role_command(exe, Role::Tray, &args);
        assert_eq!(command.get_program(), exe.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["tray", "--root", "root folder"]
        );
    }
    #[test]
    fn role_identity_and_existing_instance_decision() {
        assert_ne!(Role::Gui.name(), Role::Tray.name());
    }
    #[test]
    fn exclusive_roles_release_and_show_headlessly() {
        let namespace = format!("lws-instance-test-{}", std::process::id());
        let InstanceOutcome::Started(gui) = acquire_named(Role::Gui, &namespace, false).unwrap()
        else {
            panic!("first GUI must start")
        };
        assert!(matches!(
            acquire_named(Role::Gui, &namespace, false).unwrap(),
            InstanceOutcome::Existing
        ));
        let InstanceOutcome::Started(tray) = acquire_named(Role::Tray, &namespace, false).unwrap()
        else {
            panic!("roles must be independent")
        };
        let (send, receive) = std::sync::mpsc::channel();
        gui.set_show_handler(move || {
            send.send(()).unwrap();
        })
        .unwrap();
        assert!(matches!(
            acquire_named(Role::Gui, &namespace, true).unwrap(),
            InstanceOutcome::Existing
        ));
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(gui);
        assert!(matches!(
            acquire_named(Role::Gui, &namespace, false).unwrap(),
            InstanceOutcome::Started(_)
        ));
        drop(tray);
    }

    #[test]
    fn tray_readiness_means_an_icon_on_screen_and_dies_with_the_tray() {
        let namespace = format!("lws-ready-test-{}", std::process::id());
        assert!(!tray_is_ready_named(&namespace), "no tray, not ready");
        let InstanceOutcome::Started(tray) = acquire_named(Role::Tray, &namespace, false).unwrap()
        else {
            panic!("the tray must start")
        };
        assert!(
            !tray_is_ready_named(&namespace),
            "a running tray without an icon (no watcher, no taskbar yet) is not ready"
        );
        let ready = tray.tray_ready().expect("a tray guard carries the marker");
        ready.set(true);
        assert!(tray_is_ready_named(&namespace));
        ready.set(false);
        assert!(!tray_is_ready_named(&namespace), "the watcher went away");
        ready.set(true);
        drop(tray);
        assert!(
            !tray_is_ready_named(&namespace),
            "a stopped tray is not ready"
        );
    }

    #[test]
    fn only_the_tray_carries_a_readiness_marker() {
        let namespace = format!("lws-ready-gui-test-{}", std::process::id());
        let InstanceOutcome::Started(gui) = acquire_named(Role::Gui, &namespace, false).unwrap()
        else {
            panic!("the GUI must start")
        };
        assert!(gui.tray_ready().is_none());
    }
}
