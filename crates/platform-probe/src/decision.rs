//! The pure backend decision: [`decide`] plus the types it produces.
//!
//! Mirrors `docs/03-architecture.md` ("Определение сеанса и выбор платформенного
//! бэкенда") and `docs/ipc.proto` (`BackendKind`, `BackendInfo`, `AgentReady`).

use std::fmt;

use crate::env::{EnvSnapshot, HostOs, PortalVersions};

/// Minimum `org.freedesktop.portal.RemoteDesktop` version for `ConnectToEIS`.
pub const MIN_REMOTE_DESKTOP_VERSION: u32 = 2;
/// Minimum `org.freedesktop.portal.InputCapture` version for `ConnectToEIS`/`GetZones`.
pub const MIN_INPUT_CAPTURE_VERSION: u32 = 1;
/// The portal frontend version that first shipped `InputCapture` (XDP 1.17.0).
pub const MIN_XDP_VERSION: &str = "1.17.0";

/// `[platform].backend` from `config.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendOverride {
    /// Detect Windows / X11 / Wayland.
    #[default]
    Auto,
    /// Force `platform-x11`; refused when the session is Wayland.
    X11,
    /// Force `platform-wayland`; refused when the session is a real X11 session.
    Wayland,
}

impl BackendOverride {
    /// Parses the config value (`auto`, `x11`, `wayland`, case-insensitive).
    pub fn parse(value: &str) -> Result<Self, UnsupportedSession> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(BackendOverride::Auto),
            "x11" => Ok(BackendOverride::X11),
            "wayland" => Ok(BackendOverride::Wayland),
            other => Err(UnsupportedSession::new(format!(
                "[platform].backend = {other:?} is not one of \"auto\", \"x11\", \"wayland\""
            ))),
        }
    }
}

impl fmt::Display for BackendOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BackendOverride::Auto => "auto",
            BackendOverride::X11 => "x11",
            BackendOverride::Wayland => "wayland",
        })
    }
}

/// The kind of graphical session detected on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A Windows user session.
    Windows,
    /// A real X11 session (not Xwayland).
    X11,
    /// A Wayland session.
    Wayland,
}

impl SessionKind {
    /// The `session_kind` string sent in `AgentReady.session_kind`.
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionKind::Windows => "windows",
            SessionKind::X11 => "x11",
            SessionKind::Wayland => "wayland",
        }
    }
}

/// A compositor as identified by `XDG_CURRENT_DESKTOP` / the impl-portal owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compositor {
    /// GNOME / Mutter, via `xdg-desktop-portal-gnome`.
    Gnome,
    /// KDE Plasma / KWin, via `xdg-desktop-portal-kde`.
    Kde,
    /// A wlroots compositor (`Sway`, `Hyprland`, `Wayfire`, `river`, ...):
    /// level 2, after the MVP (ADR-002).
    Wlroots(String),
    /// Any other desktop, unsupported for the Wayland backend.
    Other(String),
}

impl Compositor {
    /// `true` for the level-1 compositors whose portals this backend speaks.
    pub const fn is_supported(&self) -> bool {
        matches!(self, Compositor::Gnome | Compositor::Kde)
    }

    /// The value for `BackendInfo.compositor` (`mutter` | `kwin` | the raw name).
    pub fn ipc_name(&self) -> String {
        match self {
            Compositor::Gnome => "mutter".to_owned(),
            Compositor::Kde => "kwin".to_owned(),
            Compositor::Wlroots(name) | Compositor::Other(name) => name.clone(),
        }
    }

    /// The declarative name used in logs and GUI text.
    pub fn display_name(&self) -> String {
        match self {
            Compositor::Gnome => "GNOME (mutter)".to_owned(),
            Compositor::Kde => "KDE Plasma (kwin)".to_owned(),
            Compositor::Wlroots(name) | Compositor::Other(name) => name.clone(),
        }
    }
}

