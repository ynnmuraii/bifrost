//! The premultiplied-ARGB canvas and the glow rasterizer: the pixels of the edge effect.
//!
//! One soft accent per frame and no glyphs: a Gaussian bloom along the linked edge,
//! brightest at the pointer's projection on it, falling off along the edge and away from it,
//! and faded to nothing at the ends of the linked segment so the strip does not end square
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
/// How far from each end of the linked segment the glow fades to nothing (logical px). The
/// strip window is cut exactly to the segment, so without this the glow would end in a
/// straight cut wherever the pointer's projection is not near the end (the owner's report:
/// half a linked edge, glowing right up to the corner of the gap).
pub const TAPER_PX: f32 = 48.0;
/// The taper never eats more than this share of the segment at either end, so a short
/// segment still shows the glow at its middle instead of fading out everywhere.
pub const TAPER_FRACTION: f32 = 0.25;

/// The linked segment a canvas covers, along the edge: the glow fades to nothing at `start`
/// and `end` instead of being cut off by the window. Distances are px from the canvas origin
/// on the same axis as the crossing point of [`glow`], so they carry the display scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    /// Where the segment begins along the edge.
    pub start: f32,
    /// Where it ends.
    pub end: f32,
    /// How far inside either end the alpha reaches zero.
    pub taper: f32,
}

impl Segment {
    /// A segment that is not known (or one that is not to fade at all): all of the canvas.
    pub const WHOLE: Self = Self {
        start: f32::NEG_INFINITY,
        end: f32::INFINITY,
        taper: 0.0,
    };

    /// The segment `start..end` along the edge at `scale` physical px per logical px, with
    /// the fade of [`TAPER_PX`] at either end — or [`TAPER_FRACTION`] of the segment where
    /// that is shorter. The ends are ordered, and a segment the core could not describe is
    /// drawn without the fade rather than not at all.
    #[must_use]
    pub fn of(start: f32, end: f32, scale: f32) -> Self {
        if !start.is_finite() || !end.is_finite() {
            return Self::WHOLE;
        }
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let taper = (TAPER_PX * scale).min(TAPER_FRACTION * (end - start));
        Self {
            start,
            end,
            // `taper > 0.0` is false for NaN too: a scale the caller could not mean draws
            // the glow unfaded rather than nothing.
            taper: if taper > 0.0 { taper } else { 0.0 },
        }
    }

    /// The fade a pixel whose centre is `along` px from the canvas origin gets: 0 at the ends
    /// of the segment, 1 [`Self::taper`] px inside either of them and all the way across the
    /// middle, smoothstep between. A segment with no taper fades nowhere inside itself.
    #[must_use]
    fn fade_at(&self, along: f32) -> f32 {
        if self.taper <= 0.0 {
            return if along >= self.start && along <= self.end {
                1.0
            } else {
                0.0
            };
        }
        let reach = ((along - self.start) / self.taper).min((self.end - along) / self.taper);
        if reach <= 0.0 {
            return 0.0;
        }
        if reach >= 1.0 {
            return 1.0;
        }
        reach * reach * (3.0 - 2.0 * reach)
    }
}

