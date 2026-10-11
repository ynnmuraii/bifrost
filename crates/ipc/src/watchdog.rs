//! The heartbeat watchdog, sans-IO.
//!
//! Two independent heartbeats exist in LAN Workspace and this type models only
//! the local one (`docs/03-architecture.md`, «Что параметризуется» and rule 11):
//!
//! * core → agent: «core шлёт агенту `Ping` каждые `core_silence_ms / 4` (по
//!   умолчанию 75 ms, ≤125 ms), агент отвечает `Pong`»;
//! * agent: «Core молчит в REMOTE дольше `core_silence_ms` (передаётся в
//!   `AgentAttach`, 200..500, по умолчанию 300 ms; нет ни одного кадра, включая
//!   IPC `Ping`) → agent выполняет release и Local» (rule 6, scenario S1).
//!
//! The network `Ping`/`Pong` (1 s, offline after 3 s) is a different heartbeat on
//! the mTLS links and never reaches this type.
//!
//! The watchdog never reads a clock: every method takes `now_ms` from a monotonic
//! source the caller owns, which is what makes it testable and what lets the
//! agent keep using its own monotonic clock after a suspend (rule 10).

use crate::error::IpcError;

/// Default `core_silence_ms` of `AgentAttach`.
pub const DEFAULT_CORE_SILENCE_MS: u32 = 300;
/// Lower bound of `core_silence_ms` (`docs/03-architecture.md`: 200..500).
pub const MIN_CORE_SILENCE_MS: u32 = 200;
/// Upper bound of `core_silence_ms`; `/4` gives the documented ≤125 ms heartbeat.
pub const MAX_CORE_SILENCE_MS: u32 = 500;

/// Period of the heartbeat and the silence limit, both in monotonic milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watchdog {
    heartbeat_ms: u64,
    silence_limit_ms: u64,
    last_frame_ms: u64,
    last_heartbeat_ms: u64,
}

impl Watchdog {
    /// A watchdog for the `core_silence_ms` carried in `AgentAttach`.
    ///
    /// The heartbeat period is derived, never configured: `core_silence_ms / 4`,
    /// which is ≤125 ms inside the documented 200..500 range.
    pub fn for_core_silence_ms(core_silence_ms: u32, now_ms: u64) -> Result<Watchdog, IpcError> {
        if !(MIN_CORE_SILENCE_MS..=MAX_CORE_SILENCE_MS).contains(&core_silence_ms) {
            return Err(IpcError::config(format!(
                "core_silence_ms = {core_silence_ms} is outside \
                 {MIN_CORE_SILENCE_MS}..={MAX_CORE_SILENCE_MS} (AgentAttach, \
                 docs/03-architecture.md)"
            )));
        }
        Watchdog::with_periods(
            u64::from(core_silence_ms / 4),
            u64::from(core_silence_ms),
            now_ms,
        )
    }

    /// Explicit periods, for the network heartbeat (1 s / 3 s) or for tests.
    ///
    /// `heartbeat_ms` must be non-zero and not larger than the silence limit,
    /// otherwise "ping faster than the timeout" would not hold.
    pub fn with_periods(
        heartbeat_ms: u64,
        silence_limit_ms: u64,
        now_ms: u64,
    ) -> Result<Watchdog, IpcError> {
        if heartbeat_ms == 0 {
            return Err(IpcError::config("heartbeat period must be > 0"));
        }
        if silence_limit_ms < heartbeat_ms {
            return Err(IpcError::config(format!(
                "silence limit {silence_limit_ms} ms is smaller than the heartbeat period \
                 {heartbeat_ms} ms"
            )));
        }
        Ok(Watchdog {
            heartbeat_ms,
            silence_limit_ms,
            last_frame_ms: now_ms,
            last_heartbeat_ms: now_ms,
        })
    }

    /// Heartbeat period in milliseconds (`core_silence_ms / 4`).
    pub const fn heartbeat_ms(&self) -> u64 {
        self.heartbeat_ms
    }

    /// Silence limit in milliseconds (`core_silence_ms`).
    pub const fn silence_limit_ms(&self) -> u64 {
        self.silence_limit_ms
    }

    /// Record that a frame arrived: «нет ни одного кадра, включая IPC `Ping`»,
    /// so *any* frame counts as liveness.
    pub fn on_frame(&mut self, now_ms: u64) {
        self.last_frame_ms = now_ms;
    }

    /// Record that this side sent its heartbeat.
    pub fn on_heartbeat(&mut self, now_ms: u64) {
        self.last_heartbeat_ms = now_ms;
    }

    /// Whether it is time to send a heartbeat.
    pub fn heartbeat_due(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_heartbeat_ms) >= self.heartbeat_ms
    }

    /// Milliseconds until the next heartbeat is due (0 when it already is).
    pub fn next_heartbeat_in_ms(&self, now_ms: u64) -> u64 {
        self.heartbeat_ms
            .saturating_sub(now_ms.saturating_sub(self.last_heartbeat_ms))
    }

    /// How long nothing has arrived.
    pub fn silence_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_frame_ms)
    }

    /// Milliseconds left before the silence limit is exceeded (0 when it is).
    pub fn remaining_ms(&self, now_ms: u64) -> u64 {
        self.silence_limit_ms
            .saturating_sub(self.silence_ms(now_ms))
    }

    /// Whether silence already exceeds the limit.
    pub fn silence_exceeded(&self, now_ms: u64) -> bool {
        self.silence_ms(now_ms) > self.silence_limit_ms
    }

    /// `Ok(())` while the peer is alive, [`IpcError::Watchdog`] once it is not.
    pub fn check(&self, now_ms: u64) -> Result<(), IpcError> {
        if self.silence_exceeded(now_ms) {
            return Err(IpcError::Watchdog {
                silence_ms: self.silence_ms(now_ms),
                limit_ms: self.silence_limit_ms,
            });
        }
        Ok(())
    }
}
