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
//! claim                  -> OK claimed | ERR CLAIM_REFUSED <code>
//! abandon                -> OK abandoned
//! quit                   -> OK stopping
//! ping                   -> OK pong
//! pair-open [seconds]    -> OK address=<ip:port> attempt=<32-hex> seconds=<n>
//! pair-prompt            -> OK sas=482913 peer=<name> ceremony=<32-hex> address=<ip:port>
//! pair-answer <hex> yes  -> OK paired <name> | ERR <code> <detail>
//! pair-answer <hex> no   -> OK rejected
//! pair-with <address>    -> OK dialing
//! ```
//!
//! `<peer>` is a 64-hex node id or a paired peer's display name. `pair-prompt` and
//! `pair-answer` are answered when the ceremony reaches that point — the window is opened by
//! `pair-open` and the peer drives it from its own node, so the CLI cannot know when — which is
//! why they block on their own connection instead of returning `OK` at once. `pair-with` is the
//! dialer's half of the same ceremony, for a node whose core is already running.
//!
//! `pair --open` with a core already running uses these verbs instead of binding its own
//! listener: the window belongs to the running core, because that is the process that owns the
//! link listener `pair/1` is served on (`docs/05-protocol.md`, «Сопряжение»).

use ipc::peer::SameUser;
use ipc::transport::{self, Endpoint, GuiListener};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::command::{AgentState, PeerRef};

/// The stream a CLI client is served on: the same local endpoint type the GUI uses.
pub use ipc::transport::ClientStream;

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
/// The tag itself lives in `ipc::transport` now: the GUI endpoint, the GUI binary and this
/// channel must derive the same name from the same [`config::Paths`], and one copy of the
/// hash is what keeps them from drifting apart.
#[cfg(windows)]
fn runtime_tag(runtime_dir: &std::path::Path) -> Option<String> {
    transport::root_tag(runtime_dir)
}

