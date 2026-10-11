//! DPI awareness reporting and fallback.
//!
//! KVM-010/KVM-011 require the shipped executables to be `PerMonitorV2`-aware via
//! manifest (`outputs/03-architecture.md`, "Windows"). The spike embeds that
//! manifest from `build.rs` for MSVC-linked targets and *reports the resulting
//! context* so the claim is observable rather than asserted.
//!
//! Comparing the context handles with `==` is wrong: `GetThreadDpiAwarenessContext`
//! returns an opaque handle, not the documented negative pseudo-value (measured on
//! Windows 10.0.26200, where it is e.g. `0x6010` before and `0x22` after the
//! awareness is set). `AreDpiAwarenessContextsEqual` is the documented comparison
//! and is what this module uses.

use std::fmt;

use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetAwarenessFromDpiAwarenessContext,
    GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE,
    DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED,
};

/// The process's DPI awareness as Windows currently reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpiAwareness {
    /// Not DPI aware; Windows bitmap-stretches and virtualises coordinates.
    Unaware,
    /// System DPI aware: correct on the primary monitor's scale only.
    SystemAware,
    /// Per-monitor aware, v1.
    PerMonitor,
    /// Per-monitor aware, v2 (`dpiAwareness = PerMonitorV2`).
    PerMonitorV2,
    /// Unaware, with GDI scaling.
    UnawareGdiScaled,
    /// Aware, but not one of the contexts this crate knows.
    Unknown,
}

impl fmt::Display for DpiAwareness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Unaware => "unaware",
            Self::SystemAware => "system-aware",
            Self::PerMonitor => "per-monitor",
            Self::PerMonitorV2 => "PerMonitorV2",
            Self::UnawareGdiScaled => "unaware-gdiscaled",
            Self::Unknown => "unknown",
        };
        f.write_str(text)
    }
}

fn equals(context: DPI_AWARENESS_CONTEXT, expected: DPI_AWARENESS_CONTEXT) -> bool {
    // SAFETY: both handles come from the same API family; the call has no
    // preconditions beyond valid handles.
    unsafe { AreDpiAwarenessContextsEqual(context, expected).as_bool() }
}

/// The calling thread's current DPI awareness context.
///
/// Read-only: the awareness itself is established by the embedded manifest before
/// `main` runs, or by [`ensure_per_monitor_v2`].
#[must_use]
pub fn current() -> DpiAwareness {
    // SAFETY: no arguments, returns a value handle.
    let context = unsafe { GetThreadDpiAwarenessContext() };

    if equals(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        DpiAwareness::PerMonitorV2
    } else if equals(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
        DpiAwareness::PerMonitor
    } else if equals(context, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
        DpiAwareness::SystemAware
    } else if equals(context, DPI_AWARENESS_CONTEXT_UNAWARE) {
        DpiAwareness::Unaware
    } else if equals(context, DPI_AWARENESS_CONTEXT_UNAWARE_GDISCALED) {
        DpiAwareness::UnawareGdiScaled
    } else {
        // Last resort: the coarse enum cannot separate V1 from V2, but it does say
        // whether the process is per-monitor aware at all.
        // SAFETY: as above.
        match unsafe { GetAwarenessFromDpiAwarenessContext(context) }.0 {
            0 => DpiAwareness::Unaware,
            1 => DpiAwareness::SystemAware,
            2 => DpiAwareness::PerMonitor,
            _ => DpiAwareness::Unknown,
        }
    }
}

/// Whether the process runs with the manifest KVM-010/KVM-011 demand.
#[must_use]
pub fn is_per_monitor_v2() -> bool {
    current() == DpiAwareness::PerMonitorV2
}

/// Make sure the process is PerMonitorV2 aware before any coordinate is read.
///
/// The product binaries get this from the embedded manifest (`build.rs`; MSVC
/// link only). On toolchains that cannot take MSVC manifest switches, and for any
/// other consumer of this crate, the same guarantee is established here instead.
/// When the manifest already ran, Windows refuses this call — in that case the
/// refusal is irrelevant and the resulting context is still what is reported.
///
/// Measured on Windows 10.0.26200: a process started without a manifest reports
/// `Unaware`, this call succeeds, and the context afterwards compares equal to
/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`.
#[must_use]
pub fn ensure_per_monitor_v2() -> DpiAwareness {
    if !is_per_monitor_v2() {
        // SAFETY: documented call with no preconditions; failure only means the
        // awareness was already fixed, which the re-read below accounts for.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }
    current()
}
