//! Who is on the other end of an accepted local connection, and who is allowed.
//!
//! `docs/03-architecture.md`, «Транспорт и аутентификация»:
//!
//! * core ↔ agent uses a channel the parent handed to the child (an anonymous
//!   pipe pair on Windows, `socketpair(AF_UNIX)` on Linux), so there is nothing
//!   to connect to; «Унаследованный канал убирает класс атак «подключиться к
//!   сокету агента раньше core». Token остаётся второй проверкой.»
//! * gui ↔ core is a **named** endpoint: «Named pipe `\\.\pipe\lan-workspace-<SID>`,
//!   DACL: только SID текущего пользователя, `PIPE_REJECT_REMOTE_CLIENTS`» on
//!   Windows and «UDS `$XDG_RUNTIME_DIR/lan-workspace/gui.sock`, каталог 0700,
//!   сокет 0600, проверка `SO_PEERCRED` uid» on Linux.
//!
//! The two platforms enforce "same user only" at different points, and this
//! module makes that difference explicit instead of pretending both are the same
//! check:
//!
//! * Windows: the kernel decides at `CreateFile` time against the pipe's DACL
//!   (see [`crate::transport`]). By the time `accept` returns, the peer is
//!   already the current user; [`PeerCredentials::source`] says
//!   [`CredentialSource::PipeDacl`] and the uid is unknown.
//! * Linux: the socket is 0600, and the owner is checked explicitly through
//!   `SO_PEERCRED` after `accept`; the source is [`CredentialSource::PeerCred`].

use core::fmt;

/// How the peer's identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// `SO_PEERCRED` of an accepted Unix socket: uid/gid/pid of the peer at
    /// connect time (Linux).
    PeerCred,
    /// The named-pipe DACL restricted access to the current user's SID before
    /// the connection could be made, so no uid is available here (Windows).
    PipeDacl,
}

/// Credentials of an accepted local peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    /// How the identity was established.
    pub source: CredentialSource,
    /// Effective uid of the peer, when the platform reports one.
    pub uid: Option<u32>,
    /// Effective gid of the peer, when the platform reports one.
    pub gid: Option<u32>,
    /// pid of the peer, when the platform reports one.
    pub pid: Option<u32>,
}

impl PeerCredentials {
    /// Credentials of a Windows named-pipe peer: the DACL already ran.
    pub const fn pipe_dacl() -> Self {
        PeerCredentials {
            source: CredentialSource::PipeDacl,
            uid: None,
            gid: None,
            pid: None,
        }
    }

    /// Credentials reported by `SO_PEERCRED`.
    pub const fn peer_cred(uid: u32, gid: u32, pid: Option<u32>) -> Self {
        PeerCredentials {
            source: CredentialSource::PeerCred,
            uid: Some(uid),
            gid: Some(gid),
            pid,
        }
    }
}

impl fmt::Display for PeerCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            CredentialSource::PeerCred => write!(
                f,
                "SO_PEERCRED uid={:?} gid={:?} pid={:?}",
                self.uid, self.gid, self.pid
            ),
            CredentialSource::PipeDacl => f.write_str("named-pipe DACL (current user only)"),
        }
    }
}

/// An accepted peer that the endpoint refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRejected {
    /// Who tried to connect.
    pub peer: PeerCredentials,
    /// What the endpoint required.
    pub expected: String,
}

impl fmt::Display for PeerRejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rejected IPC peer ({}) : {}", self.peer, self.expected)
    }
}

impl std::error::Error for PeerRejected {}

/// Hook deciding whether an accepted local peer may use the endpoint.
///
/// The check runs *after* `accept`, so it never blocks the listener; a rejected
/// peer is dropped and the listener stays usable.
pub trait PeerCheck: Send + Sync {
    /// `Ok(())` to serve the peer, `Err` to drop it.
    fn check(&self, peer: &PeerCredentials) -> Result<(), PeerRejected>;
}

/// Serves only the user that owns the endpoint: the effective uid on Linux, the
/// current user's SID on Windows (enforced by the pipe DACL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SameUser {
    expected_uid: Option<u32>,
}

impl SameUser {
    /// The current user.
    ///
    /// On Unix this is `geteuid()`; on Windows there is no uid and the pipe DACL
    /// is the check, so [`SameUser::check`] accepts
    /// [`CredentialSource::PipeDacl`] credentials unconditionally.
    pub fn current() -> Self {
        SameUser {
            expected_uid: current_uid(),
        }
    }

    /// A specific uid (tests, and callers that know the owner).
    pub fn for_uid(uid: u32) -> Self {
        SameUser {
            expected_uid: Some(uid),
        }
    }
}

impl Default for SameUser {
    fn default() -> Self {
        SameUser::current()
    }
}

impl PeerCheck for SameUser {
    fn check(&self, peer: &PeerCredentials) -> Result<(), PeerRejected> {
        match (peer.source, self.expected_uid) {
            // The DACL ran in the kernel before the peer could connect.
            (CredentialSource::PipeDacl, _) => Ok(()),
            (CredentialSource::PeerCred, Some(expected)) => match peer.uid {
                Some(uid) if uid == expected => Ok(()),
                _ => Err(PeerRejected {
                    peer: *peer,
                    expected: format!("the endpoint owner (uid {expected})"),
                }),
            },
            (CredentialSource::PeerCred, None) => Err(PeerRejected {
                peer: *peer,
                expected: "a resolvable owner uid; use SameUser::for_uid".to_owned(),
            }),
        }
    }
}

/// Serves every peer that could connect at all. Only for tests and for
/// endpoints that are protected by something else (an inherited handle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AllowAnyPeer;

impl PeerCheck for AllowAnyPeer {
    fn check(&self, _peer: &PeerCredentials) -> Result<(), PeerRejected> {
        Ok(())
    }
}

/// The effective uid of this process, when the platform reports one.
///
/// `/proc/self` is not used on purpose: `stat` there reports the *real* uid,
/// while `SO_PEERCRED` reports the *effective* one, and the two differ in a
/// setuid process.
#[cfg(unix)]
pub fn current_uid() -> Option<u32> {
    // SAFETY: `geteuid` takes no arguments, touches no memory and cannot fail.
    Some(unsafe { libc::geteuid() })
}

/// Windows has no uid; the named-pipe DACL carries the identity instead.
#[cfg(not(unix))]
pub fn current_uid() -> Option<u32> {
    None
}
