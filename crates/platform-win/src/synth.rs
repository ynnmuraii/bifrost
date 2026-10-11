//! Synthetic-motion budget.
//!
//! Keyboard, button and wheel injections carry [`crate::INJECT_TAG`] and the OS
//! injected flag, so the low-level hooks drop them. Raw Input has no per-event
//! marker at all (`RAWMOUSE` has no injected bit), so relative motion produced by
//! our own `SendInput` is indistinguishable from a real device unless we say so.
//!
//! The injector therefore declares up front how many motion records it is about
//! to emit; the Raw Input handler consumes one token per motion event that
//! arrives and drops exactly those. The budget is a per-process global because
//! there is exactly one injection path and one capture path in the agent.
//!
//! This is an explicit approximation and it is stated as such: it can drop a
//! genuine device event that happens to race an injection, and it silently
//! under-consumes if the OS coalesces injected motion. The clean fix belongs to
//! the real adapter (dedicated device identity / `RIDEV_EXCLUDE`) and is out of
//! spike scope.

use std::sync::atomic::{AtomicI32, Ordering};

static BUDGET: AtomicI32 = AtomicI32::new(0);

/// Adjust the budget by `n` tokens.
///
/// Positive values declare upcoming synthetic motion; a negative value retracts
/// a declaration that turned out not to be needed (a `SendInput` batch that was
/// only partially accepted). Retraction may make the budget transiently
/// negative, which only means that no motion is suppressed until real synthetics
/// are declared again.
pub fn expect_motions(n: i32) {
    if n != 0 {
        BUDGET.fetch_add(n, Ordering::SeqCst);
    }
}

/// Consume one token, if any. Returns `true` when the caller should treat the
/// motion event as its own.
#[must_use]
pub fn consume_motion() -> bool {
    let mut current = BUDGET.load(Ordering::SeqCst);
    while current > 0 {
        match BUDGET.compare_exchange_weak(current, current - 1, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
    false
}

/// Current number of unconsumed tokens. For diagnostics and tests.
#[must_use]
pub fn pending() -> i32 {
    BUDGET.load(Ordering::SeqCst)
}
