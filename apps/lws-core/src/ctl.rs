//! The local control channel the CLI uses against a running core.
//!
//! `docs/ipc.proto`'s `GuiFrame` is the contract for `lws-gui`, and it has no
//! "switch to this peer" message: a manual transition is a local user action the
//! documents describe as a hotkey or an edge («Сначала ручной переключатель, затем
//! edge-trigger»), not as a GUI command. Rather than extend a frozen contract that
//! the GUI work shares, `lws-core run` exposes this one-line-per-request channel on
//! its own endpoint, built with the same transport rules as the GUI one
//! (`docs/03-architecture.md`, «Транспорт и аутентификация»): a per-user DACL on
//! Windows, 0600 in the 0700 runtime directory plus a `SO_PEERCRED` uid check on
//! Unix. When `lws-gui` lands, `Status` moves to `GuiFrame` and this channel keeps
//! only what the GUI has no message for.
//!
//! Requests and replies are single lines of UTF-8:
//!
//! ```text
//! status                 -> OK focus=LOCAL owner=… members=1 agent=ready
//! switch <peer>          -> OK switched to <peer> | ERR <code> <detail>
//! quit                   -> OK stopping
//! ping                   -> OK pong
//! ```
//!
//! `<peer>` is a 64-hex node id or a paired peer's display name.

use ipc::peer::SameUser;
use ipc::transport::{self, Endpoint, GuiListener};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::command::{AgentState, PeerRef};

/// The stream a CLI client is served on.
#[cfg(unix)]
pub type ClientStream = tokio::net::UnixStream;
/// See [`ClientStream`].
#[cfg(windows)]
pub type ClientStream = tokio::net::windows::named_pipe::NamedPipeServer;

/// Where the control endpoint lives for a runtime directory.
///
/// It mirrors the GUI endpoint's rules rather than its name: `$XDG_RUNTIME_DIR/`
/// `lan-workspace/ctl.sock` next to `gui.sock` on Unix, and
/// `\\.\pipe\lan-workspace-ctl-<SID>` next to the GUI pipe on Windows.
#[cfg(unix)]
pub fn endpoint(runtime_dir: &std::path::Path) -> Result<Endpoint, String> {
    Ok(Endpoint::socket(
        transport::gui_socket_path(runtime_dir).with_file_name("ctl.sock"),
    ))
}

/// See the Unix variant.
///
/// The GUI pipe is per user (`\\.\pipe\lan-workspace-<SID>`) because a desktop has one
/// GUI, but this channel belongs to *one core*, and `--root` promises to put «config/,
/// data/, state/ and run/ under DIR»: two cores for the same user — the loopback smoke,
/// a test host — must not fight over one name. A `--root` therefore adds a tag derived
/// from the runtime directory, exactly as the Unix socket path is derived from it; the
/// default directories keep the documented per-user name, and the SID stays in it so the
/// DACL's owner is recognisable.
#[cfg(windows)]
pub fn endpoint(runtime_dir: &std::path::Path) -> Result<Endpoint, String> {
    let sid = transport::current_user_sid_string().map_err(|error| error.to_string())?;
    let tag = runtime_tag(runtime_dir);
    Ok(Endpoint::pipe(format!(
        "{}{}ctl-{sid}{}",
        transport::PIPE_PATH_PREFIX,
        transport::PIPE_NAME_PREFIX,
        tag.map(|tag| format!("-{tag}")).unwrap_or_default()
    )))
}

/// The `--root` tag for the control pipe, or `None` for the default directories.
///
/// FNV-1a over the canonical runtime directory: the daemon and the CLI client build it
/// from the same [`config::Paths`], so they agree without touching the filesystem, and a
/// pipe name cannot carry the path's `\` itself. The default runtime directory is `None`
/// on Windows and the core then lives in the state directory (`private_root`), so both
/// defaults count as "not a `--root`".
#[cfg(windows)]
fn runtime_tag(runtime_dir: &std::path::Path) -> Option<String> {
    let defaults = config::Config::default_paths().ok()?;
    let canonical = |path: &std::path::Path| {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    };
    let ours = canonical(runtime_dir);
    let is_default = [defaults.runtime_dir(), Some(defaults.state_dir())]
        .into_iter()
        .flatten()
        .any(|default| canonical(default) == ours);
    if is_default {
        return None;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in ours.as_os_str().to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Some(format!("{hash:016x}"))
}

/// One line of the request protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Report the current state.
    Status,
    /// Ask for a manual transition to a peer.
    Switch(PeerRef),
    /// Stop core.
    Quit,
    /// Liveness of the control channel itself.
    Ping,
}

impl Request {
    /// Parses one request line.
    pub fn parse(line: &str) -> Result<Self, String> {
        let line = line.trim();
        let (verb, argument) = match line.split_once(char::is_whitespace) {
            Some((verb, argument)) => (verb, argument.trim()),
            None => (line, ""),
        };
        match verb {
            "status" if argument.is_empty() => Ok(Request::Status),
            "ping" if argument.is_empty() => Ok(Request::Ping),
            "quit" if argument.is_empty() => Ok(Request::Quit),
            "switch" if !argument.is_empty() => Ok(Request::Switch(PeerRef::new(argument))),
            "switch" => Err("switch needs a peer id or display name".to_owned()),
            other => Err(format!("unknown request {other:?}")),
        }
    }
}

/// The answer for a request core handled without an FSM transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A `status` line, already formatted.
    Status(String),
    /// The switch was handed to the FSM.
    Switched(PeerRef),
    /// A refusal: `ERR <code> <detail>`.
    Error { code: String, detail: String },
    /// `OK stopping`.
    Stopping,
    /// `OK pong`.
    Pong,
}

