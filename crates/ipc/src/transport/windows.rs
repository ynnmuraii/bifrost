//! Windows named-pipe endpoints.
//!
//! `docs/03-architecture.md`, «Транспорт и аутентификация»:
//!
//! | | Windows |
//! |---|---|
//! | core ↔ agent | Анонимный pipe-пара, унаследованная при `CreateProcess` (без имени в namespace) |
//! | gui ↔ core | Named pipe `\\.\pipe\lan-workspace-<SID>`, DACL: только SID текущего пользователя, `PIPE_REJECT_REMOTE_CLIENTS` |
//!
//! [`listen`] implements the GUI row exactly: the pipe is created with an
//! explicit security descriptor converted from the SDDL
//! `D:P(A;;GA;;;<current user SID>)`, and every instance is created with
//! `PIPE_REJECT_REMOTE_CLIENTS` (tokio's `reject_remote_clients`). The first
//! instance also carries `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a process that
//! squatted the name first makes `listen` fail instead of being served.
//!
//! # Deviation from the documents, and why
//!
//! The agent channel is an *anonymous* pipe pair in the documents: nothing in
//! the namespace to connect to. Anonymous pipes are created by `CreatePipe`,
//! which produces synchronous handles, and Windows offers no overlapped I/O on
//! them — a tokio endpoint cannot wrap them. [`agent_pair`] therefore creates a
//! *named* pair with a per-launch name, the same user-only DACL,
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` and `max_instances = 1`, which preserves the
//! properties the documents care about (nobody can pre-create or join the name,
//! and the 32-byte launch token in `AgentAttach` remains the second check), at
//! the cost of a name that briefly exists in the pipe namespace. A caller that
//! must have the anonymous pair can hand its own blocking wrapper of the
//! inherited handle to [`crate::framed::Framed`] instead.
//!
//! # Unsafe
//!
//! Three calls are unavoidable here: `create_with_security_attributes_raw`
//! (unsafe by API contract), the security-descriptor conversion, and the
//! current-user SID lookup. Each is confined to a small function with a SAFETY
//! comment, and the pointer they produce is owned by [`PipeSecurity`], which
//! frees it in `Drop`.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions,
};

use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::error::IpcError;
use crate::peer::{PeerCheck, PeerCredentials};

/// Prefix of every Bifrost pipe name.
pub const PIPE_NAME_PREFIX: &str = "lan-workspace-";
/// The `\\.\pipe\` namespace prefix.
pub const PIPE_PATH_PREFIX: &str = r"\\.\pipe\";
/// `ERROR_PIPE_BUSY`: every instance is connected, retry shortly.
const ERROR_PIPE_BUSY: i32 = 231;
/// `ERROR_FILE_NOT_FOUND`: the name has no instance at all right now, retry shortly.
///
/// Windows returns it from `CreateFile` on a pipe name that exists but has nothing listening —
/// the instant between the listener's accepted instance leaving and the next one arriving.
const ERROR_FILE_NOT_FOUND: i32 = 2;
/// Largest instance count `CreateNamedPipeW` accepts (254); 255 is its
/// "unlimited" sentinel and the tokio builder rejects it.
const MAX_PIPE_INSTANCES: u32 = 254;
/// Default pipe buffer, matching the maximum IPC frame plus a frame in flight.
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
/// How long [`connect`] keeps trying a pipe that has no instance for it.
///
/// Both retryable errors clear in microseconds when the listener is healthy — the next
/// instance is created before the accepted one leaves [`GuiListener::accept`] — so the window
/// only has to outlast a busy machine: a core whose loop is descheduled for a moment must not
/// lose a client that would have been served. Two seconds is the CLI's own patience for a
/// whole exchange (`apps/lws-core/src/main.rs`), so a client never waits longer than the
/// caller was prepared to.
const CONNECT_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
/// Delay between retries of a busy or briefly-absent pipe.
const CONNECT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

/// `\\.\pipe\lan-workspace-<SID>`.
pub fn gui_pipe_name(sid: &str) -> String {
    format!("{PIPE_PATH_PREFIX}{PIPE_NAME_PREFIX}{sid}")
}

/// The DACL of the GUI pipe: full access for the current user's SID only.
///
/// `D:P` is a protected DACL (no inherited ACEs), `A;;GA;;;<SID>` grants generic
/// all to that SID, and nothing else is listed — which is what «DACL: только SID
/// текущего пользователя» asks for.
pub fn gui_sddl(sid: &str) -> String {
    format!("D:P(A;;GA;;;{sid})")
}

/// The string form of the current user's SID, e.g. `S-1-5-21-…-1001`.
pub fn current_user_sid_string() -> Result<String, IpcError> {
    unsafe { current_user_sid() }
        .map_err(|source| IpcError::io("reading the current user SID", source))
}

unsafe fn current_user_sid() -> io::Result<String> {
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no close;
    // `OpenProcessToken` writes a real handle into `token`, closed by the guard.
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(io::Error::other)?;
    let guard = TokenHandle(token);

    let mut size = 0u32;
    // SAFETY: a null buffer with length 0 is the documented way to ask for the
    // required size; the call fails with ERROR_INSUFFICIENT_BUFFER and sets it.
    let _ = unsafe { GetTokenInformation(guard.0, TokenUser, None, 0, &mut size) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut buffer = vec![0u8; size as usize];
    // SAFETY: `buffer` is at least `size` bytes, which is what the call writes.
    unsafe {
        GetTokenInformation(
            guard.0,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut c_void),
            size,
            &mut size,
        )
    }
    .map_err(io::Error::other)?;

    // SAFETY: the call filled `buffer` with a TOKEN_USER. `read_unaligned` is
    // required because a `Vec<u8>` is only 1-byte aligned.
    let user: TOKEN_USER =
        unsafe { std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER) };

    let mut raw = PWSTR::null();
    // SAFETY: `user.User.Sid` is the SID inside the buffer, which is still alive.
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut raw) }.map_err(io::Error::other)?;
    // SAFETY: on success the call allocated a NUL-terminated wide string.
    let text = unsafe { raw.to_string() }.map_err(io::Error::other)?;
    // SAFETY: the string was allocated by the API and is ours to free.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(raw.0 as *mut c_void)));
    }
    Ok(text)
}

/// Closes the process token handle.
struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcessToken and is closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Shape of the pipes this process creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeOptions {
    /// Instances that may exist at once.
    pub max_instances: u32,
    /// Input buffer size in bytes.
    pub in_buffer_size: u32,
    /// Output buffer size in bytes.
    pub out_buffer_size: u32,
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`: fail if the name already exists.
    pub first_instance: bool,
    /// Whether a child may inherit the handle.
    pub inherit_handle: bool,
}

