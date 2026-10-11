//! The two things core hands the agent at launch: the local IPC endpoint and the
//! launch-token file. Neither carries a secret in `argv`; the token file does.

use std::path::Path;

use ipc::transport::Endpoint;

/// The endpoint core created for this launch.
///
/// Core passes the name it chose: a per-launch pipe name
/// (`\\.\pipe\lan-workspace-agent-<pid>-<n>`, DACL of the current user,
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`, one instance — `docs/03-architecture.md`,
/// «Транспорт и аутентификация») on Windows, and the per-launch socket path in the
/// 0700 runtime directory on Unix. Only the name travels in `argv`.
#[cfg(windows)]
pub fn endpoint_from_spec(spec: &str) -> Endpoint {
    Endpoint::pipe(spec.to_owned())
}

/// See the Windows variant: on Unix the spec is the socket path.
#[cfg(unix)]
pub fn endpoint_from_spec(spec: &str) -> Endpoint {
    Endpoint::socket(std::path::PathBuf::from(spec))
}

/// Reads the launch token from `path` and deletes the file.
///
/// The documents forbid `argv`/`env` for the token («видны через `ps`/Process
/// Explorer»), so core writes 32 random bytes into a file inside a user-only
/// directory and passes only the path. Deleting the file on read keeps a single
/// usable copy in the lifetime of one launch.
pub fn take_launch_token(path: &Path) -> std::io::Result<Vec<u8>> {
    let bytes = std::fs::read(path)?;
    // Best effort: a failure to remove must not turn into a working but reusable
    // token, so it is reported, not ignored.
    std::fs::remove_file(path)?;
    if bytes.len() != ipc::agent::LAUNCH_TOKEN_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "launch token is {} bytes, expected {}",
                bytes.len(),
                ipc::agent::LAUNCH_TOKEN_LEN
            ),
        ));
    }
    Ok(bytes)
}
