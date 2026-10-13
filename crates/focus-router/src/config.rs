//! Timing parameters of the focus/lease layer.
//!
//! Values and bounds are from `docs/05-protocol.md`, «Watchdog без зависимости от
//! синхронизации часов»: «Таймауты настраиваются только внутри пределов ...:
//! `prepare_timeout_ms` 150..400 (по умолчанию 200), `lease_ttl_ms` 300..1000 (по
//! умолчанию 500), `challenge = lease_ttl/5` вычисляется, `agent_core_silence_ms`
//! 200..500, `repeat_guard_ms` 150. Все дедлайны lease выводятся из одного
//! параметра `lease_ttl_ms`».
//!
//! `agent_core_silence_ms` is deliberately **not** here: it configures the agent's own
//! IPC watchdog (`AgentAttach.core_silence_ms`), not the focus FSM, and the agent
//! applies it without asking the router.

use std::fmt;

/// Focus/lease timings, in milliseconds of a monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// `prepare_timeout_ms` (150..=400, default 200): how long a receiver may stay in
    /// `Prepared` without `EnterCommit`, and how long a temporary source capture is
    /// given before the transition is abandoned.
    pub prepare_timeout_ms: u64,
    /// `lease_ttl_ms` (300..=1000, default 500): every lease deadline derives from it.
    pub lease_ttl_ms: u64,
    /// `claim_grant_timeout_ms` (default 600): how long a claimant waits for the
    /// current owner to hand off before self-granting
    /// (`docs/03-architecture.md`, «Владение вводом»).
    pub claim_grant_timeout_ms: u64,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            prepare_timeout_ms: 200,
            lease_ttl_ms: 500,
            claim_grant_timeout_ms: 600,
        }
    }
}

impl Timing {
    /// `repeat_guard_ms`: receiver-side autorepeat stops after this much silence in
    /// both input and renew (AC-14).
    ///
    /// A protocol limit, **not** a tunable: «Пределы протокола (размер фрейма, 128
    /// событий, chunk 16 KiB) не настраиваются» and «`repeat_guard_ms` 150»
    /// (`docs/03-architecture.md`, «Что параметризуется, а что нет»; `docs/05-protocol.md`).
    pub const REPEAT_GUARD_MS: u64 = 150;

    /// Lower bound of `lease_ttl_ms` (`docs/05-protocol.md`: 300..1000).
    pub const LEASE_TTL_MIN_MS: u64 = 300;
    /// Upper bound of `lease_ttl_ms` (`docs/05-protocol.md`: 300..1000).
    pub const LEASE_TTL_MAX_MS: u64 = 1_000;

    /// The `lease_ttl_ms` a *peer* advertised, clamped to the documented range.
    ///
    /// A `LeaseChallenge` carries the receiver's `lease_ttl_ms`, and that value governs the
    /// lease's deadlines on both sides («Все дедлайны lease выводятся из одного параметра
    /// `lease_ttl_ms`»). The wire field is a peer's number, so it is clamped before it governs
    /// a deadline of ours: a broken or hostile peer must not be able to park a watchdog a
    /// millisecond or a day away.
    pub const fn clamp_lease_ttl_ms(ttl_ms: u64) -> u64 {
        if ttl_ms < Self::LEASE_TTL_MIN_MS {
            Self::LEASE_TTL_MIN_MS
        } else if ttl_ms > Self::LEASE_TTL_MAX_MS {
            Self::LEASE_TTL_MAX_MS
        } else {
            ttl_ms
        }
    }

    /// Challenge period: `lease_ttl_ms / 5` (default 100 ms).
    ///
    /// Computed, never configured — «`challenge = lease_ttl/5` вычисляется, а не
    /// задаётся» (`docs/06-development-plan.md`, config table).
    pub const fn challenge_interval_ms(&self) -> u64 {
        self.lease_ttl_ms / 5
    }

    /// Deadline for a lease whose challenge was issued at `issue_ms`.
    ///
    /// «Новый deadline ставится на `issue_time + lease_ttl`, а не `receive_time +
    /// lease_ttl`» — the challenge issue time is the anchor, so a delayed old renew
    /// cannot stretch the lease indefinitely.
    pub const fn lease_deadline_ms(&self, issue_ms: u64) -> u64 {
        issue_ms.saturating_add(self.lease_ttl_ms)
    }

    /// Bound check for every field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        check("prepare_timeout_ms", self.prepare_timeout_ms, 150, 400)?;
        check(
            "lease_ttl_ms",
            self.lease_ttl_ms,
            Self::LEASE_TTL_MIN_MS,
            Self::LEASE_TTL_MAX_MS,
        )?;
        if self.claim_grant_timeout_ms == 0 {
            return Err(ConfigError {
                field: "claim_grant_timeout_ms",
                value: self.claim_grant_timeout_ms,
                expected: "> 0",
            });
        }
        // A challenge interval of 0 would issue challenges in a busy loop.
        if self.challenge_interval_ms() == 0 {
            return Err(ConfigError {
                field: "lease_ttl_ms",
                value: self.lease_ttl_ms,
                expected: ">= 5 so that lease_ttl/5 is a usable challenge period",
            });
        }
        Ok(())
    }
}

fn check(field: &'static str, value: u64, low: u64, high: u64) -> Result<(), ConfigError> {
    if (low..=high).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError {
            field,
            value,
            expected: "in the documented range",
        })
    }
}

/// A timing value outside its documented bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigError {
    /// Offending field.
    pub field: &'static str,
    /// Value we were given.
    pub value: u64,
    /// What the docs allow.
    pub expected: &'static str,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "timing.{} = {} is invalid: {}",
            self.field, self.value, self.expected
        )
    }
}

impl std::error::Error for ConfigError {}