impl PipeOptions {
    /// The GUI serves one client at a time, but a spare instance is kept ready so
    /// the next `accept` does not race with a connecting client. The instance
    /// count is only a resource cap — access is decided by the DACL.
    pub const fn gui() -> Self {
        PipeOptions {
            max_instances: MAX_PIPE_INSTANCES,
            in_buffer_size: PIPE_BUFFER_BYTES,
            out_buffer_size: PIPE_BUFFER_BYTES,
            first_instance: true,
            inherit_handle: false,
        }
    }

    /// A single-instance pair for the agent channel.
    pub const fn agent_pair() -> Self {
        PipeOptions {
            max_instances: 1,
            in_buffer_size: PIPE_BUFFER_BYTES,
            out_buffer_size: PIPE_BUFFER_BYTES,
            first_instance: true,
            inherit_handle: false,
        }
    }

    /// The same options with the first-instance flag set as given.
    pub const fn with_first_instance(self, first: bool) -> Self {
        PipeOptions {
            first_instance: first,
            ..self
        }
    }

    fn server_options(&self) -> ServerOptions {
        let mut options = ServerOptions::new();
        options
            .pipe_mode(PipeMode::Byte)
            .reject_remote_clients(true)
            .access_inbound(true)
            .access_outbound(true)
            .max_instances(self.max_instances as usize)
            .in_buffer_size(self.in_buffer_size)
            .out_buffer_size(self.out_buffer_size)
            .first_pipe_instance(self.first_instance);
        options
    }
}

