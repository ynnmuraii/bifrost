//! Monitor enumeration for `DisplaysReport` (KVM-040), through RandR 1.5 `GetMonitors`.
//!
//! `docs/03-architecture.md`, «Координаты»: «Logical unit: … на X11 — физический
//! пиксель root window, делённый на `scale` из конфигурации (по умолчанию 1.0, так как
//! X11 не задаёт DPI-масштаб на монитор)», and the node logical space starts at the
//! primary display's top-left. This build has no per-display scale setting, so the
//! documented default applies and logical bounds equal the RandR rectangles — which are
//! already rotated by the server.
//!
//! RandR 1.5 is required (`GetMonitors`); a server that only offers an older version is
//! reported as unsupported instead of being guessed at from CRTCs, because a guessed
//! monitor layout would silently mis-place every transition.
//!
//! Not implemented here: per-monitor rotation (`randr_get_crtc_info`'s transform) — the
//! server already rotates the logical desktop, and `geometry` never reads
//! `Display.rotation` for validation.
#![cfg(target_os = "linux")]

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, MonitorInfo};
use x11rb::protocol::xproto;
use x11rb::rust_connection::RustConnection;

/// One monitor of the root window.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The RandR monitor name (often the connector name, e.g. `eDP-1`).
    pub id: String,
    /// Logical bounds: `[x, y, width, height]`.
    pub logical: [f64; 4],
    /// Native bounds in root-window pixels: `[x, y, width, height]`.
    pub native: [i32; 4],
    /// Logical → native scale. `1.0` here, as documented for X11.
    pub scale: f64,
    /// Whether RandR marks this monitor as primary (the node logical origin).
    pub primary: bool,
    pub physical_size: Option<[f64; 2]>,
    pub physical_size_source: String,
}

/// Monitor enumeration failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaysError(pub String);

impl std::fmt::Display for DisplaysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cannot enumerate monitors: {}", self.0)
    }
}

impl std::error::Error for DisplaysError {}

/// Every RandR monitor, primary first.
pub fn enumerate() -> Result<Vec<Monitor>, DisplaysError> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|error| DisplaysError(format!("cannot open the X11 connection: {error}")))?;
    let root = conn.setup().roots[screen_num].root;

    let version = randr::query_version(&conn, 1, 5)
        .map_err(|error| DisplaysError(format!("RandR query failed: {error}")))?
        .reply()
        .map_err(|error| DisplaysError(format!("RandR query failed: {error}")))?;
    if (version.major_version, version.minor_version) < (1, 5) {
        return Err(DisplaysError(format!(
            "RandR 1.5 is required for GetMonitors, the server offers {}.{}",
            version.major_version, version.minor_version
        )));
    }

    let reply = randr::get_monitors(&conn, root, true)
        .map_err(|error| DisplaysError(format!("GetMonitors failed: {error}")))?
        .reply()
        .map_err(|error| DisplaysError(format!("GetMonitors failed: {error}")))?;

    let mut named = Vec::with_capacity(reply.monitors.len());
    for monitor in reply.monitors {
        let name = xproto::get_atom_name(&conn, monitor.name)
            .map_err(|error| DisplaysError(format!("GetAtomName failed: {error}")))?
            .reply()
            .map_err(|error| DisplaysError(format!("GetAtomName failed: {error}")))?;
        named.push((String::from_utf8_lossy(&name.name).into_owned(), monitor));
    }
    Ok(assemble(named))
}

