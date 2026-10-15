//! LAN Workspace Wayland backend: XDG desktop portal sessions plus EIS I/O (KVM-015..KVM-018).
//!
//! This crate is the only place in the product that speaks D-Bus and EIS. Everything
//! it needs is pure Rust (`ashpd` for the portals, `reis` for the libei/EIS protocol),
//! so there is no `libei`, `libportal` or `libinput` system dependency (ADR-002).
//!
//! # Two halves
//!
//! * `capture` — `org.freedesktop.portal.InputCapture`: the compositor hands input to
//!   us, we integrate relative pointer deltas from an absolute seed, and we `Release`
//!   input back to the compositor when the peer takes over.
//! * `inject` — `org.freedesktop.portal.RemoteDesktop`: we receive an EIS file
//!   descriptor and drive absolute or relative pointer, keyboard, button, and scroll
//!   devices over it.
//!
//! `compositor`, `grant` and `shortcuts` hold the pure logic: compositor detection and
//! the portal-version signature, restore-token storage (KVM-017) and the
//! `org.freedesktop.portal.GlobalShortcuts` emergency hotkey (KVM-018).
//!
//! `overlay` is unrelated to input: the optional edge-effect strip, a wlr-layer-shell
//! surface on its own thread and connection (`edgefx` owns the animation).
//!
//! # Runtime
//!
//! The async runtime is **tokio**, because `reis`'s `tokio` feature hands us the EIS
//! event stream as a `tokio`-driven `Stream` and `ashpd`'s default feature is `tokio`
//! as well; one runtime therefore covers D-Bus, signals and EIS in a single
//! `select!`. The crate never spawns tasks of its own: every future is polled by the
//! caller's runtime.
//!
//! `WaylandCapture` and `WaylandInjector` are **not `Send`**: reis' high-level EIS
//! event stream keeps `Box<dyn FnOnce(u64)>` callback state, which is not `Send`. They
//! are meant to be owned by one task on one thread (as in the spike examples); a
//! multi-threaded agent should keep the backend on its own task and talk to it over a
//! channel instead of moving it between threads.
//!
//! # Notable protocol constraints (verified against the portal and EIS docs)
//!
//! * The compositor decides when capture starts; absolute cursor position is only
//!   known from `InputCapture.Activated`. Deltas that arrive before we know a position
//!   are counted and dropped rather than guessed.
//! * `SetPointerBarriers` is only accepted while the capture session is *disabled*, and
//!   on GNOME 46 `Disable` cannot be undone (the session has to be recreated), so
//!   `WaylandCapture::set_barriers` refuses to run once `enable` was called.
//! * A sender may only emulate input on a device the EIS server has *resumed*; a
//!   paused device is treated as neutral (the server releases keys/buttons itself).
//! * Two smooth scroll conventions exist: `ei_scroll.scroll_discrete` counts 120ths of
//!   a wheel detent, `ei_scroll.scroll` is pixel-precise. `Scroll{h120, v120}`
//!   therefore maps onto `scroll_discrete` with the sub-120 remainder kept in the
//!   backend, and the smooth form is converted with the `wl_pointer` convention
//!   (10 axis units == 1 detent == 120).

#![forbid(unsafe_code)]

// The D-Bus interface names are part of the Linux-only surface: nothing outside this
// backend needs them, and gating them keeps the crate an empty lib on other platforms.

/// `org.freedesktop.portal.InputCapture` (KVM-015).
#[cfg(target_os = "linux")]
pub const INPUT_CAPTURE_INTERFACE: &str = "org.freedesktop.portal.InputCapture";
/// `org.freedesktop.portal.RemoteDesktop` (KVM-016).
#[cfg(target_os = "linux")]
pub const REMOTE_DESKTOP_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
/// `org.freedesktop.portal.Clipboard`; only its availability is recorded.
#[cfg(target_os = "linux")]
pub const CLIPBOARD_INTERFACE: &str = "org.freedesktop.portal.Clipboard";
/// `org.freedesktop.portal.GlobalShortcuts` (KVM-018).
#[cfg(target_os = "linux")]
pub const GLOBAL_SHORTCUTS_INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