/// A security descriptor built from an SDDL string, owned until dropped.
///
/// Holding the descriptor is what makes the raw pointer handed to
/// `CreateNamedPipeW` valid; [`PipeSecurity::create`] is the only safe way to use
/// it.
#[derive(Debug)]
pub struct PipeSecurity {
    sddl: String,
    attributes: SECURITY_ATTRIBUTES,
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the security descriptor is heap memory owned solely by this value, it
// is never mutated after construction, and moving or sharing the value only
// moves that ownership. `SECURITY_ATTRIBUTES` itself is plain data.
unsafe impl Send for PipeSecurity {}
// SAFETY: as above; `&PipeSecurity` only ever yields a read-only pointer.
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    /// Build the descriptor for an explicit SID.
    pub fn for_user_sid(sid: &str) -> Result<Self, IpcError> {
        let sddl = gui_sddl(sid);
        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the
        // call, and `descriptor` receives a freshly allocated self-relative
        // security descriptor owned by this value until Drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|error| IpcError::io("converting the pipe SDDL", io::Error::other(error)))?;

        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: BOOL(0),
        };
        Ok(PipeSecurity {
            sddl,
            attributes,
            descriptor,
        })
    }

    /// Build the descriptor for the current user.
    pub fn for_current_user() -> Result<Self, IpcError> {
        PipeSecurity::for_user_sid(&current_user_sid_string()?)
    }

    /// The SDDL this descriptor was built from, for logs and diagnostics.
    pub fn sddl(&self) -> &str {
        &self.sddl
    }

    /// Create one pipe instance under this descriptor.
    pub fn create(&self, name: &str, options: &PipeOptions) -> Result<NamedPipeServer, IpcError> {
        let server_options = options.server_options();
        // SAFETY: `attrs` points at `self.attributes`, whose
        // `lpSecurityDescriptor` stays valid for as long as `self` lives;
        // `CreateNamedPipeW` only reads the structure.
        let server = unsafe {
            server_options.create_with_security_attributes_raw(
                name,
                &self.attributes as *const SECURITY_ATTRIBUTES as *mut c_void,
            )
        };
        server.map_err(|source| IpcError::io("creating the named pipe", source))
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.0.is_null() {
            // SAFETY: the descriptor was allocated by the SDDL conversion and is
            // freed exactly once, here.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.descriptor.0)));
            }
        }
    }
}

/// Where a named pipe lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    name: String,
}

impl Endpoint {
    /// An endpoint with an explicit pipe name.
    pub fn pipe(name: impl Into<String>) -> Self {
        Endpoint { name: name.into() }
    }

    /// `\\.\pipe\lan-workspace-<SID>` for an explicit SID.
    pub fn gui_pipe_for_sid(sid: &str) -> Self {
        Endpoint::pipe(gui_pipe_name(sid))
    }

    /// `\\.\pipe\lan-workspace-<SID>` for the current user.
    ///
    /// Core should prefer the name built from `config::Paths`, which owns the
    /// per-OS layout; this constructor is for callers without configuration.
    pub fn gui() -> Result<Self, IpcError> {
        Ok(Endpoint::gui_pipe_for_sid(&current_user_sid_string()?))
    }

    /// The full pipe name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A human-readable form for logs and errors.
    pub fn describe(&self) -> String {
        self.name.clone()
    }
}

