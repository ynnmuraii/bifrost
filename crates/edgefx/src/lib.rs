//! Edge effects: a soft glow strip that gathers toward the cursor at a linked edge, and the
//! matching "release" bloom on the machine the cursor arrives at.
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
pub mod veil;

pub use effect::{Effect, Phase};
pub use raster::{Canvas, ACCENT, GLOW_REACH_PX, GLOW_SIGMA_PX, MAX_ALPHA};

/// Distance (logical px) from a linked edge at which the glow starts to appear.
pub const REACH_PX: f32 = 160.0;
/// How much further than [`REACH_PX`] the pointer must go before an approach already showing
/// ends. The core owns that state (it is the side that sees the pointer); the band keeps a
/// pointer resting on the boundary from flipping approach↔leave, which reads as a flicker.
pub const REACH_HYSTERESIS_PX: f32 = 24.0;
/// How deep the glow reaches into the display (logical px).
pub const DEPTH_PX: f32 = 36.0;
/// The longest strip the overlay window will cover (logical px): the glow's own support, so
/// the window is exactly as big as the effect can be.
pub const MAX_STRIP_PX: f32 = 2.0 * GLOW_REACH_PX;
/// Source side: fade-out once input has crossed.
pub const FADE_OUT_S: f32 = 0.30;
/// Receiver side: how long the entry bloom plays.
pub const RELEASE_S: f32 = 0.40;
/// An approach the core has said nothing about for this long fades out: it died mid-approach
/// (or the link went away) and left the effect alone. A pointer that stops moving sends no
/// samples at all, so this is a backstop for lost messages, never the effect's normal life.
pub const LOST_S: f32 = 5.0;
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

    /// The window rectangle `(x, y, width, height)` in desktop pixels for a display whose
    /// device-pixel bounds are `native = [x, y, w, h]`: flush with the strip's edge of that
    /// display, `start` px (logical) along it.
    #[must_use]
    pub fn rect_in(&self, native: [i32; 4], scale: f32) -> (i32, i32, u32, u32) {
        let (w, h) = self.size_px(scale);
        let along = (self.start * scale).round() as i32;
        let (x, y) = match self.edge {
            Edge::Top => (native[0] + along, native[1]),
            Edge::Bottom => (native[0] + along, native[1] + native[3] - h as i32),
            Edge::Left => (native[0], native[1] + along),
            Edge::Right => (native[0] + native[2] - w as i32, native[1] + along),
        };
        (x, y, w, h)
    }
}

/// Proximity → intensity in `0..=1`: 0 at or beyond `reach`, 1 on the edge, smooth between.
/// NaN and negative reach give 0.
#[must_use]
pub fn intensity(distance: f32, reach: f32) -> f32 {
    if distance.is_nan() || distance >= reach || reach <= 0.0 {
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
        assert!((middle.start - (2000.0 - MAX_STRIP_PX / 2.0)).abs() < 0.01);
        assert_eq!(Strip::of(&target(0.0)).start, 0.0);
        let far = Strip::of(&target(4000.0));
        assert_eq!(far.start + far.len, 4000.0);
    }

    /// The window is the glow's own support: a crossing point can sit anywhere in it and
    /// nothing of the effect is cut off.
    #[test]
    fn the_strip_covers_the_whole_glow_around_the_crossing_point() {
        let strip = Strip::of(&Target {
            edge: Edge::Left,
            start: 0.0,
            end: 100_000.0,
            along: 50_000.0,
        });
        assert_eq!(strip.len, MAX_STRIP_PX);
        assert!(
            strip.len >= 2.0 * GLOW_REACH_PX,
            "the glow would be cut off: {} < {}",
            strip.len,
            2.0 * GLOW_REACH_PX
        );
    }

    #[test]
    fn the_window_sits_flush_on_its_edge_of_the_display() {
        // A 4K display at 150 % placed right of the primary one.
        let native = [1920, -200, 3840, 2160];
        let strip = |edge| Strip {
            edge,
            start: 100.0,
            len: 400.0,
        };
        assert_eq!(strip(Edge::Top).rect_in(native, 1.5), (2070, -200, 600, 54));
        assert_eq!(
            strip(Edge::Bottom).rect_in(native, 1.5),
            (2070, 1906, 600, 54)
        );
        assert_eq!(strip(Edge::Left).rect_in(native, 1.5), (1920, -50, 54, 600));
        assert_eq!(
            strip(Edge::Right).rect_in(native, 1.5),
            (5706, -50, 54, 600)
        );
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
