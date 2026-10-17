//! Bifrost session detection and backend selection (KVM-014 / KVM-025).
//!
//! The decision is a **pure function** over an injectable [`EnvSnapshot`]:
//! [`decide`] never touches the environment, the D-Bus or an X server, which is
//! what makes KVM-025's "unit tests on substituted environment variables" possible.
//! The I/O that *builds* a snapshot (environment variables, the `version`
//! properties of `org.freedesktop.portal.*` over the session bus, the presence of
//! the X11 extensions) lives in [`probe`] and is Linux-only.
//!
//! Rules implemented here are the numbered list in `docs/03-architecture.md`,
//! section "Определение сеанса и выбор платформенного бэкенда":
//!
//! 1. `XDG_SESSION_TYPE=wayland|x11` picks the branch; when it is absent or `tty`,
//!    `WAYLAND_DISPLAY` wins over `DISPLAY`; neither means `UnsupportedSession`.
//! 2. X11 requires XTEST, XInput2 and RandR.
//! 3. Wayland requires `WAYLAND_DISPLAY` and `RemoteDesktop >= 2`; persistent capture additionally needs `InputCapture >= 2`.
//! 4. GNOME (Mutter) and KDE (KWin) are level 1; wlroots is level 2 (post-MVP).
//! 5. `DISPLAY` + `WAYLAND_DISPLAY` means Xwayland: the session stays Wayland and
//!    `platform-x11` is NEVER chosen for it.
//! 6. `[platform].backend` overrides auto-detection but never the checks: a
//!    mismatch is an error, never a silent fallback to another backend.
#![forbid(unsafe_code)]

pub mod decision;
pub mod env;

pub use decision::{
    compositor_from_desktop_string, compositor_from_impl_portal, decide, Backend, BackendInfo,
    BackendOverride, Capabilities, Compositor, PortalInterface, ProbeError, SessionInfo,
    SessionKind, UnsupportedSession, MIN_INPUT_CAPTURE_VERSION, MIN_REMOTE_DESKTOP_VERSION,
    MIN_XDP_VERSION,
};
pub use env::{EnvSnapshot, HostOs, PortalVersions, X11Extensions};

pub mod system_name;

#[cfg(target_os = "linux")]
pub mod probe;

#[cfg(target_os = "linux")]
pub use probe::{probe, read_env_snapshot, read_portals, read_snapshot, read_x11_extensions};