/// The chosen platform backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `platform-win`.
    Windows,
    /// `platform-x11`.
    X11,
    /// `platform-wayland`. Only the level-1 compositors are reachable here.
    Wayland {
        /// The detected compositor (always [`Compositor::Gnome`] or
        /// [`Compositor::Kde`]).
        compositor: Compositor,
    },
}

impl Backend {
    /// The backend name for `BackendInfo.backend` and the adapter layer.
    pub const fn name(&self) -> &'static str {
        match self {
            Backend::Windows => "platform-win",
            Backend::X11 => "platform-x11",
            Backend::Wayland { .. } => "platform-wayland",
        }
    }
}

/// Capability bits derived from the session, ready for `AgentReady.capabilities`.
///
/// Field names follow `docs/protocol.proto` `Capability`; the strings produced by
/// [`Capabilities::advertised`] drop the `CAPABILITY_` prefix, which is how
/// `AgentReady.capabilities` carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `CAPABILITY_CAPTURE`.
    pub capture: bool,
    /// `CAPABILITY_INJECT`.
    pub inject: bool,
    /// `CAPABILITY_ABSOLUTE_POINTER`.
    pub absolute_pointer: bool,
    /// `CAPABILITY_PHYSICAL_HID_KEYS`.
    pub physical_hid_keys: bool,
    /// `CAPABILITY_TEXT_CLIPBOARD` — declared only after the clipboard portal says
    /// `clipboard_enabled`, so it is `false` at probe time (ADR-008).
    pub text_clipboard: bool,
    /// `CAPABILITY_NATIVE_REPEAT`.
    pub native_repeat: bool,
    /// `CAPABILITY_SOFTWARE_REPEAT`.
    pub software_repeat: bool,
    /// `CAPABILITY_RECEIVER_TAKEOVER`.
    pub receiver_takeover: bool,
    /// `CAPABILITY_DISPLAYS_CHANGED`.
    pub displays_changed: bool,
    /// `CAPABILITY_EMERGENCY_HOTKEY`.
    pub emergency_hotkey: bool,
}

impl Capabilities {
    /// The capability names to advertise in `Hello`/`AgentReady`.
    pub fn advertised(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut push = |on: bool, name: &'static str| {
            if on {
                out.push(name);
            }
        };
        push(self.capture, "CAPTURE");
        push(self.inject, "INJECT");
        push(self.absolute_pointer, "ABSOLUTE_POINTER");
        push(self.physical_hid_keys, "PHYSICAL_HID_KEYS");
        push(self.text_clipboard, "TEXT_CLIPBOARD");
        push(self.native_repeat, "NATIVE_REPEAT");
        push(self.software_repeat, "SOFTWARE_REPEAT");
        push(self.receiver_takeover, "RECEIVER_TAKEOVER");
        push(self.displays_changed, "DISPLAYS_CHANGED");
        push(self.emergency_hotkey, "EMERGENCY_HOTKEY");
        out
    }

    /// The capability names that are *not* available, for `Status.capabilities_unavailable`.
    pub fn unavailable(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut push = |off: bool, name: &'static str| {
            if off {
                out.push(name);
            }
        };
        push(!self.absolute_pointer, "ABSOLUTE_POINTER");
        push(!self.text_clipboard, "TEXT_CLIPBOARD");
        push(!self.receiver_takeover, "RECEIVER_TAKEOVER");
        push(!self.emergency_hotkey, "EMERGENCY_HOTKEY");
        out
    }
}

/// `(interface, version)` pair for `BackendInfo.portals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalInterface {
    /// D-Bus interface name.
    pub name: String,
    /// Interface version reported by the portal.
    pub version: u32,
}

