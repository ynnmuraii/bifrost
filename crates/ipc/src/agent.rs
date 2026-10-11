//! Rules of the core ↔ agent contract that are not framing.
//!
//! Sources: `docs/03-architecture.md`, «Правила agent (инварианты)» and the
//! «Транспорт и аутентификация» table.
//!
//! * launch token: «32 случайных байта в первом кадре `AgentAttach` по
//!   унаследованному каналу; не в argv и не в env (видны через `ps`/Process
//!   Explorer)» — the token is the second check after the inherited channel, and
//!   the agent must compare it before doing anything else.
//! * frame version: «Версии кадра — `major`/`minor` в `AgentFrame`; несовпадение
//!   major закрывает канал с понятной ошибкой.»
//! * rule 7: «`RECOVERY`: допускаются только `ReleaseKeys` и `AgentShutdown`.
//!   Capture и inject запрещены.»
//!
//! The launch token never appears in a log line, an error message or a
//! diagnostic export; this module only reports *that* it did not match.

use protocol::ipc::{agent_frame, AgentMode};

use crate::error::IpcError;

/// Length of `AgentAttach.launch_token` (32 random bytes).
pub const LAUNCH_TOKEN_LEN: usize = 32;

/// Compare two tokens without an early exit, so the comparison time does not
/// depend on where the first difference is.
///
/// The length check returns early, which leaks only the length — a public
/// constant ([`LAUNCH_TOKEN_LEN`]).
pub fn tokens_match(expected: &[u8], presented: &[u8]) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in expected.iter().zip(presented.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Check the token of the first `AgentAttach` frame.
///
/// The token is compared, never echoed: the error carries no bytes of it.
pub fn verify_launch_token(expected: &[u8], presented: &[u8]) -> Result<(), IpcError> {
    if expected.len() != LAUNCH_TOKEN_LEN {
        return Err(IpcError::config(format!(
            "launch token must be {LAUNCH_TOKEN_LEN} bytes, got {}",
            expected.len()
        )));
    }
    if !tokens_match(expected, presented) {
        return Err(IpcError::protocol(
            "AgentAttach.launch_token does not match the token passed at spawn",
        ));
    }
    Ok(())
}

/// Check `AgentFrame.major` against the version this side speaks.
///
/// `minor` is negotiated by the caller when it matters; only a `major` mismatch
/// closes the channel.
pub fn check_frame_version(major: u32, expected_major: u32) -> Result<(), IpcError> {
    if major != expected_major {
        return Err(IpcError::protocol(format!(
            "AgentFrame major {major} does not match the expected {expected_major}; \
             close the channel"
        )));
    }
    Ok(())
}

/// Whether `message` may be handled in `RECOVERY`: only `ReleaseKeys` and
/// `AgentShutdown` (rule 7).
pub fn recovery_allows(message: &agent_frame::Msg) -> bool {
    matches!(
        message,
        agent_frame::Msg::ReleaseKeys(_) | agent_frame::Msg::Shutdown(_)
    )
}

/// Whether `message` is allowed in the given `AgentMode`.
///
/// `NORMAL` allows everything; `RECOVERY` allows [`recovery_allows`] only;
/// an unknown mode is treated as `NORMAL` by the agent state machine, which is
/// where the decision to enter `RECOVERY` lives.
pub fn mode_allows(mode: i32, message: &agent_frame::Msg) -> bool {
    match AgentMode::try_from(mode) {
        Ok(AgentMode::Recovery) => recovery_allows(message),
        _ => true,
    }
}

/// The first frame of the channel must be `AgentAttach`.
pub fn expect_attach(frame: &agent_frame::Msg) -> Result<&protocol::ipc::AgentAttach, IpcError> {
    match frame {
        agent_frame::Msg::Attach(attach) => Ok(attach),
        _ => Err(IpcError::protocol(
            "the first AgentFrame must carry AgentAttach (launch token)",
        )),
    }
}