/// The listening side of the GUI channel.
#[derive(Debug)]
pub struct GuiListener {
    name: String,
    security: PipeSecurity,
    options: PipeOptions,
    /// The instance that is waiting for the next client.
    ///
    /// `None` only while [`GuiListener::accept`] holds it (a named pipe has one instance per
    /// waiting client, and the instance a client connected to is *not* waiting any more).
    pending: Option<NamedPipeServer>,
}

/// Create the GUI pipe with the current user's DACL and
/// `PIPE_REJECT_REMOTE_CLIENTS`.
///
/// Fails with [`IpcError::AlreadyRunning`] when another process already owns the
/// name. Two shapes of that are possible: with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` the creation is refused outright
/// (`ERROR_ACCESS_DENIED`), while a name whose instances are all taken fails
/// earlier with `ERROR_PIPE_BUSY`. Both mean "somebody else is there".
pub fn listen(endpoint: &Endpoint) -> Result<GuiListener, IpcError> {
    let security = PipeSecurity::for_current_user()?;
    let options = PipeOptions::gui();
    let pending = security
        .create(endpoint.name(), &options)
        .map_err(|error| match error {
            IpcError::Io { source, .. }
                if matches!(source.raw_os_error(), Some(5) | Some(ERROR_PIPE_BUSY)) =>
            {
                IpcError::AlreadyRunning {
                    endpoint: endpoint.describe(),
                }
            }
            other => other,
        })?;
    Ok(GuiListener {
        name: endpoint.name().to_owned(),
        security,
        options,
        pending: Some(pending),
    })
}

impl GuiListener {
    /// The pipe name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The SDDL the DACL was built from.
    pub fn sddl(&self) -> &str {
        self.security.sddl()
    }

    /// Accept one GUI client.
    ///
    /// The kernel already checked the DACL at connect time, so the
    /// [`PeerCheck`] hook sees [`CredentialSource::PipeDacl`] credentials and the
    /// default [`crate::peer::SameUser`] accepts them.
    ///
    /// # The instance the next client needs
    ///
    /// A named pipe has one instance per client it can hold, and a client that finds none
    /// *listening* reads `ERROR_FILE_NOT_FOUND` (no instance at all) or `ERROR_PIPE_BUSY`
    /// (every instance is connected). Both are this listener's bookkeeping, so the next
    /// instance is created *before* the accepted one leaves this method: a client that
    /// arrives while core serves the accepted one finds a pipe that is listening. The
    /// instance a client connected to cannot serve another one, so it is never reused.
    ///
    /// # Cancel safety
    ///
    /// This method is cancel safe: core's session loop polls it as one branch of a
    /// `select!` next to its ticks and agent frames, so it is cancelled most times a client
    /// shows up. The instance that is waiting for the client is taken out of `pending`
    /// *before* the wait and [`PendingInstance`] puts it back if the wait is cancelled —
    /// without that the instance is dropped with a live client on it, and the client reads a
    /// closed pipe instead of its answer («core closed the control channel without an
    /// answer» watched as `lws-core status` failing a few times in two hundred).
    pub async fn accept(&mut self, check: &dyn PeerCheck) -> Result<NamedPipeServer, IpcError> {
        let waiting = self
            .pending
            .take()
            .expect("the listener holds its waiting instance outside `accept`");
        let instance = PendingInstance {
            server: Some(waiting),
            pending: &mut self.pending,
        };
        if let Err(source) = instance.server().connect().await {
            // The wait failed (a closed instance, an invalid handle): an instance whose
            // `ConnectNamedPipe` errored cannot be waited on again — keeping it would leave the
            // channel stuck on a dead pipe with every later client refused — so it goes, and a
            // fresh one takes its place.
            //
            // A client that queued on the instance and left is *not* this case: Windows reports
            // `ERROR_NO_DATA` and mio maps it to a successful connect, so the instance is handed
            // on and the caller's first read or write on the gone client fails, which is where
            // it is dropped.
            instance.discard();
            self.pending = Some(
                self.security
                    .create(&self.name, &self.options.with_first_instance(false))?,
            );
            return Err(IpcError::io("connecting a GUI pipe client", source));
        }
        // The client is on this instance. The next one exists before this one is handed off,
        // so the pipe is never left without an instance that is listening.
        let next = self
            .security
            .create(&self.name, &self.options.with_first_instance(false))?;
        *instance.pending = Some(next);
        let server = instance.take();

        let credentials = PeerCredentials::pipe_dacl();
        check
            .check(&credentials)
            .map_err(|rejected| crate::error::rejected_on(&self.name, rejected))?;
        Ok(server)
    }
}