/// Structured backend detail, shaped like `docs/ipc.proto` `BackendInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    /// `platform-win` | `platform-x11` | `platform-wayland`.
    pub backend: String,
    /// Compositor name (`mutter` | `kwin` | `""` off Wayland).
    pub compositor: String,
    /// Portal interface versions (empty off Wayland).
    pub portals: Vec<PortalInterface>,
    /// `GlobalShortcuts` is available (see [`Capabilities::emergency_hotkey`]).
    pub has_emergency_hotkey: bool,
    /// The portal accepts restore tokens (`persist_mode = 2`).
    pub has_portal_persistence: bool,
    /// Local physical input can end a lease.
    pub has_receiver_takeover: bool,
    /// An absolute EIS pointer region can be established. Provisional until
    /// KVM-016 confirms `reis` + fractionally scaled regions.
    pub has_absolute_injection: bool,
    /// Sender-side input capture is available.
    pub has_capture: bool,
    /// Receiver-side injection is available.
    pub has_inject: bool,
}

/// The result of a successful decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Detected session kind.
    pub session_kind: SessionKind,
    /// Chosen backend.
    pub backend: Backend,
    /// Detected compositor (Wayland only).
    pub compositor: Option<Compositor>,
    /// Portal interface versions (Wayland only).
    pub portals: PortalVersions,
    /// `DISPLAY` and `WAYLAND_DISPLAY` both set: Xwayland is present.
    pub xwayland: bool,
    /// Derived capabilities.
    pub capabilities: Capabilities,
    /// The portal honours `persist_mode = 2` for restore tokens.
    pub portal_persistence: bool,
}

impl SessionInfo {
    /// Shapes the info like `docs/ipc.proto` `BackendInfo`.
    pub fn to_backend_info(&self) -> BackendInfo {
        BackendInfo {
            backend: self.backend.name().to_owned(),
            compositor: self
                .compositor
                .as_ref()
                .map(Compositor::ipc_name)
                .unwrap_or_default(),
            portals: self
                .portals
                .iter()
                .filter(|(_, version)| *version > 0)
                .map(|(name, version)| PortalInterface {
                    name: name.to_owned(),
                    version,
                })
                .collect(),
            has_emergency_hotkey: self.capabilities.emergency_hotkey,
            has_portal_persistence: self.portal_persistence,
            has_receiver_takeover: self.capabilities.receiver_takeover,
            has_absolute_injection: self.capabilities.absolute_pointer,
            has_capture: self.capabilities.capture,
            has_inject: self.capabilities.inject,
        }
    }

    fn windows() -> Self {
        Self {
            session_kind: SessionKind::Windows,
            backend: Backend::Windows,
            compositor: None,
            portals: PortalVersions::default(),
            xwayland: false,
            capabilities: Capabilities {
                capture: true,
                inject: true,
                absolute_pointer: true,
                physical_hid_keys: true,
                text_clipboard: false,
                // Measured: a key injected with `SendInput` and held without an up produces
                // no further down of its own (lws-agent `repeat` module, KVM-036), so a
                // Windows receiver must repeat it in software.
                native_repeat: false,
                software_repeat: true,
                receiver_takeover: true,
                displays_changed: true,
                emergency_hotkey: true,
            },
            portal_persistence: false,
        }
    }

    fn x11() -> Self {
        Self {
            session_kind: SessionKind::X11,
            backend: Backend::X11,
            compositor: None,
            portals: PortalVersions::default(),
            xwayland: false,
            capabilities: Capabilities {
                capture: true,
                inject: true,
                absolute_pointer: true,
                physical_hid_keys: true,
                text_clipboard: false,
                // Measured: a key injected with `SendInput` and held without an up produces
                // no further down of its own (lws-agent `repeat` module, KVM-036), so a
                // Windows receiver must repeat it in software.
                native_repeat: false,
                software_repeat: true,
                receiver_takeover: true,
                displays_changed: true,
                // The emergency combo is handled inside the X11 grab (KVM-013).
                emergency_hotkey: true,
            },
            portal_persistence: false,
        }
    }

