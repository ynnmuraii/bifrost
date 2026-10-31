//! PTT uses the existing cross-process host clock, never a process-local origin.
pub(crate) fn now_ns() -> Option<u64> {
    // Round the microsecond sample upward so truncation cannot extend a native lease.
    ipc::clock::host_monotonic_us()?
        .checked_add(1)?
        .checked_mul(1000)
}