/// Puts the instance that is waiting for a client back into the listener if the wait is
/// cancelled, and hands it to the caller when the wait finishes.
///
/// A [`NamedPipeServer`] that a client has connected to is the client's only way to be
/// served: dropping it closes the connection under that client. `PendingInstance` exists so a
/// cancelled [`GuiListener::accept`] cannot do that (`mem::take` takes the instance out of the
/// listener before the `await`, which is exactly the window a `select!` cancels in).
struct PendingInstance<'a> {
    server: Option<NamedPipeServer>,
    pending: &'a mut Option<NamedPipeServer>,
}

impl PendingInstance<'_> {
    /// The instance that is waiting for its client.
    fn server(&self) -> &NamedPipeServer {
        self.server
            .as_ref()
            .expect("the instance is owned until `take` or `discard`")
    }

    /// Ends the wait, whichever way it went: the caller owns the instance.
    fn take(mut self) -> NamedPipeServer {
        self.server
            .take()
            .expect("the instance is owned until `take` or `discard`")
    }

    /// The wait failed: the instance is unusable, so dropping it is the point.
    ///
    /// Distinct from a cancellation ([`Drop`]), where a client may already be attached and the
    /// instance must be kept.
    fn discard(mut self) {
        self.server = None;
    }
}

impl Drop for PendingInstance<'_> {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            // Cancelled: a client may already be connected to this instance, so it goes back to
            // the listener for the next `accept` instead of being dropped under that client.
            *self.pending = Some(server);
        }
    }
}

/// Connect to a named pipe endpoint, retrying while the listener has no instance for us.
pub async fn connect(endpoint: &Endpoint) -> Result<NamedPipeClient, IpcError> {
    let deadline = std::time::Instant::now() + CONNECT_RETRY_WINDOW;
    loop {
        match ClientOptions::new().open(endpoint.name()) {
            Ok(client) => return Ok(client),
            Err(error) => {
                // `ERROR_PIPE_BUSY`: every instance is connected. `ERROR_FILE_NOT_FOUND`: the
                // listener has none listening at this instant — the instance it was waiting on
                // went to another client and the next one is not created yet. Both clear in
                // microseconds, and refusing a client that hit one would fail a call core
                // would have answered, so the client waits for the instance to appear.
                let waiting = matches!(
                    error.raw_os_error(),
                    Some(ERROR_PIPE_BUSY) | Some(ERROR_FILE_NOT_FOUND)
                );
                if waiting && std::time::Instant::now() < deadline {
                    tokio::time::sleep(CONNECT_RETRY_DELAY).await;
                    continue;
                }
                return Err(IpcError::io("connecting to the named pipe", error));
            }
        }
    }
}