// The clipboard decision is pure (the portal's own constraints are the input), so it builds
// and tests on any host; the portal stream itself is not driven yet, see the module.
pub mod clipboard;

mod data_control;

#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod compositor;
#[cfg(target_os = "linux")]
mod grant;
#[cfg(target_os = "linux")]
mod inject;
#[cfg(target_os = "linux")]
pub mod overlay;
#[cfg(target_os = "linux")]
mod relative;
#[cfg(target_os = "linux")]
mod shortcuts;

pub use clipboard::{availability, Availability, Unavailable};

#[cfg(target_os = "linux")]
pub use data_control::Clipboard;

#[cfg(target_os = "linux")]
pub use capture::{portal_versions, CapturedEvent, EmergencyHandle, MotionCounts, WaylandCapture};
#[cfg(target_os = "linux")]
pub use compositor::{compositor_signature, portal_interfaces, Compositor, PortalVersions};
#[cfg(target_os = "linux")]
pub use grant::{GrantStore, PortalGrant};
#[cfg(target_os = "linux")]
pub use inject::{InjectOutcome, InjectSource, Region, WaylandCapabilities, WaylandInjector};
#[cfg(target_os = "linux")]
pub use relative::{
    select_pointer_mode, PointerMode, RelativeBounds, RelativeCursor, RelativeMotion,
};
#[cfg(target_os = "linux")]
pub use shortcuts::EmergencyShortcuts;

#[cfg(target_os = "linux")]
pub use ashpd::desktop::input_capture::{Barrier, BarrierID, BarrierPosition, Capabilities};

/// `BackendInfo` from `docs/ipc.proto`, as observed by this backend.
///
/// The fields are filled in from facts the backend can verify rather than from
/// compile-time assumptions: `portals` lists the portal interfaces the compositor
/// actually exposes, `has_emergency_hotkey` is derived from the GlobalShortcuts
/// interface being present (false on GNOME 46), and `has_portal_persistence` is
/// derived from the portal issuing a restore token.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    /// Backend identifier: always `wayland`.
    pub backend: String,
    /// Compositor identifier: `gnome` or `kde`.
    pub compositor: String,
    /// Portal interfaces available at the recorded versions.
    pub portals: Vec<&'static str>,
    /// Whether the emergency hotkey could be registered (KVM-018).
    pub has_emergency_hotkey: bool,
    /// Whether the compositor issues restore tokens (KVM-017).
    pub has_portal_persistence: bool,
    /// Whether the compositor can hand input capture to us (receiver takeover).
    pub has_receiver_takeover: bool,
    /// Whether absolute pointer injection is available (region established).
    pub has_absolute_injection: bool,
}

#[cfg(target_os = "linux")]
impl BackendInfo {
    /// Backend identifier used by [`BackendInfo::backend`].
    pub const BACKEND: &'static str = "wayland";
}