impl Default for Segment {
    /// Nothing known about the segment yet: all of the canvas, no fade.
    fn default() -> Self {
        Self::WHOLE
    }
}

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
/// `segment` is the linked part of the edge the canvas covers: the glow is faded by
/// [`Segment::fade_at`] over its last [`TAPER_PX`] px at either end, so the strip ends where
/// the link does instead of being cut square by the window. Mid-segment pixels are
/// multiplied by exactly 1 and so painted exactly as they were without a taper.
///
/// `edge` says which side of the canvas the linked edge is: the depth axis runs from the
/// edge into the display. Deterministic: the same arguments paint the same pixels.
pub fn glow(canvas: &mut Canvas<'_>, edge: Edge, point: f32, intensity: f32, segment: Segment) {
    let peak = MAX_ALPHA * intensity.clamp(0.0, 1.0);
    // NaN intensity or crossing point draws nothing (the canvas keeps whatever it had).
    if !peak.is_finite() || peak <= 0.0 || !point.is_finite() {
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
        let fade = segment.fade_at(along as f32 + 0.5);
        if fade <= 0.0 {
            continue;
        }
        let along_peak = peak * fade * (-0.5 * offset * offset).exp();
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

    /// A strip canvas of `LEN` px along the edge and [`DEPTH_PX`] px deep, whose linked
    /// segment is the whole of it, drawn into.
    fn draw(edge: Edge, point: f32, intensity: f32) -> (Vec<u32>, usize, usize) {
        draw_segment(edge, LEN, 0.0, LEN as f32, point, intensity)
    }

    /// A strip canvas `len` px along the edge and [`DEPTH_PX`] px deep, covered by the linked
    /// segment `start..end` (px along the edge), drawn into at `point`.
    fn draw_segment(
        edge: Edge,
        len: usize,
        start: f32,
        end: f32,
        point: f32,
        intensity: f32,
    ) -> (Vec<u32>, usize, usize) {
        let (w, h) = if edge.is_horizontal() {
            (len, DEPTH_PX as usize)
        } else {
            (DEPTH_PX as usize, len)
        };
        let mut px = vec![0u32; w * h];
        glow(
            &mut Canvas::new(&mut px, w, h),
            edge,
            point,
            intensity,
            Segment::of(start, end, 1.0),
        );
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

    /// The owner's report: the linked part of an edge ends part way down the display, and the
    /// glow was cut off square there — bright, because nothing but the Gaussian's own tail
    /// took it away by then.
    #[test]
    fn the_glow_fades_to_nothing_at_both_ends_of_the_segment() {
        /// A linked segment shorter than the display, the pointer's projection 60 px from its
        /// end.
        const SEG: usize = 360;
        let (px, w, h) = draw_segment(Edge::Left, SEG, 0.0, SEG as f32, 300.5, 1.0);
        let at = |along: usize| alpha(&px, w, h, Edge::Left, along, 0);
        // What the cut looked like before: the same draw with no segment fade at all.
        let mut plain = vec![0u32; w * h];
        glow(
            &mut Canvas::new(&mut plain, w, h),
            Edge::Left,
            300.5,
            1.0,
            Segment::WHOLE,
        );
        let straight = alpha(&plain, w, h, Edge::Left, SEG - 1, 0);
        assert!(
            straight > 40,
            "the segment end was not bright to begin with: {straight}"
        );
        assert!(at(SEG - 1) <= 2, "still a straight cut: {}", at(SEG - 1));
        assert!(at(0) <= 2, "still a straight cut at the start: {}", at(0));
        // Rounded off, not dimmed: one taper length inside the end the glow is still full.
        assert!(
            at(SEG - 48) > 90,
            "the taper ate the glow: {}",
            at(SEG - 48)
        );
        // And only the ends changed: the middle of the segment is the pixels it always was.
        assert_eq!(at(200), alpha(&plain, w, h, Edge::Left, 200, 0));
        assert_eq!(at(250), alpha(&plain, w, h, Edge::Left, 250, 0));
    }

    #[test]
    fn the_taper_is_monotonic_from_each_end_over_the_taper_length() {
        let (px, w, h) = draw_segment(Edge::Top, LEN, 0.0, LEN as f32, POINT, 1.0);
        let at = |along: usize| alpha(&px, w, h, Edge::Top, along, 0);
        let taper = (TAPER_PX.min(TAPER_FRACTION * LEN as f32)) as usize;
        assert_eq!(taper, 48, "the taper is capped by TAPER_PX here");
        assert_eq!(at(0), 0, "the first pixel of the segment is lit");
        assert_eq!(at(LEN - 1), 0, "the last pixel of the segment is lit");
        for along in 1..=taper {
            assert!(
                at(along) >= at(along - 1),
                "the start taper dips at {along}: {} → {}",
                at(along - 1),
                at(along)
            );
        }
        for along in (LEN - taper..LEN - 1).rev() {
            assert!(
                at(along) >= at(along + 1),
                "the end taper dips at {along}: {} → {}",
                at(along + 1),
                at(along)
            );
        }
    }

    #[test]
    fn the_taper_leaves_the_middle_of_a_long_segment_exactly_as_it_was() {
        let (tapered, w, h) = draw_segment(Edge::Top, LEN, 0.0, LEN as f32, POINT, 1.0);
        // The same draw with the fade taken out of the rasterizer: what it painted before the
        // segment ends were known.
        let mut plain = vec![0u32; w * h];
        glow(
            &mut Canvas::new(&mut plain, w, h),
            Edge::Top,
            POINT,
            1.0,
            Segment::WHOLE,
        );
        // Every pixel further than one taper length from either end is bit-for-bit the same,
        // and the peak is still the 40 % cap of the accent, premultiplied.
        for along in 48..=LEN - 49 {
            for depth in 0..DEPTH_PX as usize {
                assert_eq!(
                    tapered[along], plain[along],
                    "the taper changed the middle of the edge at {along}"
                );
                assert_eq!(
                    alpha(&tapered, w, h, Edge::Top, along, depth),
                    alpha(&plain, w, h, Edge::Top, along, depth),
                    "the taper changed the middle at {along}/{depth}"
                );
            }
        }
        assert_eq!(tapered[POINT as usize], 0x66_66_24_12);
        // The fade starts exactly at the taper length: 48 px in, the pixel is untouched.
        let segment = Segment::of(0.0, LEN as f32, 1.0);
        assert_eq!(segment.fade_at(48.5), 1.0);
        assert_eq!(segment.fade_at(LEN as f32 - 48.5), 1.0);
        assert!(segment.fade_at(47.5) < 1.0);
    }

    /// A 40 px linked segment: the taper takes a quarter at either end, and the glow is still
    /// there in the middle of it.
    #[test]
    fn a_short_segment_still_shows_the_glow() {
        let short = Segment::of(0.0, 40.0, 1.0);
        assert_eq!(short.taper, 10.0, "a quarter of the segment");
        let (px, w, h) = draw_segment(Edge::Left, 40, 0.0, 40.0, 20.5, 1.0);
        let at = |along: usize| alpha(&px, w, h, Edge::Left, along, 0);
        assert_eq!(at(20), 102, "the cap at the crossing point");
        assert!(at(0) <= 2, "a straight cut at 0 px: {}", at(0));
        assert!(at(39) <= 2, "a straight cut at 39 px: {}", at(39));
        for along in 1..=10 {
            assert!(at(along) >= at(along - 1), "the taper dips at {along}");
        }
    }

    /// The taper carries the display scale: 48 logical px of fade, whatever the DPI.
    #[test]
    fn the_taper_scales_with_the_display() {
        assert_eq!(Segment::of(0.0, 520.0, 1.0).taper, 48.0);
        assert_eq!(Segment::of(0.0, 1040.0, 2.0).taper, 96.0);
        // A strip cut around the pointer of a segment far longer than the canvas: both ends
        // are hundreds of px outside it, so nothing in the canvas fades.
        let cut = Segment::of(-1200.0, 2800.0, 1.0);
        assert_eq!(cut.taper, 48.0);
        assert_eq!(cut.fade_at(0.0), 1.0);
        assert_eq!(cut.fade_at(504.0), 1.0);
        // A segment the core could not describe is drawn whole rather than not at all.
        let unknown = Segment::of(f32::NAN, f32::NAN, 1.0);
        assert_eq!(unknown, Segment::WHOLE);
        assert_eq!(unknown.fade_at(0.0), 1.0);
    }

    #[test]
    fn the_glow_stays_inside_the_canvas_and_can_be_cleared() {
        let mut buf = vec![0u32; 8 * 4];
        let mut canvas = Canvas::new(&mut buf, 8, 4);
        let segment = Segment::of(0.0, 8.0, 1.0);
        glow(&mut canvas, Edge::Top, -40.0, 1.0, segment);
        glow(&mut canvas, Edge::Top, 60.0, 1.0, segment);
        assert!(canvas.px.iter().any(|p| *p != 0));
        canvas.clear();
        assert!(canvas.px.iter().all(|p| *p == 0));
    }
}
