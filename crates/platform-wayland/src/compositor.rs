//! Compositor identity and the portal-version signature of a grant (KVM-017).
//!
//! Pure logic: no D-Bus, no I/O beyond reading the session environment when detecting
//! the running compositor.

use std::fmt;

use crate::WaylandError;

/// The compositors the Wayland backend supports (ADR-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compositor {
    /// GNOME (mutter, `xdg-desktop-portal-gnome`).
    Gnome,
    /// KDE Plasma (KWin, `xdg-desktop-portal-kde`).
    Kde,
}

impl Compositor {
    /// Stable identifier used in the grant signature and in [`crate::BackendInfo`].
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gnome => "gnome",
            Self::Kde => "kde",
        }
    }

    /// Classifies a `XDG_CURRENT_DESKTOP`-style colon-separated desktop list.
    ///
    /// Ubuntu and Pop! are GNOME sessions, so their entries count as GNOME.
    pub fn from_desktop_names(names: &str) -> Option<Self> {
        names
            .split(':')
            .find_map(|name| match name.trim().to_ascii_lowercase().as_str() {
                "gnome" | "gnome-classic" | "gnome-flashback" | "gnome-xorg" | "ubuntu" | "pop" => {
                    Some(Self::Gnome)
                }
                "kde" | "plasma" | "plasmawayland" => Some(Self::Kde),
                _ => None,
            })
    }

    /// Detects the running compositor from the session environment.
    ///
    /// # Errors
    ///
    /// [`WaylandError::UnsupportedCompositor`] when none of
    /// `XDG_CURRENT_DESKTOP`, `XDG_SESSION_DESKTOP` and `DESKTOP_SESSION` names a
    /// compositor this backend implements.
    pub fn detect() -> Result<Self, WaylandError> {
        const VARS: [&str; 3] = [
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "DESKTOP_SESSION",
        ];
        for var in VARS {
            if let Ok(value) = std::env::var(var) {
                if let Some(compositor) = Self::from_desktop_names(&value) {
                    return Ok(compositor);
                }
            }
        }
        Err(WaylandError::UnsupportedCompositor {
            xdg_current_desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        })
    }
}

impl fmt::Display for Compositor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Versions of the four portal interfaces the backend cares about.
///
/// `0` means the interface is absent, which is how the portal signals "not
/// implemented by this compositor" to [`crate::portal_versions`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalVersions {
    /// `org.freedesktop.portal.InputCapture` version.
    pub input_capture: u32,
    /// `org.freedesktop.portal.RemoteDesktop` version.
    pub remote_desktop: u32,
    /// `org.freedesktop.portal.Clipboard` version; only recorded, never driven.
    pub clipboard: u32,
    /// `org.freedesktop.portal.GlobalShortcuts` version; `0` on GNOME 46.
    pub global_shortcuts: u32,
}

impl PortalVersions {
    /// `true` when every interface answered.
    pub const fn all_present(&self) -> bool {
        self.input_capture > 0
            && self.remote_desktop > 0
            && self.clipboard > 0
            && self.global_shortcuts > 0
    }
}

/// Signature of the portal endpoint that issued a restore token (KVM-017).
///
/// Restore tokens are only valid for the application identity, compositor and portal
/// versions that issued them. Including the app id invalidates pre-Bifrost grants
/// without sending their obsolete bearer tokens to the portal.
#[must_use]
pub fn compositor_signature(compositor: Compositor, versions: &PortalVersions) -> String {
    format!(
        "bifrost;{};ic={};rd={};cb={};gs={}",
        compositor.as_str(),
        versions.input_capture,
        versions.remote_desktop,
        versions.clipboard,
        versions.global_shortcuts
    )
}

/// The portal interfaces a compositor exposes at the recorded versions, for
/// [`crate::BackendInfo::portals`].
#[must_use]
pub fn portal_interfaces(versions: &PortalVersions) -> Vec<&'static str> {
    let mut interfaces = Vec::new();
    if versions.input_capture > 0 {
        interfaces.push(crate::INPUT_CAPTURE_INTERFACE);
    }
    if versions.remote_desktop > 0 {
        interfaces.push(crate::REMOTE_DESKTOP_INTERFACE);
    }
    if versions.clipboard > 0 {
        interfaces.push(crate::CLIPBOARD_INTERFACE);
    }
    if versions.global_shortcuts > 0 {
        interfaces.push(crate::GLOBAL_SHORTCUTS_INTERFACE);
    }
    interfaces
}