/// Everything that can go wrong in the Wayland backend.
#[cfg(target_os = "linux")]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WaylandError {
    /// A portal call failed.
    #[error("desktop portal call failed: {0}")]
    Portal(#[from] ashpd::Error),
    /// The EIS protocol or its transport failed.
    #[error("EIS protocol error: {0}")]
    Ei(#[from] reis::Error),
    /// Local I/O failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The compositor's portal does not implement the interface at all.
    #[error("portal interface {portal_interface} is not available on this compositor")]
    PortalUnavailable {
        /// The missing D-Bus interface.
        portal_interface: &'static str,
    },
    /// The portal implements the interface, but not the version we need.
    #[error(
        "portal interface {portal_interface} requires version {required}, the compositor implements {found}"
    )]
    PortalTooOld {
        /// The D-Bus interface.
        portal_interface: &'static str,
        /// Version we need.
        required: u32,
        /// Version the compositor offers.
        found: u32,
    },
    /// The emergency hotkey interface is missing (KVM-018); this is deliberately a
    /// hard error and never a silent no-op.
    #[error(
        "emergency hotkey unavailable: this compositor does not implement {portal_interface} \
         (xdg-desktop-portal-gnome 48 or newer is required on GNOME; GNOME 46 has no \
         GlobalShortcuts portal)"
    )]
    HotkeyUnavailable {
        /// The D-Bus interface that is required for the hotkey.
        portal_interface: &'static str,
    },
    /// Neither GNOME nor KDE: the backend has no EIS/portal behaviour for it.
    #[error(
        "unsupported compositor (XDG_CURRENT_DESKTOP={xdg_current_desktop:?}); the Wayland \
         backend supports GNOME and KDE only"
    )]
    UnsupportedCompositor {
        /// The value we could not classify.
        xdg_current_desktop: String,
    },
    /// No absolute pointer region is available for absolute injection.
    #[error(
        "the EIS server advertised no absolute pointer region; absolute motion is unavailable"
    )]
    NoAbsoluteRegion,
    /// The requested point is outside every region the EIS server accepts.
    #[error("position ({x}, {y}) is outside every EIS region; EIS would discard the event")]
    PositionOutOfRegion {
        /// Requested logical X.
        x: f64,
        /// Requested logical Y.
        y: f64,
    },
    /// There is no activation to release input for.
    #[error("the capture session is not active (no activation id)")]
    CaptureNotActive,
    /// Barriers cannot be changed on a live session; see the crate docs.
    #[error(
        "pointer barriers cannot be changed while capture is enabled: on GNOME 46 a disabled \
         session cannot be re-enabled, so barriers must be set before enable() or the session \
         must be recreated"
    )]
    BarriersWhileEnabled,
    /// The sender device was never resumed by the EIS server.
    #[error(
        "the EIS device was not resumed within {seconds} s; input cannot be injected until the \
         compositor resumes it"
    )]
    DeviceNotResumed {
        /// How long we waited.
        seconds: u64,
    },
    /// The EIS event stream ended.
    #[error("the EIS connection ended")]
    EiDisconnected,
    /// The EIS server advertised no device after we bound seat capabilities.
    #[error("the EIS server advertised no device after seat capabilities were bound")]
    NoEiDevice,
    /// No EIS device can inject absolute or relative pointer motion.
    #[error("the EIS server advertised no pointer injection device")]
    NoPointerDevice,
    /// The EIS device does not implement the interface an event needs.
    #[error("the EIS device has no {interface} interface, so this event cannot be injected")]
    MissingInterface {
        /// EIS interface name, for example `ei_keyboard`.
        interface: &'static str,
    },
    /// The EIS device is paused, so the compositor would discard emulated input.
    #[error("the EIS device is paused; input is not injected until the compositor resumes it")]
    DevicePaused,
    /// `--direct` was requested but there is no compositor-provided ei socket.
    #[error("LIBEI_SOCKET is not set (or not connectable): --direct needs a compositor-spawned ei socket")]
    EiSocketUnavailable,
    /// The wire event cannot be expressed with an evdev code.
    #[error("no evdev code for HID usage page {page:#06x} usage {usage:#06x}")]
    UnmappedUsage {
        /// HID usage page.
        page: u16,
        /// HID usage id.
        usage: u16,
    },
    /// A persisted grant blob could not be parsed.
    #[error("malformed grant store: {0}")]
    InvalidGrant(String),
}

/// Maps the two portal errors that mean "this compositor cannot do it" onto typed
/// variants so callers can tell "not supported here" from "call failed".
#[cfg(target_os = "linux")]
pub(crate) fn map_portal_error(
    portal_interface: &'static str,
    error: ashpd::Error,
) -> WaylandError {
    match error {
        ashpd::Error::PortalNotFound(_) => WaylandError::PortalUnavailable { portal_interface },
        ashpd::Error::RequiresVersion(required, found) => WaylandError::PortalTooOld {
            portal_interface,
            required,
            found,
        },
        other => WaylandError::Portal(other),
    }
}
