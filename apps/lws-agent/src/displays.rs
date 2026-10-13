//! Displays as `DisplaysReport` carries them (KVM-040).
//!
//! `docs/03-architecture.md`, «Координаты»: `Display.logical_bounds` are in the node's
//! own logical space with the origin at the primary display's top-left, coordinates may
//! be negative, and `native_bounds` is diagnostic — «`native_known = false` означает,
//! что физический размер недоступен (типично для Wayland через порталы)». The IPC
//! message has no `native_known` flag, so the convention this module implements is
//! documented here: **a non-positive native width or height means "not known"**, which
//! is the only reading under which a zero-sized monitor cannot be mistaken for a real
//! one. Every backend fills real values except the portal one, which reports zeros.
//!
//! `rotation` is reported as `0` for now: Windows and X11 report a *logical* space the
//! OS has already rotated, the portal zones are already rotated, and `geometry` never
//! reads `Display.rotation` for validation. Reading it per display (Windows `DEVMODEW`,
//! RandR output transforms) is a separate piece of work.
//!
//! The module is platform-neutral and pure, so its conversion is unit-tested on every
//! host and each backend only has to produce [`RawDisplay`] values.

use protocol::ipc;

/// One display as a backend sees it, before it becomes a wire message.
#[derive(Debug, Clone, PartialEq)]
pub struct RawDisplay {
    /// Stable id inside this node.
    pub id: String,
    /// The OS device name this display answers at (`\\.\DISPLAY1` on Windows), when the
    /// backend has one; empty otherwise. Carried so core can map a layout written under the
    /// old name after Windows renumbers its displays.
    pub name: String,
    /// Logical bounds `[x, y, width, height]` in the node's logical space.
    pub logical: [f64; 4],
    /// Native bounds in device pixels, or `None` when the platform does not expose
    /// them (portals).
    pub native: Option<[i32; 4]>,
    /// Logical → native scale; must be finite and positive.
    pub scale: f64,
    /// Whether this display hosts the node logical origin.
    pub primary: bool,
}

impl RawDisplay {
    /// A display with no physical bounds (the portal case): the Wayland backend is the
    /// only production caller, so it is dead code on the other platforms.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn without_native(id: impl Into<String>, logical: [f64; 4], primary: bool) -> Self {
        RawDisplay {
            id: id.into(),
            name: String::new(),
            logical,
            native: None,
            scale: 1.0,
            primary,
        }
    }

    /// The wire message for this display.
    pub fn to_info(&self) -> ipc::DisplayInfo {
        let native = self.native.unwrap_or([0, 0, 0, 0]);
        ipc::DisplayInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            lx: self.logical[0],
            ly: self.logical[1],
            lw: self.logical[2],
            lh: self.logical[3],
            nx: native[0],
            ny: native[1],
            nw: native[2],
            nh: native[3],
            rotation: 0,
            scale: self.scale,
            primary: self.primary,
        }
    }
}

/// The report for a set of displays, keeping the given revision.
///
/// The set is sent in the order the backend produced (primary first) and is not
/// re-sorted here: the backend knows its own layout best, and the order is what
/// `local_revision` compares against.
pub fn report(displays: &[RawDisplay], local_revision: u64) -> ipc::DisplaysReport {
    ipc::DisplaysReport {
        displays: displays.iter().map(RawDisplay::to_info).collect(),
        local_revision,
    }
}

/// The agent's own display counter: `Hello.displays_revision` is «локальный счётчик
/// набора дисплеев узла», and core copies it from `DisplaysReport.local_revision`.
///
/// The revision grows only when the *set* changes, so a `QueryDisplays` that finds the
/// same monitors twice does not look like a hot-plug.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DisplaysState {
    last: Vec<RawDisplay>,
    revision: u64,
}

impl DisplaysState {
    /// The report for `current`, bumping the revision if the set changed.
    pub fn observe(&mut self, current: &[RawDisplay]) -> ipc::DisplaysReport {
        if self.revision == 0 || self.last != current {
            self.last = current.to_vec();
            self.revision += 1;
        }
        report(current, self.revision)
    }