    fn wayland(compositor: Compositor, portals: PortalVersions, xwayland: bool) -> Self {
        let global_shortcuts = portals.global_shortcuts >= 1;
        let remote_desktop_v2 = portals.remote_desktop >= MIN_REMOTE_DESKTOP_VERSION;
        let input_capture_v2 = portals.input_capture >= 2;
        Self {
            session_kind: SessionKind::Wayland,
            backend: Backend::Wayland {
                compositor: compositor.clone(),
            },
            compositor: Some(compositor),
            portals,
            xwayland,
            capabilities: Capabilities {
                capture: true,
                inject: true,
                // Provisional: the version gate is real, the region is the KVM-016 gate.
                absolute_pointer: remote_desktop_v2,
                physical_hid_keys: true,
                text_clipboard: false,
                // Whether the compositor repeats EIS-emulated keys is a KVM-016
                // question; neither repeat capability is claimed until it is answered.
                native_repeat: false,
                software_repeat: false,
                // AC-13a: a portal receiver cannot see physical input.
                receiver_takeover: false,
                displays_changed: true,
                emergency_hotkey: global_shortcuts,
            },
            portal_persistence: remote_desktop_v2 || input_capture_v2,
        }
    }
}

/// A session this node cannot serve, with the reason (and versions) in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSession {
    /// Human-readable reason, always naming the offending value/version.
    pub reason: String,
}

impl UnsupportedSession {
    /// Builds an unsupported-session error.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for UnsupportedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UnsupportedSession: {}", self.reason)
    }
}

impl std::error::Error for UnsupportedSession {}

/// The decision error type: success or an explicit refusal. There is no third
/// outcome and no silent fallback.
pub type ProbeError = UnsupportedSession;

/// Resolves the compositor from the impl-portal backend token, falling back to the
/// desktop strings.
fn resolve_compositor(snapshot: &EnvSnapshot) -> Option<Compositor> {
    if let Some(token) = snapshot.impl_portal_backend.as_deref() {
        if let Some(compositor) = compositor_from_impl_portal(token) {
            return Some(compositor);
        }
    }
    for desktop in [
        snapshot.xdg_current_desktop.as_deref(),
        snapshot.xdg_session_desktop.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(compositor) = compositor_from_desktop_string(desktop) {
            return Some(compositor);
        }
    }
    None
}

/// Maps an `org.freedesktop.impl.portal.desktop.<token>` owner to a compositor.
pub fn compositor_from_impl_portal(token: &str) -> Option<Compositor> {
    match token.trim().to_ascii_lowercase().as_str() {
        "gnome" => Some(Compositor::Gnome),
        "kde" => Some(Compositor::Kde),
        "wlr" => Some(Compositor::Wlroots("wlroots".to_owned())),
        "hyprland" => Some(Compositor::Wlroots("Hyprland".to_owned())),
        "sway" => Some(Compositor::Wlroots("Sway".to_owned())),
        "lxqt" => Some(Compositor::Other("LXQt".to_owned())),
        "xfce" => Some(Compositor::Other("Xfce".to_owned())),
        "cosmic" => Some(Compositor::Other("COSMIC".to_owned())),
        _ => None,
    }
}