/// One line of the request protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Report the current state.
    Status,
    /// Ask for a manual transition to a peer.
    Switch(PeerRef),
    /// Set the destination without toggling; `None` means local input.
    ///
    /// `switch <peer>` decides from the state it finds, so a caller that read the status and
    /// then issued the verb races the lease: the same command can start *and* return a
    /// transition. Naming the destination removes the observation entirely.
    SwitchTo(Option<PeerRef>),
    /// Ask this node to own the workspace (`GuiFrame.ClaimOwnership`): the same path as
    /// the claim hotkey or the GUI button, and the only way a node claims in a test.
    Claim,
    /// Give up ownership and stay a follower (`GuiFrame.AbandonOwnership`).
    Abandon,
    /// Drop every stored portal token, in core and in the agent
    /// (`ForgetPortalGrants`/`ResetPortalGrants`).
    ResetPortals,
    /// Stop core.
    Quit,
    /// Liveness of the control channel itself.
    Ping,
    /// `layout show`: the applied revision, the floor and the pending edit (KVM-072).
    LayoutShow,
    /// `layout edit <file>`: apply a whole document through the coordinator's CAS.
    LayoutEdit(String),
    /// `layout apply`: apply the pending edit now that this node coordinates.
    LayoutApply,
    /// `layout discard`: drop the pending edit.
    LayoutDiscard,
    /// `layout auto`: replace the document with the desktop-anchored default layout, through
    /// the coordinator's CAS — the one explicit action that turns a layout naming monitors by
    /// hand into one that follows them.
    LayoutAuto,
    /// `pair-open [seconds]`: open the pairing window on the running core
    /// (`GuiFrame.StartPairing`), sized by the optional argument.
    PairOpen(Option<u32>),
    /// `pair-prompt`: wait for the next ceremony's six digits.
    PairPrompt,
    /// `pair-answer <attempt-hex> approve|reject`: the user's decision.
    PairAnswer {
        /// The `attempt_id` the prompt named, as 32 hex characters.
        attempt: String,
        /// `true` for approve, `false` for reject.
        approve: bool,
    },
    /// `pair-with <address>`: dial a peer that opened its window (`GuiFrame.PairWith`).
    PairWith(String),
    /// `revoke <peer>`: forget local trust, close live links and remove the computer
    /// through the shared layout CAS — the same path as `GuiFrame.RevokePeer`.
    ///
    /// One implementation, so the file and the running core cannot disagree (audit F16): the
    /// CLI used to edit `trust.toml` behind a running core's back, which kept the peer
    /// trusted in memory and its link up.
    Revoke(PeerRef),
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
            "layout" if argument.trim() == "show" => Ok(Request::LayoutShow),
            "layout" if argument.starts_with("edit ") => Ok(Request::LayoutEdit(
                argument["edit ".len()..].trim().to_owned(),
            )),
            "layout" if argument.trim() == "apply" => Ok(Request::LayoutApply),
            "layout" if argument.trim() == "discard" => Ok(Request::LayoutDiscard),
            "layout" if argument.trim() == "auto" => Ok(Request::LayoutAuto),
            "layout" => Err("layout takes show, edit <file>, apply, discard, or auto".to_owned()),
            "ping" if argument.is_empty() => Ok(Request::Ping),
            "quit" if argument.is_empty() => Ok(Request::Quit),
            "reset-portals" if argument.is_empty() => Ok(Request::ResetPortals),
            "reset-portals" => Err("reset-portals takes no argument".to_owned()),
            "switch" if argument == "--to" => Err("switch --to needs local or a peer".to_owned()),
            "switch" if argument.starts_with("--to ") => {
                let target = argument["--to ".len()..].trim();
                if target.is_empty() {
                    Err("switch --to needs local or a peer".to_owned())
                } else {
                    Ok(Request::SwitchTo(
                        (target != "local").then(|| PeerRef::new(target)),
                    ))
                }
            }
            "switch" if !argument.is_empty() => Ok(Request::Switch(PeerRef::new(argument))),
            "switch" => Err("switch needs a peer id or display name".to_owned()),
            "pair-open" if argument.is_empty() => Ok(Request::PairOpen(None)),
            "pair-open" => match argument.parse::<u32>() {
                Ok(seconds) => Ok(Request::PairOpen(Some(seconds))),
                Err(_) => Err(format!(
                    "pair-open takes an optional number of seconds, not {argument:?}"
                )),
            },
            "pair-prompt" if argument.is_empty() => Ok(Request::PairPrompt),
            "pair-prompt" => Err("pair-prompt takes no argument".to_owned()),
            "pair-answer" => match argument.split_once(char::is_whitespace) {
                Some((attempt, answer)) => match answer.trim() {
                    "approve" | "yes" | "y" => Ok(Request::PairAnswer {
                        attempt: attempt.to_owned(),
                        approve: true,
                    }),
                    "reject" | "no" | "n" => Ok(Request::PairAnswer {
                        attempt: attempt.to_owned(),
                        approve: false,
                    }),
                    other => Err(format!(
                        "pair-answer takes approve or reject, not {other:?}"
                    )),
                },
                None => Err("pair-answer needs an attempt id and approve|reject".to_owned()),
            },
            "pair-with" if !argument.is_empty() => Ok(Request::PairWith(argument.to_owned())),
            "pair-with" => Err("pair-with needs an address".to_owned()),
            "revoke" if !argument.is_empty() => Ok(Request::Revoke(PeerRef::new(argument))),
            "revoke" => Err("revoke needs a peer id or display name".to_owned()),
            "claim" if argument.is_empty() => Ok(Request::Claim),
            "claim" => Err("claim takes no argument".to_owned()),
            "abandon" if argument.is_empty() => Ok(Request::Abandon),
            "abandon" => Err("abandon takes no argument".to_owned()),
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
    /// The claim was handed to the FSM.
    Claimed,
    /// Ownership was abandoned.
    Abandoned,
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
            Response::Claimed => "OK claimed".to_owned(),
            Response::Abandoned => "OK abandoned".to_owned(),
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
                "claimed" => Ok(Response::Claimed),
                "abandoned" => Ok(Response::Abandoned),
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
///
/// The client owns the whole exchange: it writes one line, reads the reply *until the
/// newline*, and only then closes the connection. Closing earlier is what made the daemon's
/// reply write fail, and what made a `status` come back as «core closed the control channel
/// without an answer».
pub async fn request(endpoint: &Endpoint, line: &str) -> std::io::Result<String> {
    let mut stream = transport::connect(endpoint)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    write_line(&mut stream, line).await?;
    let reply = read_line(&mut stream).await?;
    // Half-close the write side now that the request is complete: the reply is already in
    // hand, and on Unix the peer sees a clean EOF instead of a reset.
    tokio::io::AsyncWriteExt::shutdown(&mut stream).await.ok();
    reply.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "core closed the control channel without an answer",
        )
    })
}

/// Writes one line and flushes it, so the peer can read it before this side goes away.
pub async fn write_line<S>(stream: &mut S, line: &str) -> std::io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    stream
        .write_all(format!("{line}\n").as_bytes())
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// Reads one line; `Ok(None)` means the peer closed before sending one.
///
/// Both sides use exactly this and never a second line on the same connection: the daemon
/// answers one request per connection, so reading again after the reply only raced the
/// client's close and turned every answer into a logged failure.
pub async fn read_line<S>(stream: &mut S) -> std::io::Result<Option<String>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    Ok(Some(line))
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
    /// How many keys and buttons core currently believes are held.
    ///
    /// The agent's ledger is the authority; this is the mirror the FSM refuses a normal
    /// transition on, so a fault-injection run can assert «the pressed set is empty»
    /// without reading logs (`docs/03-architecture.md`, «Нормальный переход только с
    /// пустым physical pressed set»).
    pub pressed: usize,
    /// The last thing the agent reported that a human should see: an `AgentError`
    /// code (`PORTAL_DENIED`, `CAPTURE_FAILED`, …) or a portal hint. Empty when the
    /// agent has said nothing of the sort.
    pub refusal: &'a str,
}

