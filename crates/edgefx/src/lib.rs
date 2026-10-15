//! Edge effects: the small ASCII strip that gathers toward the cursor at a linked edge, and
//! the matching "release" on the machine the cursor arrives at.
//!
//! This crate is *pure*: no OS calls, no clock, no allocation per frame. The platform
//! crates own a window (Win32 layered window, wlr-layer-shell surface), feed [`Update`]s
//! into an [`Effect`], call [`Effect::step`] at most [`FRAME_S`] apart while
//! [`Effect::is_active`], and blit what [`Effect::draw`] rasterizes. When the effect is idle
//! the platform loop has nothing to do: no timer, no repaint.
//!
//! Coordinates are *display-local logical pixels* along and across one display edge, the
//! same numbers the core already computes for the edge geometry. [`veil`] is the cursor-hide
//! lifecycle, kept here so every release path can be tested against a fake host.

pub mod coalesce;
pub mod effect;
pub mod raster;
pub mod sim;
pub mod veil;

pub use effect::{Effect, Phase};
pub use raster::{Canvas, ACCENT, DIM};

/// Distance (logical px) from a linked edge at which the strip starts to appear.
pub const REACH_PX: f32 = 160.0;
/// How deep the strip reaches into the display (logical px).
pub const DEPTH_PX: f32 = 36.0;
/// The longest strip the overlay window will cover (logical px).
pub const MAX_STRIP_PX: f32 = 1600.0;
/// Source side: fade-out once input has crossed.
pub const FADE_OUT_S: f32 = 0.30;
/// Receiver side: how long the release plays.
pub const RELEASE_S: f32 = 0.50;
/// An approach with no update for this long fades out (core died, agent kept alone).
pub const STALE_S: f32 = 0.25;
/// Frame cap while an effect is alive.
pub const FRAME_S: f32 = 1.0 / 30.0;

/// Which side of a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// A top or bottom edge: the strip runs horizontally.
    #[must_use]
    pub const fn is_horizontal(self) -> bool {
        matches!(self, Self::Top | Self::Bottom)
    }
}

/// One linked edge segment plus the crossing point, in display-local logical px.
/// `start..end` and `along` are measured along the edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub edge: Edge,
    pub start: f32,
    pub end: f32,
    pub along: f32,
}

/// What the core tells the overlay (the payload of the IPC `EdgeFx` frame).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Update {
    /// The pointer is `distance` px from the crossing point's edge.
    Proximity { target: Target, distance: f32 },
    /// Input left this machine. The target is the place the crossing happened, when known
    /// and no approach was drawn (Wayland source: only the activation is seen).
    Crossed(Option<Target>),
    /// Input arrived here at `target.along`.
    Entered(Target),
    /// Input left this machine again.
    Left,
    /// Stop everything at once (link down, effects switched off).
    Clear,
}

/// The window-sized part of a [`Target`]: what the platform must cover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strip {
    pub edge: Edge,
    /// Where the strip starts along the edge (logical px).
    pub start: f32,
    /// Its length along the edge (logical px), at most [`MAX_STRIP_PX`].
    pub len: f32,
}

impl Strip {
    /// The strip for `target`; a segment longer than [`MAX_STRIP_PX`] is cut around `along`.
    #[must_use]
    pub fn of(target: &Target) -> Self {
        let full = (target.end - target.start).max(1.0);
        if full <= MAX_STRIP_PX {
            return Self {
                edge: target.edge,
                start: target.start,
                len: full,
            };
        }
        let start =
            (target.along - MAX_STRIP_PX / 2.0).clamp(target.start, target.end - MAX_STRIP_PX);
        Self {
            edge: target.edge,
            start,
            len: MAX_STRIP_PX,
        }
    }

    /// The window size in physical pixels at `scale` (width, height).
    #[must_use]
    pub fn size_px(&self, scale: f32) -> (u32, u32) {
        let along = (self.len * scale).ceil().max(1.0) as u32;
        let depth = (DEPTH_PX * scale).ceil().max(1.0) as u32;
        if self.edge.is_horizontal() {
            (along, depth)
        } else {
            (depth, along)
        }
    }
}

/// Proximity → intensity in `0..=1`: 0 at or beyond `reach`, 1 on the edge, smooth between.
/// NaN and negative reach give 0.
#[must_use]
pub fn intensity(distance: f32, reach: f32) -> f32 {
    if !(distance < reach) || reach <= 0.0 {
        return 0.0;
    }
    let t = 1.0 - distance.max(0.0) / reach;
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intensity_is_zero_far_one_at_the_edge_and_monotonic() {
        assert_eq!(intensity(REACH_PX, REACH_PX), 0.0);
        assert_eq!(intensity(1000.0, REACH_PX), 0.0);
        assert_eq!(intensity(f32::NAN, REACH_PX), 0.0);
        assert_eq!(intensity(0.0, REACH_PX), 1.0);
        assert_eq!(intensity(-5.0, REACH_PX), 1.0);
        let mut last = 0.0;
        for step in (0..=160).rev() {
            let now = intensity(step as f32, REACH_PX);
            assert!(now >= last, "not monotonic at {step}");
            last = now;
        }
    }

    #[test]
    fn a_long_segment_is_cut_around_the_crossing_point_and_stays_inside() {
        let target = |along| Target {
            edge: Edge::Left,
            start: 0.0,
            end: 4000.0,
            along,
        };
        let middle = Strip::of(&target(2000.0));
        assert_eq!(middle.len, MAX_STRIP_PX);
        assert!((middle.start - 1200.0).abs() < 0.01);
        assert_eq!(Strip::of(&target(0.0)).start, 0.0);
        let far = Strip::of(&target(4000.0));
        assert_eq!(far.start + far.len, 4000.0);
    }

    #[test]
    fn strip_size_swaps_with_the_edge_orientation() {
        let top = Strip {
            edge: Edge::Top,
            start: 0.0,
            len: 500.0,
        };
        let left = Strip {
            edge: Edge::Left,
            start: 0.0,
            len: 500.0,
        };
        assert_eq!(top.size_px(1.0), (500, 36));
        assert_eq!(left.size_px(2.0), (72, 1000));
    }
}