/// Maps `XDG_CURRENT_DESKTOP` / `XDG_SESSION_DESKTOP` (`ubuntu:GNOME`, `KDE`) to a
/// compositor, or `None` when the value names nothing we recognise.
pub fn compositor_from_desktop_string(value: &str) -> Option<Compositor> {
    for token in value.split(':').map(str::trim).filter(|t| !t.is_empty()) {
        let compositor = match token.to_ascii_lowercase().as_str() {
            "gnome" | "ubuntu" => Some(Compositor::Gnome),
            "kde" | "plasma" => Some(Compositor::Kde),
            "sway" => Some(Compositor::Wlroots("Sway".to_owned())),
            "hyprland" => Some(Compositor::Wlroots("Hyprland".to_owned())),
            "wayfire" => Some(Compositor::Wlroots("Wayfire".to_owned())),
            "river" => Some(Compositor::Wlroots("river".to_owned())),
            "wlroots" => Some(Compositor::Wlroots("wlroots".to_owned())),
            "xfce" => Some(Compositor::Other("Xfce".to_owned())),
            "lxqt" => Some(Compositor::Other("LXQt".to_owned())),
            "cosmic" => Some(Compositor::Other("COSMIC".to_owned())),
            "mate" => Some(Compositor::Other("MATE".to_owned())),
            "cinnamon" => Some(Compositor::Other("Cinnamon".to_owned())),
            _ => None,
        };
        if let Some(compositor) = compositor {
            // `ubuntu:` alone is not evidence of GNOME; keep scanning for a stronger
            // token, but remember a weak match.
            if !matches!(token.to_ascii_lowercase().as_str(), "ubuntu") {
                return Some(compositor);
            }
        }
    }
    // `ubuntu:GNOME` handled above; a bare `ubuntu` still means the GNOME desktop.
    if value
        .split(':')
        .any(|t| t.trim().eq_ignore_ascii_case("ubuntu"))
    {
        return Some(Compositor::Gnome);
    }
    None
}

/// What the session type variables say, before validation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detected {
    X11,
    Wayland,
    Tty,
    OtherType(String),
}

fn classify(snapshot: &EnvSnapshot) -> Detected {
    let session_type = snapshot
        .xdg_session_type
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase());
    match session_type.as_deref() {
        Some("wayland") => Detected::Wayland,
        Some("x11") => Detected::X11,
        Some("tty") | None => {
            if snapshot.wayland_display.is_some() {
                Detected::Wayland
            } else if snapshot.display.is_some() {
                Detected::X11
            } else {
                Detected::Tty
            }
        }
        Some(other) => Detected::OtherType(other.to_owned()),
    }
}

