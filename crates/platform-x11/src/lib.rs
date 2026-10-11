//! LAN Workspace X11 backend (spike that graduates into the real adapter).
//!
//! This crate is the Linux/X11 implementation of the platform-adapter boundary
//! described by **ADR-002**: it turns normalized [`input_model::InputEvent`]s into
//! X11 input and turns X11 input back into normalized events. It contains no
//! protocol, no network and no D-Bus code — the wire format and the transfer
//! state machine live in the core; an adapter only speaks the OS.
//!
//! # Injection (KVM-012)
//!
//! [`X11Injector`] drives the X server through the **XTest** extension
//! (`XTestFakeKeyEvent` / `XTestFakeButtonEvent` / `XTestFakeMotionEvent`) and
//! flushes the request buffer. Every event it actually pushed is recorded in a
//! [`PressedLedger`] so a watchdog (or an emergency abort) can always release the
//! keys and buttons that are currently down.
//!
//! Key identity is layout-independent, per **ADR-006**: HID usage -> Linux evdev
//! code (via `input-model`) -> X11 keycode of the *current XKB keymap*. Under the
//! standard XKB `evdev` rules an X11 keycode equals the evdev code plus 8, so the
//! translation is exact and needs no XKB lookup: an XKB keymap binds keycode ->
//! keysym, it does not reassign keycodes. See [`keymap`].
//!
//! # Capture (KVM-013)
//!
//! [`X11Capture`] receives **XInput2 raw events** (`XI_RawMotion`,
//! `XI_RawKeyPress`, ...) and, when asked, takes the pointer+keyboard grab with
//! `owner_events = false`. Raw events alone do not suppress local input; the grab
//! does, which is why [`X11Capture::grab`] is a separate, fallible step: a failed
//! grab cancels the transfer instead of silently passing input through.
//!
//! Two things make capture usable as a KVM source:
//!
//! * **Injected-loop prevention** — XTest fake input is attributed to the virtual
//!   XTEST slave devices (`Virtual core XTEST pointer` / `Virtual core XTEST
//!   keyboard`). [`X11Capture`] enumerates devices via XI2 at startup and drops
//!   every raw event whose `deviceid` or `sourceid` is one of them, so our own
//!   synthetic input never re-enters the transfer.
//! * **Recenter** — raw motion is relative and the cursor would otherwise stick at
//!   a screen edge. While grabbed, the cursor is warped back to the screen centre
//!   once it drifts past a threshold, and the caller keeps seeing an integrated
//!   (logical) position.
//!
//! Grab release on crash needs no cooperation from us: if the process dies the X
//! connection is closed and the server drops every grab held by that client.
//! [`X11Capture`] also implements [`Drop`] so the graceful path ungrabs explicitly.
//!
//! # Platform
//!
//! Everything here is Linux-only. On any other target the crate compiles to an
//! empty library so the workspace still builds on a developer's Windows host.

#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod inject;
#[cfg(target_os = "linux")]
pub mod keymap;

#[cfg(target_os = "linux")]
pub use capture::{CapturedEvent, X11Capture};
#[cfg(target_os = "linux")]
pub use inject::{InjectResult, PressedLedger, X11Injector};
#[cfg(target_os = "linux")]
pub use keymap::{
    evdev_to_x11_keycode, hid_usage_to_x11_keycode, x11_keycode_to_hid_usage,
    EMERGENCY_F12_X11_KEYCODE,
};

#[cfg(target_os = "linux")]
mod error {
    use std::fmt;

    /// Errors surfaced by the X11 backend.
    #[derive(Debug)]
    pub enum X11Error {
        /// Opening the display named by `$DISPLAY` failed (or `$DISPLAY` is unusable).
        Connect(x11rb::errors::ConnectError),
        /// The X connection broke, or a request could not be written to it.
        Connection(x11rb::errors::ConnectionError),
        /// The server answered a request with an X protocol error.
        Reply(x11rb::errors::ReplyError),
        /// The server lacks something this backend requires (e.g. XInput2).
        Unsupported(String),
        /// The backend reached a state it cannot continue from.
        Internal(String),
        /// Waiting for the X connection's file descriptor failed.
        Poll(rustix::io::Errno),
    }

    impl fmt::Display for X11Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                X11Error::Connect(err) => write!(f, "cannot connect to the X display: {err}"),
                X11Error::Connection(err) => write!(f, "X connection error: {err}"),
                X11Error::Reply(err) => write!(f, "X request failed: {err}"),
                X11Error::Unsupported(what) => write!(f, "unsupported X server: {what}"),
                X11Error::Internal(what) => write!(f, "X11 backend error: {what}"),
                X11Error::Poll(err) => write!(f, "polling the X connection failed: {err}"),
            }
        }
    }

    impl std::error::Error for X11Error {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                X11Error::Connect(err) => Some(err),
                X11Error::Connection(err) => Some(err),
                X11Error::Reply(err) => Some(err),
                X11Error::Poll(err) => Some(err),
                X11Error::Unsupported(_) | X11Error::Internal(_) => None,
            }
        }
    }

    impl From<x11rb::errors::ConnectError> for X11Error {
        fn from(err: x11rb::errors::ConnectError) -> Self {
            X11Error::Connect(err)
        }
    }

    impl From<x11rb::errors::ConnectionError> for X11Error {
        fn from(err: x11rb::errors::ConnectionError) -> Self {
            X11Error::Connection(err)
        }
    }

    impl From<x11rb::errors::ReplyError> for X11Error {
        fn from(err: x11rb::errors::ReplyError) -> Self {
            X11Error::Reply(err)
        }
    }

    impl From<rustix::io::Errno> for X11Error {
        fn from(err: rustix::io::Errno) -> Self {
            X11Error::Poll(err)
        }
    }
}

#[cfg(target_os = "linux")]
pub use error::X11Error;