impl Response {
    /// The wire form of one reply line.
    pub fn render(&self) -> String {
        match self {
            Response::Status(line) => format!("OK {line}"),
            Response::Switched(peer) => format!("OK switched to {peer}"),
            Response::Error { code, detail } => format!("ERR {code} {detail}"),
            Response::Stopping => "OK stopping".to_owned(),
            Response::Pong => "OK pong".to_owned(),
        }
    }

    /// Parses a reply line, for the CLI side.
    pub fn parse(line: &str) -> Result<Self, String> {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("OK switched to ") {
            return Ok(Response::Switched(PeerRef::new(rest)));
        }
        if let Some(rest) = line.strip_prefix("ERR ") {
            let (code, detail) = match rest.split_once(char::is_whitespace) {
                Some((code, detail)) => (code, detail.trim()),
                None => (rest, ""),
            };
            return Ok(Response::Error {
                code: code.to_owned(),
                detail: detail.to_owned(),
            });
        }
        if let Some(rest) = line.strip_prefix("OK ") {
            return match rest {
                "stopping" => Ok(Response::Stopping),
                "pong" => Ok(Response::Pong),
                other => Ok(Response::Status(other.to_owned())),
            };
        }
        Err(format!("not a control reply: {line:?}"))
    }
}

/// The control endpoint of a running core.
pub struct Server {
    listener: GuiListener,
}

impl Server {
    /// Binds the endpoint. Fails with [`ipc::IpcError::AlreadyRunning`] when another
    /// core already owns it, which is what a second `lws-core run` in the same
    /// configuration directory must see.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, ipc::IpcError> {
        Ok(Server {
            listener: transport::listen(endpoint)?,
        })
    }

    /// Accepts the next CLI client.
    ///
    /// The kernel already checked the DACL on Windows; on Unix the default
    /// [`SameUser`] compares `SO_PEERCRED`.
    pub async fn accept(&mut self) -> Result<ClientStream, ipc::IpcError> {
        self.listener.accept(&SameUser::current()).await
    }
}

/// Sends one request line and returns the reply line.
pub async fn request(endpoint: &Endpoint, line: &str) -> std::io::Result<String> {
    let mut stream = transport::connect(endpoint)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    serve_line(&mut stream, line).await
}

/// One request/reply exchange on an accepted connection.
pub async fn serve_line<S>(stream: &mut S, line: &str) -> std::io::Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    stream.flush().await.ok();
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    let read = reader.read_line(&mut reply).await?;
    if read == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "core closed the control channel without an answer",
        ));
    }
    Ok(reply)
}

/// What the session loop tells the status line about itself.
pub struct StatusInput<'a> {
    /// `LOCAL`/`PREPARING`/`REMOTE`/`RELEASING`/`BLOCKED`.
    pub focus: &'a str,
    /// `owner` or `follower`.
    pub role: &'a str,
    /// The peer of the active lease, when there is one.
    pub lease_peer: Option<&'a str>,
    /// Established links.
    pub members: usize,
    /// The agent's state word.
    pub agent: AgentState,
    /// The backend the agent reported.
    pub backend: &'a str,
    /// The local node id in hex.
    pub node_id: &'a str,
    /// The owner's id in hex, when one is known.
    pub owner: &'a str,
    /// The owner claim serial.
    pub serial: u64,
}

/// Formats the one-line `status` answer.
pub fn status_line(input: &StatusInput<'_>) -> String {
    format!(
        "focus={} role={} peer={} members={} agent={} backend={} node={} owner={} serial={}",
        input.focus,
        input.role,
        input.lease_peer.unwrap_or("-"),
        input.members,
        input.agent.as_str(),
        if input.backend.is_empty() {
            "-"
        } else {
            input.backend
        },
        input.node_id,
        if input.owner.is_empty() {
            "-"
        } else {
            input.owner
        },
        input.serial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_one_line() {
        assert_eq!(Request::parse("status"), Ok(Request::Status));
        assert_eq!(Request::parse("  ping  "), Ok(Request::Ping));
        assert_eq!(Request::parse("quit"), Ok(Request::Quit));
        assert_eq!(
            Request::parse(
                "switch 7d357775d9879d19429ae1f5af540c8bc3013313a0c0fa64b961fcbcecc4c242"
            ),
            Ok(Request::Switch(PeerRef::new(
                "7d357775d9879d19429ae1f5af540c8bc3013313a0c0fa64b961fcbcecc4c242"
            )))
        );
        assert!(Request::parse("switch").is_err());
        assert!(Request::parse("status extra").is_err());
        assert!(Request::parse("frobnicate").is_err());
    }

    #[test]
    fn replies_round_trip_through_one_line() {
        for reply in [
            Response::Status("focus=LOCAL role=owner".to_owned()),
            Response::Switched(PeerRef::new("laptop")),
            Response::Error {
                code: "NOT_OWNER".to_owned(),
                detail: "this node is a follower".to_owned(),
            },
            Response::Stopping,
            Response::Pong,
        ] {
            assert_eq!(
                Response::parse(&reply.render()),
                Ok(reply.clone()),
                "{reply:?}"
            );
        }
    }

    #[test]
    fn a_status_line_names_the_fields_the_cli_prints() {
        let line = status_line(&StatusInput {
            focus: "REMOTE",
            role: "owner",
            lease_peer: Some("laptop"),
            members: 1,
            agent: AgentState::Ready,
            backend: "platform-win",
            node_id: "aaaa",
            owner: "aaaa",
            serial: 3,
        });
        assert!(line.contains("focus=REMOTE"));
        assert!(line.contains("peer=laptop"));
        assert!(line.contains("agent=ready"));
        assert!(line.contains("serial=3"));
    }
}
