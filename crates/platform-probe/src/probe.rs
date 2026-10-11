//! Linux I/O that turns the real machine into an [`EnvSnapshot`] (KVM-014/025).
//!
//! The only side effects in this crate live here: reading environment variables,
//! asking the session bus for the `version` property of each
//! `org.freedesktop.portal.*` interface and for the owner of each
//! `org.freedesktop.impl.portal.desktop.*` name, and opening `DISPLAY` to check for
//! XTEST/XInput2/RANDR. The decision itself stays in [`crate::decide`].

use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput;
use x11rb::protocol::xproto::ConnectionExt as _;
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, Proxy};
use zbus::names::BusName;

use crate::decision::{decide, BackendOverride, SessionInfo, UnsupportedSession};
use crate::env::{EnvSnapshot, HostOs, PortalVersions, X11Extensions};

/// Portal object path on the session bus.
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
/// Portal service name.
const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";

/// Implementation-portal bus names, in the order we probe them, mapped to the
/// backend token recorded in the snapshot.
const IMPL_PORTALS: [(&str, &str); 7] = [
    ("org.freedesktop.impl.portal.desktop.gnome", "gnome"),
    ("org.freedesktop.impl.portal.desktop.kde", "kde"),
    ("org.freedesktop.impl.portal.desktop.wlr", "wlr"),
    ("org.freedesktop.impl.portal.desktop.hyprland", "hyprland"),
    ("org.freedesktop.impl.portal.desktop.lxqt", "lxqt"),
    ("org.freedesktop.impl.portal.desktop.xfce", "xfce"),
    ("org.freedesktop.impl.portal.desktop.cosmic", "cosmic"),
];

/// Reads the environment variables of this process into a snapshot.
///
/// Portal versions and X11 extensions stay unset; call [`read_snapshot`] for the
/// complete picture, or chain [`EnvSnapshot::with_portals`] / `with_x11` yourself
/// from a test.
pub fn read_env_snapshot() -> EnvSnapshot {
    EnvSnapshot::from_vars(HostOs::Linux, std::env::vars())
}

/// Builds a complete snapshot: environment, X11 extensions and portal versions.
pub fn read_snapshot() -> EnvSnapshot {
    let mut snapshot = read_env_snapshot();
    if snapshot.display.is_some() {
        if let Some(extensions) = read_x11_extensions() {
            snapshot = snapshot.with_x11(extensions);
        }
    }
    if let Ok((portals, impl_backend)) = read_portals() {
        snapshot = snapshot.with_portals(portals, impl_backend);
    }
    snapshot
}

/// The real probe: gather, then decide.
pub fn probe(requested: BackendOverride) -> Result<SessionInfo, UnsupportedSession> {
    decide(&read_snapshot(), requested)
}

/// Opens `DISPLAY` and reports XTEST/XInput2/RANDR.
///
/// `XInput2` requires major version `>= 2`; `RANDR` is queried at `>= 1.2`.
/// Returns `None` when no X server is reachable.
pub fn read_x11_extensions() -> Option<X11Extensions> {
    let (connection, _screen) = x11rb::connect(None).ok()?;

    let has = |name: &[u8]| {
        connection
            .query_extension(name)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.present)
            .unwrap_or(false)
    };

    let xtest = has(b"XTEST");
    let randr = has(b"RANDR")
        && connection
            .randr_query_version(1, 2)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some();
    let xi2 = has(b"XInputExtension")
        && xinput::xi_query_version(&connection, 2, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.major_version >= 2)
            .unwrap_or(false);

    let _ = connection.flush();
    Some(X11Extensions { xtest, xi2, randr })
}

/// Reads the `version` property of every portal interface we depend on, plus the
/// backend token of the implementation portal that currently has an owner.
///
/// An interface that is not implemented by the running portal makes the property
/// read fail, which is recorded as version `0` — the same value a portal older
/// than the feature would report.
pub fn read_portals() -> zbus::Result<(PortalVersions, Option<String>)> {
    let connection = Connection::session()?;

    let version = |interface: &str| -> u32 {
        Proxy::new(&connection, PORTAL_SERVICE, PORTAL_PATH, interface)
            .ok()
            .and_then(|proxy| proxy.get_property::<u32>("version").ok())
            .unwrap_or(0)
    };

    let portals = PortalVersions {
        input_capture: version(PortalVersions::INPUT_CAPTURE),
        remote_desktop: version(PortalVersions::REMOTE_DESKTOP),
        clipboard: version(PortalVersions::CLIPBOARD),
        global_shortcuts: version(PortalVersions::GLOBAL_SHORTCUTS),
    };

    let dbus = DBusProxy::new(&connection)?;
    let mut impl_backend = None;
    for (name, token) in IMPL_PORTALS {
        let Ok(bus_name) = BusName::try_from(name) else {
            continue;
        };
        if dbus.name_has_owner(bus_name).unwrap_or(false) {
            impl_backend = Some(token.to_owned());
            break;
        }
    }

    Ok((portals, impl_backend))
}
