//! The premultiplied-ARGB canvas and the glow rasterizer: the pixels of the edge effect.
//!
//! One soft accent per frame and no glyphs: a Gaussian bloom along the linked edge,
//! brightest at the pointer's projection on it, falling off along the edge and away from it
//! (see [`glow`]). The same pixels go to a Win32 layered window, a Wayland `wl_shm` buffer
//! and the offscreen PNG screenshots.

use crate::Edge;

/// `0xRRGGBB`. Tokens of the dark theme in `mdf.css`: `--accent`.
pub const ACCENT: u32 = 0xFF_5A_2E;
/// Peak alpha of the glow, at its brightest pixel. An accent on the desktop, not a light
/// source: the owner asked for a subtle, not bright, effect.
pub const MAX_ALPHA: f32 = 0.40;
/// Standard deviation of the falloff along the edge (logical px).
pub const GLOW_SIGMA_PX: f32 = 72.0;
/// The glow is zero at and beyond this distance from the crossing point (logical px), so the
/// strip window can be sized from it ([`crate::MAX_STRIP_PX`]). The Gaussian's tail is cut
/// where its alpha has already quantized away, which is invisible but keeps the strip small
/// and makes "nothing beyond the reach" exact.
pub const GLOW_REACH_PX: f32 = 3.5 * GLOW_SIGMA_PX;
/// Falloff rate across the edge, in strip depths: `exp(-ACROSS_DECAY)` is left at the far
/// side before the renormalization that makes it exactly zero.
const ACROSS_DECAY: f32 = 3.2;

/// A borrowed pixel buffer: `width * height` premultiplied `0xAARRGGBB` words.
pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: usize,
    h: usize,
}

impl<'a> Canvas<'a> {
    /// # Panics
    /// If the buffer is smaller than `w * h`.
    pub fn new(px: &'a mut [u32], w: usize, h: usize) -> Self {
        assert!(px.len() >= w * h, "canvas buffer too small");
        Self { px, w, h }
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.w
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.h
    }

    pub fn clear(&mut self) {
        self.px[..self.w * self.h].fill(0);
    }
}