/// The pure session/backend decision.
///
/// `snapshot` is everything the probe could observe; `requested` is the config
/// override. A successful result names exactly one backend; every other outcome is
/// an [`UnsupportedSession`] whose message contains the offending version or value.
pub fn decide(
    snapshot: &EnvSnapshot,
    requested: BackendOverride,
) -> Result<SessionInfo, UnsupportedSession> {
    if snapshot.host == HostOs::Windows {
        return match requested {
            BackendOverride::Auto => Ok(SessionInfo::windows()),
            other => Err(UnsupportedSession::new(format!(
                "[platform].backend = {other} is not a Linux backend, and this host is Windows"
            ))),
        };
    }

    let detected = classify(snapshot);
    let xwayland = snapshot.has_xwayland();

    match detected {
        Detected::Tty => Err(UnsupportedSession::new(
            "no graphical session: neither WAYLAND_DISPLAY nor DISPLAY is set and \
             XDG_SESSION_TYPE is not wayland/x11 (a bare TTY has no backend)",
        )),
        Detected::OtherType(other) => Err(UnsupportedSession::new(format!(
            "XDG_SESSION_TYPE = {other:?} is not one of \"wayland\", \"x11\", \"tty\""
        ))),
        Detected::X11 => {
            // AC-12: an Xwayland session is never served by `platform-x11`.
            // `XDG_SESSION_TYPE` claiming X11 while a Wayland socket is present is
            // exactly that mixture, and it fails closed rather than guessing.
            if let Some(wayland_display) = snapshot.wayland_display.as_deref() {
                return Err(UnsupportedSession::new(format!(
                    "XDG_SESSION_TYPE=x11 but WAYLAND_DISPLAY={wayland_display} is set: this \
                     session mixes Wayland with an X server, i.e. Xwayland; \
                     UnsupportedSession, and platform-x11 is never used for it (AC-12)"
                )));
            }
            if requested == BackendOverride::Wayland {
                return Err(UnsupportedSession::new(
                    "[platform].backend = wayland but the session is X11 \
                     (XDG_SESSION_TYPE=x11); refusing instead of guessing",
                ));
            }
            let extensions = snapshot.x11.ok_or_else(|| {
                UnsupportedSession::new(
                    "X11 session, but the X server extensions were not probed \
                     (could not open DISPLAY); XTEST, XInput2 and RANDR are required",
                )
            })?;
            if !extensions.all_present() {
                return Err(UnsupportedSession::new(format!(
                    "X11 session is missing required extensions: {} \
                     (XTEST, XInput2 and RANDR are all required)",
                    extensions.missing().join(", ")
                )));
            }
            Ok(SessionInfo::x11())
        }
        Detected::Wayland => {
            let wayland_display = snapshot.wayland_display.as_deref().ok_or_else(|| {
                UnsupportedSession::new(
                    "XDG_SESSION_TYPE=wayland but WAYLAND_DISPLAY is unset; \
                     the Wayland backend needs it",
                )
            })?;
            if requested == BackendOverride::X11 {
                let detail = if xwayland {
                    format!(
                        " (Xwayland is present: DISPLAY={}, WAYLAND_DISPLAY={wayland_display})",
                        snapshot.display.as_deref().unwrap_or("")
                    )
                } else {
                    String::new()
                };
                return Err(UnsupportedSession::new(format!(
                    "[platform].backend = x11 but the session is Wayland{detail}; \
                     Xwayland is not an X11 backend and is never used as a fallback"
                )));
            }
            if !snapshot.portals_queried {
                return Err(UnsupportedSession::new(
                    "session bus unreachable: cannot read the \
                     org.freedesktop.portal.{InputCapture,RemoteDesktop} versions from \
                     org.freedesktop.portal.Desktop",
                ));
            }
            let compositor = resolve_compositor(snapshot).ok_or_else(|| {
                UnsupportedSession::new(format!(
                    "unknown compositor: XDG_CURRENT_DESKTOP={:?}, XDG_SESSION_DESKTOP={:?}, \
                     no org.freedesktop.impl.portal.desktop.* owner on the session bus",
                    snapshot.xdg_current_desktop.as_deref().unwrap_or(""),
                    snapshot.xdg_session_desktop.as_deref().unwrap_or(""),
                ))
            })?;
            match &compositor {
                Compositor::Wlroots(name) => {
                    return Err(UnsupportedSession::new(format!(
                        "compositor {name} is level 2 (after MVP): no \
                         org.freedesktop.portal.InputCapture backend exists for wlroots \
                         (XDP-wlr); layer-shell/pointer-constraints are out of scope (ADR-002)"
                    )));
                }
                Compositor::Other(name) => {
                    return Err(UnsupportedSession::new(format!(
                        "compositor {name} is not level 1: needs \
                         xdg-desktop-portal-gnome >= 45 (GNOME >= 45) or \
                         xdg-desktop-portal-kde with Plasma >= 6.1"
                    )));
                }
                Compositor::Gnome | Compositor::Kde => {}
            }

            let portals = snapshot.portals;
            if portals.input_capture < MIN_INPUT_CAPTURE_VERSION {
                return Err(UnsupportedSession::new(format!(
                    "{} version {} is too old: need >= {MIN_INPUT_CAPTURE_VERSION} \
                     (xdg-desktop-portal >= {MIN_XDP_VERSION})",
                    PortalVersions::INPUT_CAPTURE,
                    portals.input_capture,
                )));
            }
            if portals.remote_desktop < MIN_REMOTE_DESKTOP_VERSION {
                return Err(UnsupportedSession::new(format!(
                    "{} version {} is too old: need >= {MIN_REMOTE_DESKTOP_VERSION} for \
                     ConnectToEIS (xdg-desktop-portal >= {MIN_XDP_VERSION})",
                    PortalVersions::REMOTE_DESKTOP,
                    portals.remote_desktop,
                )));
            }
            Ok(SessionInfo::wayland(compositor, portals, xwayland))
        }
    }
}