/// A connected pipe pair for the core ↔ agent channel.
///
/// See the module documentation for why this is a named pair rather than the
/// anonymous pair of the documents.
pub async fn agent_pair() -> Result<(NamedPipeServer, NamedPipeClient), IpcError> {
    static NEXT: AtomicU64 = AtomicU64::new(1);

    let security = PipeSecurity::for_current_user()?;
    let options = PipeOptions::agent_pair();
    let name = format!(
        "{PIPE_PATH_PREFIX}{PIPE_NAME_PREFIX}agent-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );

    let server = security.create(&name, &options)?;
    let client = ClientOptions::new()
        .open(&name)
        .map_err(|source| IpcError::io("connecting the agent pipe", source))?;
    server
        .connect()
        .await
        .map_err(|source| IpcError::io("connecting the agent pipe", source))?;

    Ok((server, client))
}

#[cfg(test)]
mod tests {
    use crate::peer::SameUser;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// A pipe name no other test or process uses.
    fn test_endpoint() -> Endpoint {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Endpoint::pipe(format!(
            "{PIPE_PATH_PREFIX}{PIPE_NAME_PREFIX}test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// A cancelled `accept` must not lose the client that connected while it waited.
    ///
    /// Core's session loop polls `accept` as one branch of a `select!` beside its ticks and
    /// agent frames, so a client that shows up right then is cancelled on almost every
    /// request. Before [`PendingInstance`] the instance waiting for that client was dropped
    /// with the connection on it, and `lws-core status` reported «core closed the control
    /// channel without an answer» for a few calls in two hundred — even though core served
    /// every *other* one, which is what made it look like the answer had been lost rather
    /// than the connection.
    #[tokio::test]
    async fn a_cancelled_accept_keeps_the_client_it_waited_for() {
        let endpoint = test_endpoint();
        let mut listener = listen(&endpoint).expect("a listener");

        // Start an `accept`: it takes the instance out of the listener and waits for a client.
        let same_user = SameUser::current();
        let mut accept = Box::pin(listener.accept(&same_user));
        tokio::select! {
            result = &mut accept => panic!("no client is connected yet: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        // A client connects while that `accept` is waiting — and the `accept` is then dropped,
        // which is what core's `select!` does when its tick or an agent frame wins the race.
        let client = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { connect(&endpoint).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(accept);

        // The client is still there, so the next accept serves it.
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            listener.accept(&SameUser::current()),
        )
        .await
        .expect("the client connected before the cancel must still be accepted")
        .expect("accept");
        let mut client = client.await.expect("client task").expect("connect");

        // And the connection is usable, not just present.
        client.write_all(b"ping\n").await.expect("write");
        client.flush().await.expect("flush");
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .expect("read");
        assert_eq!(line, "ping\n");
    }

    /// A client that arrives while core is serving the previous one is not stranded.
    ///
    /// The shape this replaced kept a second instance listening beside the one `accept` was
    /// waiting on, and the kernel could hand a client to *that* one: the client was connected,
    /// so its own retry had nothing to retry, and it waited for core to come back around to
    /// `accept` — which under load took longer than the client's patience. One instance at a
    /// time is the fix: every client the kernel accepts is attached to the instance core is
    /// waiting on, so the next `accept` always serves the client that is there.
    #[tokio::test]
    async fn a_client_that_arrives_while_another_is_served_is_not_stranded() {
        let endpoint = test_endpoint();
        let mut listener = listen(&endpoint).expect("a listener");

        // The first client: accepted, and held while it is served.
        let first = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { connect(&endpoint).await }
        });
        let mut served = listener.accept(&SameUser::current()).await.expect("accept");

        // A second client arrives while that instance is still in use.
        let second = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { connect(&endpoint).await }
        });
        let mut next = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            listener.accept(&SameUser::current()),
        )
        .await
        .expect("the second client must reach the listener that is waiting for it")
        .expect("accept");

        // Both connections are usable, not merely present.
        let mut client = first.await.expect("task").expect("connect");
        client.write_all(b"one\n").await.expect("write");
        client.flush().await.expect("flush");
        let mut line = String::new();
        BufReader::new(&mut served)
            .read_line(&mut line)
            .await
            .expect("read");
        assert_eq!(line, "one\n");

        let mut client = second.await.expect("task").expect("connect");
        client.write_all(b"two\n").await.expect("write");
        client.flush().await.expect("flush");
        let mut line = String::new();
        BufReader::new(&mut next)
            .read_line(&mut line)
            .await
            .expect("read");
        assert_eq!(line, "two\n");
    }
}