/// Paints the glow over whatever the canvas holds: brightest at `point` (px from the canvas
/// origin, along the edge, i.e. the projection of the pointer onto the edge), falling off
/// along the edge (Gaussian, zero at [`GLOW_REACH_PX`]) and away from it (exponential, zero
/// at the far side of the strip). The peak alpha is [`MAX_ALPHA`] × `intensity` (`0..=1`).
///
/// `edge` says which side of the canvas the linked edge is: the depth axis runs from the
/// edge into the display. Deterministic: the same arguments paint the same pixels.
pub fn glow(canvas: &mut Canvas<'_>, edge: Edge, point: f32, intensity: f32) {
    let peak = MAX_ALPHA * intensity.clamp(0.0, 1.0);
    if !(peak > 0.0) || !point.is_finite() {
        return;
    }
    let (along_len, depth_len) = if edge.is_horizontal() {
        (canvas.w, canvas.h)
    } else {
        (canvas.h, canvas.w)
    };
    // Across the edge: `exp(-ACROSS_DECAY * depth / span)`, renormalized to 1 on the edge and
    // exactly 0 on the far side, one multiply per step instead of an `exp` per pixel.
    let span = depth_len.saturating_sub(1).max(1) as f32;
    let decay = (-ACROSS_DECAY / span).exp();
    let tail = (-ACROSS_DECAY).exp();
    let norm = 1.0 / (1.0 - tail);
    let (red, green, blue) = (ACCENT >> 16 & 0xFF, ACCENT >> 8 & 0xFF, ACCENT & 0xFF);
    let width = canvas.w;
    for along in 0..along_len {
        let offset = (along as f32 + 0.5 - point) / GLOW_SIGMA_PX;
        if offset.abs() >= GLOW_REACH_PX / GLOW_SIGMA_PX {
            continue;
        }
        let along_peak = peak * (-0.5 * offset * offset).exp();
        let mut across = 1.0;
        for depth in 0..depth_len {
            let alpha = along_peak * (across - tail) * norm;
            across *= decay;
            // Rounded, not truncated: the peak of a 40 % accent is exactly alpha 102, and
            // truncation would draw 101 there.
            let a = (alpha * 255.0 + 0.5) as u32;
            if a == 0 {
                continue;
            }
            let index = match edge {
                Edge::Top => depth * width + along,
                Edge::Bottom => (canvas.h - 1 - depth) * width + along,
                Edge::Left => along * width + depth,
                Edge::Right => along * width + (width - 1 - depth),
            };
            canvas.px[index] =
                (a << 24) | (red * a / 255) << 16 | (green * a / 255) << 8 | (blue * a / 255);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEPTH_PX;

    const LEN: usize = 520;
    /// Half a pixel in: the pixel centre at `POINT as usize` then sits exactly on the peak.
    const POINT: f32 = 260.5;

    /// A strip canvas of `LEN` px along the edge and [`DEPTH_PX`] px deep, drawn into.
    fn draw(edge: Edge, point: f32, intensity: f32) -> (Vec<u32>, usize, usize) {
        let (w, h) = if edge.is_horizontal() {
            (LEN, DEPTH_PX as usize)
        } else {
            (DEPTH_PX as usize, LEN)
        };
        let mut px = vec![0u32; w * h];
        glow(&mut Canvas::new(&mut px, w, h), edge, point, intensity);
        (px, w, h)
    }

    /// The alpha of the pixel `along` px along the edge and `depth` px into the display.
    fn alpha(px: &[u32], w: usize, h: usize, edge: Edge, along: usize, depth: usize) -> u32 {
        let (x, y) = match edge {
            Edge::Top => (along, depth),
            Edge::Bottom => (along, h - 1 - depth),
            Edge::Left => (depth, along),
            Edge::Right => (w - 1 - depth, along),
        };
        px[y * w + x] >> 24
    }

    #[test]
    fn the_glow_peaks_on_the_edge_at_the_crossing_point_and_fades_along_it() {
        let (px, w, h) = draw(Edge::Top, POINT, 1.0);
        let at = |along: usize| alpha(&px, w, h, Edge::Top, along, 0);
        assert_eq!(at(POINT as usize), 102, "the cap, at the crossing point");
        let mut last = at(POINT as usize);
        for step in [1, 10, 40, 80, 120, 160, 200] {
            let now = at(POINT as usize + step);
            assert!(now <= last, "not falling along the edge at +{step}");
            last = now;
        }
        // Symmetric, and nothing at all past the reach.
        for step in [1, 40, 120, 200, 251] {
            assert_eq!(
                at(POINT as usize - step),
                at(POINT as usize + step),
                "asymmetric at -{step}"
            );
        }
        for along in [0, 7, LEN - 1] {
            assert_eq!(at(along), 0, "lit beyond the reach at {along}");
        }
    }

    #[test]
    fn the_glow_fades_away_from_the_edge_and_nothing_reaches_the_far_side() {
        let (px, w, h) = draw(Edge::Top, POINT, 1.0);
        let mut last = u32::MAX;
        for depth in 0..DEPTH_PX as usize {
            let now = alpha(&px, w, h, Edge::Top, POINT as usize, depth);
            assert!(now <= last, "not falling into the display at depth {depth}");
            last = now;
        }
        assert_eq!(last, 0, "the far side of the strip is lit");
        // Most of the fade is spent in the first half of the strip: a soft edge, not a line.
        assert!(
            alpha(&px, w, h, Edge::Top, POINT as usize, 1)
                < alpha(&px, w, h, Edge::Top, POINT as usize, 0)
        );
        assert!(
            alpha(
                &px,
                w,
                h,
                Edge::Top,
                POINT as usize,
                (DEPTH_PX as usize) / 2
            ) < 20
        );
    }

    #[test]
    fn every_edge_lights_its_own_side_of_the_strip() {
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            let (px, w, h) = draw(edge, POINT, 1.0);
            let peak = px.iter().map(|p| p >> 24).max().expect("a canvas");
            assert!(peak > 0);
            // The brightest pixel of the whole canvas is the one on the linked edge, at the
            // crossing point; a few px either side round to the same byte, so this is a
            // property of the value at that pixel, not of the search's tie-breaking.
            assert_eq!(
                alpha(&px, w, h, edge, POINT as usize, 0),
                peak,
                "{edge:?} on a {w}x{h} canvas"
            );
            assert!(
                alpha(&px, w, h, edge, POINT as usize, 1) < peak,
                "{edge:?} does not fall into the display"
            );
        }
    }

    #[test]
    fn the_peak_alpha_is_capped_and_scales_with_intensity() {
        let cap = (MAX_ALPHA * 255.0) as u32;
        for intensity in [1.0, 2.0, 10.0] {
            let (px, ..) = draw(Edge::Right, POINT, intensity);
            let peak = px.iter().map(|p| p >> 24).max().unwrap_or(0);
            assert_eq!(peak, cap, "intensity {intensity}");
        }
        let (px, w, h) = draw(Edge::Top, POINT, 0.5);
        assert_eq!(alpha(&px, w, h, Edge::Top, POINT as usize, 0), cap / 2);
        for intensity in [0.0, -1.0, f32::NAN] {
            let (px, ..) = draw(Edge::Top, POINT, intensity);
            assert!(px.iter().all(|p| *p == 0), "intensity {intensity} drew");
        }
    }

    #[test]
    fn the_pixels_are_the_accent_colour_premultiplied() {
        let (px, w, _) = draw(Edge::Top, POINT, 1.0);
        assert_eq!(
            px[POINT as usize], 0x66_66_24_12,
            "102 alpha of #FF5A2E, premultiplied"
        );
        assert_eq!(w, LEN, "the top edge runs along x");
    }

    #[test]
    fn the_same_arguments_paint_the_same_pixels() {
        for intensity in [0.25, 0.7, 1.0] {
            let (first, ..) = draw(Edge::Bottom, 123.5, intensity);
            let (second, ..) = draw(Edge::Bottom, 123.5, intensity);
            assert_eq!(first, second);
        }
        // A NaN crossing point is nothing rather than a poisoned canvas.
        let (px, ..) = draw(Edge::Left, f32::NAN, 1.0);
        assert!(px.iter().all(|p| *p == 0));
    }

    #[test]
    fn the_glow_stays_inside_the_canvas_and_can_be_cleared() {
        let mut buf = vec![0u32; 8 * 4];
        let mut canvas = Canvas::new(&mut buf, 8, 4);
        glow(&mut canvas, Edge::Top, -40.0, 1.0);
        glow(&mut canvas, Edge::Top, 60.0, 1.0);
        assert!(canvas.px.iter().any(|p| *p != 0));
        canvas.clear();
        assert!(canvas.px.iter().all(|p| *p == 0));
    }
}
