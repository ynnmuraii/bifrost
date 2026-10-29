#![deny(unsafe_op_in_unsafe_fn)]
//! Local IPC of Bifrost (KVM-023/KVM-024): the transport, the peer checks
//! and the heartbeat watchdog that keep `lws-core`, `lws-agent` and `lws-gui`
//! talking without a network in between.
//!
//! # What is here
//!
//! * [`framed`] — `4-byte big-endian length || Protobuf` over any async byte
//!   stream, reusing `protocol`'s framing (`Channel::Ipc`, 64 KiB) unchanged.
//! * [`link`] — [`link::AgentLink`] / [`link::GuiLink`]: they stamp the outgoing
//!   `seq` and reject a gap, a repeat or a regression on the incoming side, which
//!   the documents turn into "close the channel, release everything".
//! * [`watchdog`] — [`watchdog::Watchdog`], sans-IO: `Ping` every
//!   `core_silence_ms / 4` and the `core_silence_ms` silence limit of rule 6.
//! * [`clock`] — [`clock::Clock`] and [`clock::SuspendDetector`]: the monotonic clock
//!   the watchdog is fed from, the wall clock beside it, and the divergence that tells
//!   a resume from a stall.
//! * [`peer`] — [`peer::PeerCheck`] and [`peer::SameUser`]: who may connect.
//! * [`agent`] — the launch token, the frame `major` check and the `RECOVERY`
//!   allow-list of rule 7.
//! * [`transport`] — named pipes with an explicit DACL on Windows, Unix sockets
//!   with 0600 and `SO_PEERCRED` on Linux.
//!
//! # The agent's "no network sockets" invariant
//!
//! `docs/03-architecture.md`, rule 1: «Нет сетевых сокетов. В Windows-сборке не
//! линкуется winsock; на Linux — проверка в CI по списку символов.»
//!
//! This crate cannot enforce that by feature selection: tokio's `net` feature is
//! one switch that turns on TCP, UDP, Unix sockets and named pipes together, and
//! there is no Unix-socket-only or pipe-only feature. The invariant therefore
//! stays a **link-level** property of the agent binary, checked the way the
//! documents say: by scanning the linked symbols in CI (`ws2_32`/`WSAStartup`/
//! `connect`/`socket` for Windows; `socket`, `connect`, `getaddrinfo`, `sendto`
//! for the Linux build) rather than by pretending the feature set is enough.
//! What this crate does guarantee is narrower and checkable: it opens no TCP or
//! UDP socket, it has no DNS dependency, and every endpoint it can create is
//! local.
//!
//! # Example shape
//!
//! ```no_run
//! # async fn run() -> Result<(), ipc::IpcError> {
//! use ipc::{agent, peer::SameUser, transport, watchdog::Watchdog, AgentLink, Framed};
//!
//! // core side
//! let (core_end, agent_end) = transport::agent_pair().await?;
//!
//! // core: send the first frame, which carries the launch token
//! let mut core = Framed::ipc(core_end);
//! let token = [7u8; agent::LAUNCH_TOKEN_LEN];
//! core.send(&protocol::ipc::AgentFrame {
//!     major: 0,
//!     minor: 3,
//!     seq: 1,
//!     msg: Some(protocol::ipc::agent_frame::Msg::Attach(
//!         protocol::ipc::AgentAttach {
//!             launch_token: token.to_vec(),
//!             core_silence_ms: 300,
//!             ..Default::default()
//!         },
//!     )),
//! })
//! .await?;
//!
//! // agent: the link stamps and checks seq, the watchdog owns the silence limit
//! let mut link = AgentLink::new(agent_end, 0, 3);
//! let frame = link.recv().await?;
//! let attach = agent::expect_attach(frame.msg.as_ref().unwrap())?;
//! agent::verify_launch_token(&token, &attach.launch_token)?;
//! let mut watchdog = Watchdog::for_core_silence_ms(attach.core_silence_ms, 0)?;
//! watchdog.on_frame(0);
//! # let _ = SameUser::current();
//! # Ok(())
//! # }
//! ```

pub mod agent;
pub mod clock;
pub mod error;
pub mod framed;
pub mod link;
pub mod peer;
pub mod transport;
pub mod watchdog;

pub use error::IpcError;
pub use framed::Framed;
pub use link::{AgentLink, AgentSender, AgentSplit, GuiLink};
pub use peer::{CredentialSource, PeerCheck, PeerCredentials, PeerRejected, SameUser};
pub use watchdog::{Watchdog, DEFAULT_CORE_SILENCE_MS, MAX_CORE_SILENCE_MS, MIN_CORE_SILENCE_MS};

/// The IPC frame channel: 65536-byte frames, never on the network.
pub use protocol::Channel as FrameChannel;
