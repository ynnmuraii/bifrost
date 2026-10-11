//! The injectable environment snapshot consumed by the pure [`crate::decide`].
//!
//! Everything here is plain data: no I/O, no `std::env`, no D-Bus. The Linux
//! [`crate::probe`] module fills a snapshot from the real machine; a test fills it
//! by hand.

use std::collections::BTreeMap;

/// Which operating system the snapshot describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Microsoft Windows.
    Windows,
    /// Linux (X11 or Wayland).
    Linux,
}

/// `version` properties of the `org.freedesktop.portal.*` interfaces this product
/// depends on. `0` means the interface is absent from the portal (which is how a
/// portal older than the feature and a missing backend both look on D-Bus).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalVersions {
    /// `org.freedesktop.portal.InputCapture` — `>= 1` for `ConnectToEIS`/`GetZones`.
    pub input_capture: u32,
    /// `org.freedesktop.portal.RemoteDesktop` — `>= 2` for `ConnectToEIS`.
    pub remote_desktop: u32,
    /// `org.freedesktop.portal.Clipboard` — `>= 1` when present.
    pub clipboard: u32,
    /// `org.freedesktop.portal.GlobalShortcuts` — `>= 1` when present.
    pub global_shortcuts: u32,
}

impl PortalVersions {
    /// The D-Bus interface name of the input-capture portal.
    pub const INPUT_CAPTURE: &'static str = "org.freedesktop.portal.InputCapture";
    /// The D-Bus interface name of the remote-desktop portal.
    pub const REMOTE_DESKTOP: &'static str = "org.freedesktop.portal.RemoteDesktop";
    /// The D-Bus interface name of the clipboard portal.
    pub const CLIPBOARD: &'static str = "org.freedesktop.portal.Clipboard";
    /// The D-Bus interface name of the global-shortcuts portal.
    pub const GLOBAL_SHORTCUTS: &'static str = "org.freedesktop.portal.GlobalShortcuts";

    /// Version of a named portal interface, as recorded in the snapshot.
    pub fn get(&self, interface: &str) -> u32 {
        match interface {
            Self::INPUT_CAPTURE => self.input_capture,
            Self::REMOTE_DESKTOP => self.remote_desktop,
            Self::CLIPBOARD => self.clipboard,
            Self::GLOBAL_SHORTCUTS => self.global_shortcuts,
            _ => 0,
        }
    }

    /// `(interface, version)` pairs in a stable order, for `BackendInfo.portals`.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, u32)> + '_ {
        [
            (Self::INPUT_CAPTURE, self.input_capture),
            (Self::REMOTE_DESKTOP, self.remote_desktop),
            (Self::CLIPBOARD, self.clipboard),
            (Self::GLOBAL_SHORTCUTS, self.global_shortcuts),
        ]
        .into_iter()
    }
}

/// Presence of the X11 extensions the X11 backend needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11Extensions {
    /// The `XTEST` extension (input injection).
    pub xtest: bool,
    /// The `XInputExtension` at version `>= 2.0` (raw capture).
    pub xi2: bool,
    /// The `RANDR` extension (display topology).
    pub randr: bool,
}

impl X11Extensions {
    /// All three extensions present.
    pub const fn all_present(self) -> bool {
        self.xtest && self.xi2 && self.randr
    }

    /// Comma-separated names of the missing extensions, for an error message.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.xtest {
            missing.push("XTEST");
        }
        if !self.xi2 {
            missing.push("XInput2");
        }
        if !self.randr {
            missing.push("RANDR");
        }
        missing
    }
}

/// A complete, injectable picture of the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    /// Host operating system.
    pub host: HostOs,
    /// `XDG_SESSION_TYPE`.
    pub xdg_session_type: Option<String>,
    /// `WAYLAND_DISPLAY`.
    pub wayland_display: Option<String>,
    /// `DISPLAY` (also present under Xwayland).
    pub display: Option<String>,
    /// `XDG_CURRENT_DESKTOP` (e.g. `KDE`, `ubuntu:GNOME`).
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_DESKTOP` (e.g. `KDE`, `gnome`).
    pub xdg_session_desktop: Option<String>,
    /// Backend token of the `org.freedesktop.impl.portal.desktop.*` bus name that
    /// currently has an owner: `gnome`, `kde`, `wlr`, `hyprland`, `lxqt`, ...
    pub impl_portal_backend: Option<String>,
    /// Portal interface versions.
    pub portals: PortalVersions,
    /// `true` when the session bus answered; `false` when the snapshot could not
    /// read `org.freedesktop.portal.Desktop` at all.
    pub portals_queried: bool,
    /// X11 extension presence, when an X server was reachable.
    pub x11: Option<X11Extensions>,
}

impl EnvSnapshot {
    /// An empty snapshot for `host` (nothing detected).
    pub fn new(host: HostOs) -> Self {
        Self {
            host,
            xdg_session_type: None,
            wayland_display: None,
            display: None,
            xdg_current_desktop: None,
            xdg_session_desktop: None,
            impl_portal_backend: None,
            portals: PortalVersions::default(),
            portals_queried: false,
            x11: None,
        }
    }

    /// Builds a snapshot from an environment-variable list.
    ///
    /// This is the pure seam that keeps "unit tests on substituted environment
    /// variables" (KVM-025) possible: the caller decides where the pairs come from
    /// (`std::env::vars()`, `std::env::vars_os()`, a literal in a test).
    /// Portal versions and X11 extensions stay empty; use [`Self::with_portals`]
    /// and [`Self::with_x11`].
    pub fn from_vars<I, K, V>(host: HostOs, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let map: BTreeMap<String, String> = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let get = |key: &str| {
            map.get(key)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        Self {
            host,
            xdg_session_type: get("XDG_SESSION_TYPE"),
            wayland_display: get("WAYLAND_DISPLAY"),
            display: get("DISPLAY"),
            xdg_current_desktop: get("XDG_CURRENT_DESKTOP"),
            xdg_session_desktop: get("XDG_SESSION_DESKTOP"),
            impl_portal_backend: None,
            portals: PortalVersions::default(),
            portals_queried: false,
            x11: None,
        }
    }

    /// Attaches the portal versions and the impl-portal backend token.
    pub fn with_portals(
        mut self,
        portals: PortalVersions,
        impl_portal_backend: Option<String>,
    ) -> Self {
        self.portals = portals;
        self.impl_portal_backend = impl_portal_backend;
        self.portals_queried = true;
        self
    }

    /// Attaches the X11 extension presence.
    pub fn with_x11(mut self, x11: X11Extensions) -> Self {
        self.x11 = Some(x11);
        self
    }

    /// `true` when both `WAYLAND_DISPLAY` and `DISPLAY` are set: the combined
    /// signature of Xwayland (rule 5).
    pub fn has_xwayland(&self) -> bool {
        self.wayland_display.is_some() && self.display.is_some()
    }
}
