//! The two things core hands the agent at launch: the local IPC endpoint and the
//! launch-token file. Neither carries a secret in `argv`; the token file does.

use std::path::Path;

use ipc::transport::Endpoint;

/// The endpoint core created for this launch.
///
/// Core passes the name it chose: a per-launch pipe name
/// (`\\.\pipe\bifrost-agent-<pid>-<n>`, DACL of the current user,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bifrost-agent-launch-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temporary directory");
        dir
    }

    #[test]
    fn the_token_is_read_once_and_the_file_is_gone_afterwards() {
        let dir = temp_dir();
        let path = dir.join("launch-1.token");
        let token: Vec<u8> = (0u8..32).collect();
        std::fs::write(&path, &token).expect("write the token file");

        assert_eq!(take_launch_token(&path).expect("first read"), token);
        assert!(
            !path.exists(),
            "one launch must leave at most one usable copy"
        );

        // A second read finds nothing: the file was deleted, not merely consumed.
        let second = take_launch_token(&path).expect_err("the file is gone");
        assert_eq!(second.kind(), std::io::ErrorKind::NotFound);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_of_the_wrong_length_is_refused_and_still_deleted() {
        let dir = temp_dir();
        let path = dir.join("launch-2.token");
        std::fs::write(&path, b"too short").expect("write");

        let error = take_launch_token(&path).expect_err("a short token is not a token");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            !path.exists(),
            "a rejected token must not stay behind for a retry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_token_file_reports_not_found() {
        let dir = temp_dir();
        let error = take_launch_token(&dir.join("never-written.token")).expect_err("missing file");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
