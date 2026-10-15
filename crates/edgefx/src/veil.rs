//! The cursor-hide lifecycle, pure.
//!
//! The Windows veil is a small topmost window under the held cursor (see
//! `platform_win::veil`); this is the decision of *when it exists*, against a [`VeilHost`]
//! the tests fake. One invariant carries everything: **the veil is shown only while the
//! `probe` says the capture is Remote**. Every entry point takes a fresh probe; a probe of
//! `None` (Local mode, emergency, expiry, a dropped capture) hides it, so no release path
//! has a veil to forget.

pub type Point = (i32, i32);

/// The window operations. A host may be a real window or a recording fake.
pub trait VeilHost {
    /// Show (or move) the window at the hold point, topmost, without activating.
    fn show(&mut self, at: Point);
    /// Re-assert `HWND_TOPMOST`: another topmost window (the taskbar) may have been raised
    /// over the veil since, and the cursor is drawn by whichever window is under it.
    fn raise(&mut self);
    fn hide(&mut self);
    /// The 100 ms liveness timer: running only while the veil is shown, so idle costs nothing.
    fn timer(&mut self, on: bool);
}

#[derive(Debug, Default)]
pub struct Veil {
    enabled: bool,
    at: Option<Point>,
}

impl Veil {
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled, at: None }
    }

    #[must_use]
    pub const fn is_shown(&self) -> bool {
        self.at.is_some()
    }

    /// The setting changed (live `SetEffects`): off hides at once.
    pub fn set_enabled(&mut self, enabled: bool, probe: Option<Point>, host: &mut impl VeilHost) {
        self.enabled = enabled;
        self.apply(probe, host);
    }

    /// The backend entered Remote.
    pub fn engage(&mut self, probe: Option<Point>, host: &mut impl VeilHost) {
        self.apply(probe, host);
    }

    /// The liveness timer fired.
    pub fn tick(&mut self, probe: Option<Point>, host: &mut impl VeilHost) {
        self.apply(probe, host);
        if self.is_shown() {
            host.raise();
        }
    }

    /// Explicit end: the backend left Remote, the thread is shutting down or unwinding.
    pub fn release(&mut self, host: &mut impl VeilHost) {
        self.apply(None, host);
    }

    fn apply(&mut self, probe: Option<Point>, host: &mut impl VeilHost) {
        match probe.filter(|_| self.enabled) {
            Some(point) => {
                if self.at != Some(point) {
                    host.show(point);
                }
                if self.at.is_none() {
                    host.timer(true);
                }
                self.at = Some(point);
            }
            None => {
                if self.at.take().is_some() {
                    host.hide();
                    host.timer(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        visible: bool,
        timer: bool,
        shows: u32,
        raises: u32,
        hides: u32,
        log: Vec<&'static str>,
    }

    impl VeilHost for Fake {
        fn show(&mut self, _: Point) {
            self.visible = true;
            self.shows += 1;
            self.log.push("show");
        }
        fn raise(&mut self) {
            self.raises += 1;
        }
        fn hide(&mut self) {
            self.visible = false;
            self.hides += 1;
            self.log.push("hide");
        }
        fn timer(&mut self, on: bool) {
            self.timer = on;
        }
    }

    const HOLD: Option<Point> = Some((1919, 540));

    #[test]
    fn it_is_never_shown_while_the_probe_says_local() {
        let mut veil = Veil::new(true);
        let mut host = Fake::default();
        veil.engage(None, &mut host);
        veil.tick(None, &mut host);
        assert!(!host.visible && !host.timer && host.shows == 0 && !veil.is_shown());
    }

    #[test]
    fn remote_shows_once_and_runs_the_timer_only_while_shown() {
        let mut veil = Veil::new(true);
        let mut host = Fake::default();
        veil.engage(HOLD, &mut host);
        veil.engage(HOLD, &mut host);
        assert!(host.visible && host.timer);
        assert_eq!(host.shows, 1, "one window, shown once");
        veil.tick(None, &mut host);
        assert!(!host.visible && !host.timer);
    }

    #[test]
    fn every_release_path_hides_it() {
        // Emergency / expiry / stop / dropped capture all reach the veil as a probe of None.
        for path in ["emergency", "expiry", "capture dropped"] {
            let mut veil = Veil::new(true);
            let mut host = Fake::default();
            veil.engage(HOLD, &mut host);
            veil.tick(None, &mut host);
            assert!(!host.visible, "{path}");
        }
        // Explicit end and thread shutdown/unwind.
        let mut veil = Veil::new(true);
        let mut host = Fake::default();
        veil.engage(HOLD, &mut host);
        veil.release(&mut host);
        assert!(!host.visible && !host.timer);
        // Switching the setting off while shown.
        veil.engage(HOLD, &mut host);
        veil.set_enabled(false, HOLD, &mut host);
        assert!(!host.visible);
        // And a disabled veil never shows.
        veil.engage(HOLD, &mut host);
        assert!(!host.visible);
    }

    #[test]
    fn every_tick_reasserts_topmost_and_a_moved_hold_point_moves_it() {
        let mut veil = Veil::new(true);
        let mut host = Fake::default();
        veil.engage(HOLD, &mut host);
        veil.tick(HOLD, &mut host);
        veil.tick(HOLD, &mut host);
        assert_eq!(host.raises, 2);
        assert_eq!(host.shows, 1);
        veil.tick(Some((0, 0)), &mut host);
        assert_eq!(host.shows, 2);
        assert!(host.visible);
    }

    #[test]
    fn ping_pong_reuses_the_single_window() {
        let mut veil = Veil::new(true);
        let mut host = Fake::default();
        for _ in 0..10 {
            veil.engage(HOLD, &mut host);
            veil.tick(None, &mut host);
        }
        assert_eq!(host.shows, 10);
        assert_eq!(host.hides, 10);
        assert!(!host.visible);
    }
}