    /// The revision last reported. The tests are the only caller: the session uses the
    /// revision `observe` returns.
    #[cfg(test)]
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_displays() -> Vec<RawDisplay> {
        vec![
            RawDisplay {
                id: "primary".to_owned(),
                name: "\\\\.\\DISPLAY1".to_owned(),
                logical: [0.0, 0.0, 1920.0, 1080.0],
                native: Some([0, 0, 1920, 1080]),
                scale: 1.0,
                primary: true,
            },
            RawDisplay {
                id: "left".to_owned(),
                name: String::new(),
                logical: [-1280.0, 0.0, 1280.0, 1024.0],
                native: Some([-1280, 0, 1280, 1024]),
                scale: 1.25,
                primary: false,
            },
        ]
    }

    #[test]
    fn logical_native_scale_and_primary_reach_the_wire() {
        let report = report(&two_displays(), 7);
        assert_eq!(report.local_revision, 7);
        assert_eq!(report.displays.len(), 2);

        let primary = &report.displays[0];
        assert_eq!(primary.id, "primary");
        assert_eq!(
            primary.name, "\\\\.\\DISPLAY1",
            "the device name travels beside the stable id for the alias"
        );
        assert_eq!(
            (primary.lx, primary.ly, primary.lw, primary.lh),
            (0.0, 0.0, 1920.0, 1080.0)
        );
        assert_eq!(
            (primary.nx, primary.ny, primary.nw, primary.nh),
            (0, 0, 1920, 1080)
        );
        assert!(primary.primary);
        assert_eq!(primary.rotation, 0, "rotation is not read yet");

        // Negative origins survive, and the scale is the backend's.
        let left = &report.displays[1];
        assert_eq!(
            (left.lx, left.ly, left.lw, left.lh),
            (-1280.0, 0.0, 1280.0, 1024.0)
        );
        assert_eq!((left.nx, left.nw), (-1280, 1280));
        assert_eq!(left.scale, 1.25);
        assert!(!left.primary);
    }

    #[test]
    fn an_unknown_native_size_is_reported_as_zeros() {
        // The documented convention: a non-positive native size means "not known", so a
        // portal display must never look like a zero-sized monitor.
        let report = report(
            &[RawDisplay::without_native(
                "zone-0",
                [0.0, 0.0, 800.0, 600.0],
                true,
            )],
            1,
        );
        let display = &report.displays[0];
        assert_eq!((display.nw, display.nh), (0, 0));
        assert_eq!(
            (display.lw, display.lh),
            (800.0, 600.0),
            "logical bounds are known"
        );
        assert_eq!(display.scale, 1.0);
    }

    #[test]
    fn the_revision_grows_once_per_change_and_not_per_query() {
        let mut state = DisplaysState::default();
        assert_eq!(state.revision(), 0);

        let first = state.observe(&two_displays());
        assert_eq!(first.local_revision, 1, "the first report is revision 1");

        // The same set again: same revision, so core does not see a hot-plug.
        let same = state.observe(&two_displays());
        assert_eq!(same.local_revision, 1);
        assert_eq!(same, first);

        // A monitor appears: the revision moves.
        let mut changed = two_displays();
        changed.push(RawDisplay::without_native(
            "zone-0",
            [0.0, 0.0, 800.0, 600.0],
            false,
        ));
        assert_eq!(state.observe(&changed).local_revision, 2);
        assert_eq!(state.revision(), 2);

        // Even an empty set is a state: losing every display is a change.
        assert_eq!(state.observe(&[]).local_revision, 3);
        assert_eq!(state.observe(&[]).local_revision, 3);
    }

    #[test]
    fn an_empty_set_is_a_valid_report() {
        // A node with no displays (dry mode before anything reported) is not an error:
        // core decides what to do with an empty set.
        let report = report(&[], 0);
        assert!(report.displays.is_empty());
        assert_eq!(report.local_revision, 0);
    }
}