/// Turns resolved RandR monitors into the reported form: primary first, ids unique, a
/// monitor with no area dropped.
fn assemble(named: Vec<(String, MonitorInfo)>) -> Vec<Monitor> {
    let mut monitors: Vec<Monitor> = named
        .into_iter()
        .filter(|(_, monitor)| monitor.width > 0 && monitor.height > 0)
        .map(|(name, monitor)| {
            let native = [
                i32::from(monitor.x),
                i32::from(monitor.y),
                i32::from(monitor.width),
                i32::from(monitor.height),
            ];
            // A user-defined monitor spanning outputs is not one physical panel.
            let physical_size = (monitor.outputs.len() == 1
                && monitor.width_in_millimeters > 0
                && monitor.height_in_millimeters > 0)
                .then_some([
                    f64::from(monitor.width_in_millimeters),
                    f64::from(monitor.height_in_millimeters),
                ]);
            Monitor {
                id: if name.is_empty() {
                    // RandR allows an unnamed monitor; the geometry needs an id, and
                    // the rectangle is what makes it unique.
                    format!("x11-{}-{}", monitor.x, monitor.y)
                } else {
                    name
                },
                logical: [
                    f64::from(native[0]),
                    f64::from(native[1]),
                    f64::from(native[2]),
                    f64::from(native[3]),
                ],
                native,
                scale: 1.0,
                primary: monitor.primary,
                physical_size,
                physical_size_source: if physical_size.is_some() {
                    "randr".to_owned()
                } else {
                    String::new()
                },
            }
        })
        .collect();
    // Primary first, then a stable order so two enumerations compare equal.
    monitors.sort_by(|left, right| {
        right
            .primary
            .cmp(&left.primary)
            .then_with(|| left.id.cmp(&right.id))
    });
    monitors
}

/// Whether a usable connection exists (the caller wants a reason, not a panic).
pub fn available() -> bool {
    RustConnection::connect(None).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: u32, primary: bool, x: i16, y: i16, width: u16, height: u16) -> MonitorInfo {
        MonitorInfo {
            name,
            primary,
            automatic: false,
            x,
            y,
            width,
            height,
            width_in_millimeters: 0,
            height_in_millimeters: 0,
            outputs: Vec::new(),
        }
    }

    #[test]
    fn randr_physical_size_requires_a_single_panel_and_both_axes() {
        let mut panel = monitor(1, true, 0, 0, 1080, 1920);
        panel.outputs = vec![7];
        panel.width_in_millimeters = 300;
        panel.height_in_millimeters = 530;
        let displays = assemble(vec![("DP-1".into(), panel.clone())]);
        assert_eq!(displays[0].physical_size, Some([300.0, 530.0]));
        assert_eq!(displays[0].physical_size_source, "randr");
        panel.outputs.push(8);
        assert!(assemble(vec![("aggregate".into(), panel.clone())])[0]
            .physical_size
            .is_none());
        panel.outputs = vec![7];
        panel.width_in_millimeters = 0;
        assert!(assemble(vec![("unknown".into(), panel)])[0]
            .physical_size
            .is_none());
    }

    #[test]
    fn the_primary_monitor_comes_first_and_ids_stay_distinct() {
        let assembled = assemble(vec![
            ("DP-1".to_owned(), monitor(1, false, 1920, 0, 2560, 1440)),
            ("eDP-1".to_owned(), monitor(2, true, 0, 0, 1920, 1080)),
        ]);
        assert_eq!(assembled.len(), 2);
        assert_eq!(assembled[0].id, "eDP-1");
        assert!(assembled[0].primary);
        assert_eq!(assembled[0].logical, [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(assembled[0].native, [0, 0, 1920, 1080]);
        assert_eq!(assembled[0].scale, 1.0);
        assert_eq!(assembled[1].id, "DP-1");
        assert_eq!(assembled[1].logical, [1920.0, 0.0, 2560.0, 1440.0]);

        // The same input twice gives the same order, which is what makes
        // `DisplaysReport.local_revision` meaningful.
        let again = assemble(vec![
            ("DP-1".to_owned(), monitor(1, false, 1920, 0, 2560, 1440)),
            ("eDP-1".to_owned(), monitor(2, true, 0, 0, 1920, 1080)),
        ]);
        assert_eq!(assembled, again);
    }

    #[test]
    fn negative_origins_and_zero_area_monitors() {
        let assembled = assemble(vec![
            (
                "left".to_owned(),
                monitor(1, false, -1920, -200, 1920, 1080),
            ),
            ("ghost".to_owned(), monitor(2, false, 0, 0, 0, 0)),
        ]);
        assert_eq!(
            assembled.len(),
            1,
            "a monitor with no area is not a display"
        );
        assert_eq!(assembled[0].logical, [-1920.0, -200.0, 1920.0, 1080.0]);

        // An unnamed monitor still gets a usable id.
        let unnamed = assemble(vec![("".to_owned(), monitor(0, true, 0, 0, 800, 600))]);
        assert_eq!(unnamed.len(), 1);
        assert!(!unnamed[0].id.is_empty());
        assert!(unnamed[0].primary);
    }
}