/// Formats the one-line `status` answer.
pub fn status_line(input: &StatusInput<'_>) -> String {
    let base = format!(
        "focus={} role={} peer={} members={} agent={} backend={} node={} owner={} serial={} pressed={}",
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
        input.pressed,
    );
    // The refusal is appended only when there is one, so the common line stays short
    // and the field never looks like an empty setting.
    if input.refusal.is_empty() {
        base
    } else {
        format!("{base} refusal={}", input.refusal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_through_one_line() {
        assert_eq!(Request::parse("status"), Ok(Request::Status));
        // The pairing verbs the CLI uses once a core is running.
        assert_eq!(Request::parse("pair-open"), Ok(Request::PairOpen(None)));
        assert_eq!(
            Request::parse("pair-open 30"),
            Ok(Request::PairOpen(Some(30)))
        );
        assert!(Request::parse("pair-open soon").is_err());
        assert_eq!(Request::parse("pair-prompt"), Ok(Request::PairPrompt));
        assert!(Request::parse("pair-prompt now").is_err());
        assert_eq!(
            Request::parse("pair-answer 00112233445566778899aabbccddeeff approve"),
            Ok(Request::PairAnswer {
                attempt: "00112233445566778899aabbccddeeff".to_owned(),
                approve: true,
            })
        );
        assert_eq!(
            Request::parse("pair-answer 00112233445566778899aabbccddeeff n"),
            Ok(Request::PairAnswer {
                attempt: "00112233445566778899aabbccddeeff".to_owned(),
                approve: false,
            })
        );
        assert!(Request::parse("pair-answer").is_err());
        assert!(Request::parse("pair-answer a maybe").is_err());
        assert_eq!(
            Request::parse("pair-with 192.168.1.10:47901"),
            Ok(Request::PairWith("192.168.1.10:47901".to_owned()))
        );
        assert!(Request::parse("pair-with").is_err());
        assert_eq!(Request::parse("reset-portals"), Ok(Request::ResetPortals));
        assert!(Request::parse("reset-portals now").is_err());
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
        // `switch --to` names the destination instead of toggling on the observed state, and
        // `local` is the only spelling that means "no peer" — a peer id is never `local`.
        assert_eq!(
            Request::parse("switch --to local"),
            Ok(Request::SwitchTo(None))
        );
        assert_eq!(
            Request::parse("switch --to  office desktop "),
            Ok(Request::SwitchTo(Some(PeerRef::new("office desktop"))))
        );
        assert_eq!(
            Request::parse("switch --to local"),
            Ok(Request::SwitchTo(None)),
            "a repeated directional request must not become a toggle"
        );
        assert!(Request::parse("switch --to").is_err());
        assert!(Request::parse("switch --to   ").is_err());
        // The GUI's two ownership buttons, reachable from the CLI so a harness can claim.
        assert_eq!(Request::parse("claim"), Ok(Request::Claim));
        assert_eq!(Request::parse("  abandon "), Ok(Request::Abandon));
        assert!(Request::parse("claim now").is_err());
        assert!(Request::parse("abandon now").is_err());
        assert!(Request::parse("status extra").is_err());
        assert!(Request::parse("frobnicate").is_err());
    }

    #[test]
    fn replies_round_trip_through_one_line() {
        for reply in [
            Response::Status("focus=LOCAL role=owner".to_owned()),
            Response::Switched(PeerRef::new("laptop")),
            Response::Claimed,
            Response::Abandoned,
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
            pressed: 2,
            refusal: "",
        });
        assert!(line.contains("focus=REMOTE"));
        assert!(line.contains("peer=laptop"));
        assert!(line.contains("agent=ready"));
        assert!(line.contains("serial=3"));
        // A fault-injection run reads this to prove «the pressed set is empty» after a
        // transition without parsing logs.
        assert!(line.contains("pressed=2"), "{line}");
        assert!(
            !line.contains("refusal="),
            "nothing to report must not add an empty field: {line}"
        );

        // An agent refusal is what the GUI has to show next to the node.
        let with_refusal = status_line(&StatusInput {
            focus: "LOCAL",
            role: "follower",
            lease_peer: None,
            members: 0,
            agent: AgentState::Ready,
            backend: "platform-wayland",
            node_id: "bbbb",
            owner: "",
            serial: 0,
            pressed: 0,
            refusal: "PORTAL_DENIED: the user dismissed the InputCapture dialog",
        });
        assert!(
            with_refusal.contains("refusal=PORTAL_DENIED"),
            "{with_refusal}"
        );
        assert!(with_refusal.contains("pressed=0"), "{with_refusal}");
    }
}
