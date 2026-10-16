//! The ASCII particle field and the ASCII clouds behind the map.
//!
//! This is a port of the mockup's canvas code with the resource rules of the build spec:
//!
//! * the buffers (`parts`, `cells`) are sized once per window size and reused — a frame
//!   allocates nothing here;
//! * glyphs are laid out once per scale factor ([`Glyphs`]); particles share those galleys,
//!   clouds, particles and ASCII links each submit a lazy iterator under one painter lock;
//! * the clouds are rebuilt at most every [`CLOUD_EVERY`] seconds while the field moves;
//!   pausing preserves both layers and their animation clock, even on unrelated repaints;
//! * nothing here schedules a repaint: [`crate::app`] does, and only while the window is
//!   visible and focused, at most 30 times a second — and only while the field is still
//!   moving. [`Field::paint`] says whether it is.

use std::sync::Arc;

use eframe::egui::{self, Color32, Galley, Painter, Pos2, Rect, Shape};
use eframe::epaint::TextShape;

use super::theme::{fam, font, font_epoch, Palette};
use crate::gui_settings::{FieldMode, Look, Theme};

/// A particle per this many square points of the map.
const POINTS_PER_PARTICLE: f32 = 1000.0;
/// The most particles ever, whatever the window size.
const MAX_PARTICLES: usize = 1800;
/// How often the clouds are rebuilt while the field moves.
const CLOUD_EVERY: f32 = 0.08;
/// Cloud cell size.
const CELL_W: f32 = 7.0;
const CELL_H: f32 = 11.0;
/// A field whose fastest particle moves less than this many points a second has settled: the
/// caller is told to stop asking for frames, and the positions the particles have reached are
/// what the window goes on showing. It sits above the home drift (a few points a second at
/// most) so that the drift alone never keeps the window awake, and far below a particle
/// chasing the pointer (hundreds).
const SETTLED_SPEED: f32 = 8.0;
/// How close the eased pointer must be to the real one for the field to count as settled.
const SETTLED_POINTER: f32 = 0.5;
/// How far apart the glyphs of an ASCII isoline are, in points: a character every 13 points
/// leaves gaps wide enough that the chain is unmistakably made of glyphs.
const LINK_STEP: f32 = 13.0;

const BAYER: [f32; 16] = [
    0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0,
];
const RAMP_DARK: &[char] = &[' ', '.', ',', ':', ';', '-', '=', '+', '*', '#', '%', '@'];
const RAMP_LIGHT: &[char] = &[' ', '.', '.', '·', ':', ';', '+', '*', '%', '#', '@'];
/// Every character either of the two layers can draw.
const GLYPH_SET: &str = ".,:;-=+*#%@·|•";

/// One particle: its home, where it is, and what it is doing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Particle {
    hx: f32,
    hy: f32,
    x: f32,
    y: f32,
    ph: f32,
    sp: f32,
    tx: f32,
    ty: f32,
    /// The glyph of a shape this particle is part of, `'\0'` when it is free.
    g: char,
    heat: f32,
    sx: f32,
}

/// One cell of the clouds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    x: f32,
    y: f32,
    /// `'\0'` for a solid block.
    ch: char,
    /// Drawn in the background colour on top of a solid block.
    on_solid: bool,
}

/// The pointer over the map, eased.
#[derive(Debug, Clone, Copy, Default)]
struct Mouse {
    x: f32,
    y: f32,
    sx: f32,
    sy: f32,
    on: bool,
}

/// Laid out glyphs of one size, kept for one pixel scale.
#[derive(Default)]
struct Glyphs {
    ppp: f32,
    epoch: usize,
    items: Vec<(char, Arc<Galley>)>,
}

impl Glyphs {
    fn ensure(&mut self, painter: &Painter, size: f32) {
        let ppp = painter.ctx().pixels_per_point();
        let epoch = font_epoch(painter);
        if !self.items.is_empty() && self.epoch == epoch && (self.ppp - ppp).abs() < f32::EPSILON {
            return;
        }
        self.ppp = ppp;
        self.epoch = epoch;
        self.items.clear();
        for c in GLYPH_SET.chars().chain(std::iter::once('·')) {
            if self.items.iter().any(|(k, _)| *k == c) {
                continue;
            }
            let galley = painter.layout_no_wrap(
                c.to_string(),
                font(size, fam::mono()),
                Color32::PLACEHOLDER,
            );
            self.items.push((c, galley));
        }
    }

    fn get(&self, c: char) -> Option<&Arc<Galley>> {
        self.items.iter().find(|(k, _)| *k == c).map(|(_, g)| g)
    }
}

/// What a frame of the field needs to know.
pub struct Frame<'a> {
    /// The map's rectangle on screen.
    pub map: Rect,
    /// The tokens of the theme.
    pub pal: &'a Palette,
    /// The look.
    pub look: Look,
    /// `true` while the field simulates this frame: the window is visible and focused, the
    /// field is not «Без движения» and the pointer is over the map. The pointer is what moves
    /// the particles, so without one the field rests where it is.
    pub simulate: bool,
    /// Seconds since the window opened; only differences matter.
    pub t: f32,
    /// The pointer, when it is over the map.
    pub pointer: Option<Pos2>,
}

/// The field: particles, clouds and the glyphs they are drawn with.
#[derive(Default)]
pub struct Field {
    parts: Vec<Particle>,
    cells: Vec<Cell>,
    size: (f32, f32),
    rng: u32,
    mouse: Mouse,
    glyphs12: Glyphs,
    glyphs10: Glyphs,
    glyphs11: Glyphs,
    last_t: f32,
    last_cloud: f32,
    cloud_key: Option<(Theme, bool)>,
    was_simulating: bool,
    animation_t: f32,
    shaped: bool,
    form_rect: Option<Rect>,
}

impl Field {
    /// A field with nothing in it; the first frame sizes it.
    pub fn new() -> Self {
        Field {
            rng: 0x9E37_79B9,
            last_cloud: -1.0,
            ..Field::default()
        }
    }

    /// A cached-glyph ASCII isoline, without per-link strings or layout allocations.
    ///
    /// The glyphs are spaced by [`LINK_STEP`], wide enough that the chain reads as a chain of
    /// characters rather than as the dashed line of the other style.
    pub fn link(&mut self, painter: &Painter, from: Pos2, to: Pos2, color: Color32) {
        self.glyphs11.ensure(painter, 11.0);
        let delta = to - from;
        let count = (delta.length() / LINK_STEP).ceil().max(1.0) as usize;
        painter.extend((0..=count).filter_map(|i| {
            let ch = if i == 0 || i == count {
                '+'
            } else if delta.x.abs() >= delta.y.abs() {
                '-'
            } else {
                '|'
            };
            let galley = self.glyphs11.get(ch)?;
            let at = from + delta * (i as f32 / count as f32) - galley.size() / 2.0;
            Some(TextShape::new(at, Arc::clone(galley), color).into())
        }));
    }

    fn rand(&mut self) -> f32 {
        // xorshift32: the field needs scatter, not statistics.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng >> 8) as f32 / 16_777_216.0
    }

    /// Resizes the buffers to a map of `w` × `h`; a no-op while the size is the same.
    fn resize(&mut self, w: f32, h: f32) {
        if (self.size.0 - w).abs() < 1.0 && (self.size.1 - h).abs() < 1.0 {
            return;
        }
        self.size = (w, h);
        let n = ((w * h) / POINTS_PER_PARTICLE).round().max(0.0) as usize;
        let n = n.min(MAX_PARTICLES);
        self.parts.clear();
        self.parts.reserve(n);
        for _ in 0..n {
            let (hx, hy) = (self.rand() * w, self.rand() * h);
            let (ph, sp) = (
                self.rand() * std::f32::consts::TAU,
                0.25 + self.rand() * 0.6,
            );
            self.parts.push(Particle {
                hx,
                hy,
                x: hx,
                y: hy,
                ph,
                sp,
                ..Particle::default()
            });
        }
        let cells = ((w / CELL_W).ceil() * (h / CELL_H).ceil()) as usize;
        self.cells.clear();
        self.cells.reserve(cells * 2);
        self.last_cloud = -1.0;
        self.shaped = false;
    }

    /// Particles gather into a frame around `r` (map-local points): the mockup's hover effect.
    pub fn form(&mut self, r: Rect) {
        self.release();
        if self.parts.is_empty() {
            return;
        }
        let (x, y, w, h, step) = (
            r.min.x - 14.0,
            r.min.y - 14.0,
            r.width() + 28.0,
            r.height() + 28.0,
            9.0,
        );
        let assign = |field: &mut Field, px: f32, py: f32, g: char| {
            let mut best: Option<usize> = None;
            let mut best_d = f32::INFINITY;
            for (i, p) in field.parts.iter().enumerate() {
                if p.g != '\0' {
                    continue;
                }
                let d = (p.x - px).powi(2) + (p.y - py).powi(2);
                if d < best_d {
                    best_d = d;
                    best = Some(i);
                }
            }
            if let Some(i) = best {
                let p = &mut field.parts[i];
                p.tx = px;
                p.ty = py;
                p.g = g;
            }
        };
        for (px, py) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
            assign(self, px, py, '+');
        }
        let mut a = step;
        while a < w - 4.0 {
            assign(self, x + a, y, '-');
            assign(self, x + a, y + h, '-');
            a += step;
        }
        let mut b = step;
        while b < h - 4.0 {
            assign(self, x, y + b, '|');
            assign(self, x + w, y + b, '|');
            b += step;
        }
        self.shaped = true;
        self.form_rect = Some(r);
    }

    /// Carries an existing outline with the displayed card, without reassigning particles.
    pub fn retarget_form(&mut self, r: Rect) {
        let Some(old) = self.form_rect else {
            return;
        };
        if old.size() != r.size() {
            self.form(r);
            return;
        }
        let delta = r.min - old.min;
        for p in &mut self.parts {
            if p.g != '\0' {
                p.tx += delta.x;
                p.ty += delta.y;
                p.x += delta.x;
                p.y += delta.y;
            }
        }
        self.form_rect = Some(r);
    }

    #[cfg(test)]
    pub(super) fn outline_rect(&self) -> Option<Rect> {
        self.form_rect
    }

    /// Lets the particles of a shape go.
    pub fn release(&mut self) {
        if !self.shaped {
            return;
        }
        for p in &mut self.parts {
            p.g = '\0';
        }
        self.shaped = false;
        self.form_rect = None;
    }

    /// Draws one frame, moving the particles first while `frame.simulate`; `true` when the
    /// field is still visibly moving and the caller should ask for another frame.
    ///
    /// Leaving the map or pausing the window freezes both layers exactly. Reentering resumes
    /// the animation clock rather than catching up with the time spent outside the map.
    pub fn paint(&mut self, painter: &Painter, frame: &Frame<'_>) -> bool {
        let map = frame.map;
        self.resize(map.width(), map.height());
        self.glyphs12.ensure(painter, 12.0);
        self.glyphs10.ensure(painter, 10.0);
        self.glyphs11.ensure(painter, 11.0);
        let simulate = frame.simulate && frame.look.field != FieldMode::Off;
        // A resumed frame has no elapsed simulation time, even if no frames were painted
        // while the window was inactive.
        let dt = if simulate && self.was_simulating {
            (frame.t - self.last_t).clamp(0.0, 0.1)
        } else {
            0.0
        };
        self.last_t = frame.t;
        self.was_simulating = simulate;
        self.animation_t += dt;

        // The pointer, in map coordinates, eased towards the real one.
        let local = frame
            .pointer
            .filter(|_| simulate)
            .map(|p| (p.x - map.min.x, p.y - map.min.y));
        if let Some((x, y)) = local {
            if !self.mouse.on {
                self.mouse.sx = x;
                self.mouse.sy = y;
            }
            self.mouse.x = x;
            self.mouse.y = y;
            self.mouse.on = true;
        }
        if simulate && dt > 0.0 && self.mouse.on {
            let k = 1.0 - (1.0 - 0.16f32).powf((dt * 60.0).max(1.0));
            self.mouse.sx += (self.mouse.x - self.mouse.sx) * k;
            self.mouse.sy += (self.mouse.y - self.mouse.sy) * k;
        }

        // The clouds: only when something about them changed.
        let key = (frame.look.theme, frame.look.clouds);
        let due = self.last_cloud < 0.0
            || self.cloud_key != Some(key)
            || (simulate && dt > 0.0 && self.animation_t - self.last_cloud > CLOUD_EVERY);
        if due {
            self.build_clouds(frame, self.animation_t);
            self.cloud_key = Some(key);
            self.last_cloud = self.animation_t;
        }
        self.draw_clouds(painter, frame);

        let moving = if simulate && dt > 0.0 {
            self.step(frame, dt)
        } else {
            simulate
        };
        self.draw_particles(painter, frame);
        moving
    }

    fn build_clouds(&mut self, frame: &Frame<'_>, t: f32) {
        self.cells.clear();
        if !frame.look.clouds {
            return;
        }
        let (w, h) = self.size;
        let dark = frame.look.theme == Theme::Dark;
        let ramp = if dark { RAMP_DARK } else { RAMP_LIGHT };
        let (cols, rows) = ((w / CELL_W).ceil() as i32, (h / CELL_H).ceil() as i32);
        let (r1, r2) = (h * 0.66, h * 0.5);
        let m = self.mouse;
        for j in 0..rows {
            let py = j as f32 * CELL_H;
            for i in 0..cols {
                let px = i as f32 * CELL_W;
                let (d1x, d1y) = ((px - w * 0.02) / 1.7, py - h * 1.06);
                let (d2x, d2y) = ((px - w) / 1.5, py + h * 0.08);
                let bias = (1.0 - (d1x * d1x + d1y * d1y).sqrt() / r1)
                    .max(1.0 - (d2x * d2x + d2y * d2y).sqrt() / r2);
                if bias < -0.3 {
                    continue;
                }
                let v = fbm(px * 0.011 + t * 0.03, py * 0.019 - t * 0.008);
                let mut d = bias * 1.15 + (v - 0.5) * 1.3;
                if m.on {
                    let (dx, dy) = (px - m.sx, py - m.sy);
                    d -= 1.1 * (-(dx * dx + dy * dy) / 9800.0).exp();
                }
                if d <= 0.0 {
                    continue;
                }
                let lvl = (d * 1.7).min(1.0)
                    + (BAYER[((j & 3) * 4 + (i & 3)) as usize] / 16.0 - 0.47) * 0.24;
                if lvl <= 0.05 {
                    continue;
                }
                let len = ramp.len();
                if dark && lvl > 0.7 {
                    self.cells.push(Cell {
                        x: px,
                        y: py,
                        ch: '\0',
                        on_solid: false,
                    });
                    let inv = ((1.0 - lvl) * 3.4 * len as f32).floor() as usize;
                    if inv > 0 {
                        self.cells.push(Cell {
                            x: px,
                            y: py,
                            ch: ramp[inv.min(len - 1)],
                            on_solid: true,
                        });
                    }
                } else {
                    let idx = ((lvl * len as f32).floor() as usize).clamp(1, len - 1);
                    self.cells.push(Cell {
                        x: px,
                        y: py,
                        ch: ramp[idx],
                        on_solid: false,
                    });
                }
            }
        }
    }

    fn draw_clouds(&self, painter: &Painter, frame: &Frame<'_>) {
        let pal = frame.pal;
        let dark = pal.theme == Theme::Dark;
        // The mockup paints the dark solids at 0.92; at that strength they swallow the title
        // and the captions, so the window keeps them at half strength.
        let (base, alpha) = if dark {
            (pal.ink, 0.5)
        } else {
            (pal.ink2, 0.34)
        };
        let ink = with_alpha(base, alpha);
        let origin = frame.map.min;
        painter.extend(self.cells.iter().filter_map(|cell| {
            let at = Pos2::new(origin.x + cell.x, origin.y + cell.y);
            if cell.ch == '\0' {
                Some(Shape::rect_filled(
                    Rect::from_min_size(at, egui::vec2(CELL_W, CELL_H)),
                    0.0,
                    ink,
                ))
            } else {
                let galley = self.glyphs10.get(cell.ch)?;
                let color = if cell.on_solid { pal.bg } else { ink };
                Some(TextShape::new(at, Arc::clone(galley), color).into())
            }
        }));
    }

    /// Moves every particle one frame and says whether the field is still moving: the caller
    /// keeps asking for frames until it is not.
    fn step(&mut self, frame: &Frame<'_>, dt: f32) -> bool {
        // The slider scales all three: how fast a particle chases its target, how fast it may
        // move at all, and how far it drifts around its home.
        let speed = frame.look.speed;
        let (k, max, amp) = (0.042 * speed, 3.0 * speed, 4.0 * speed);
        let f = (dt * 60.0).clamp(0.5, 3.0);
        let t = self.animation_t;
        let mode = frame.look.field;
        let m = self.mouse;
        let radius = 140.0f32;
        let mut fastest = 0.0f32;
        for p in &mut self.parts {
            let mut tx: f32;
            let mut ty: f32;
            let mut want = 0.0f32;
            if p.g != '\0' {
                tx = p.tx;
                ty = p.ty;
                want = 1.0;
            } else {
                tx = p.hx + (t * p.sp + p.ph).cos() * amp;
                ty = p.hy + (t * p.sp * 0.8 + p.ph).sin() * amp;
            }
            if m.on && p.g == '\0' {
                let (dx, dy) = (p.x - m.sx, p.y - m.sy);
                let d2 = dx * dx + dy * dy;
                if d2 < radius * radius && d2 > 1.0 {
                    let d = d2.sqrt();
                    let fall = 0.5 * (1.0 + ((d / radius) * std::f32::consts::PI).cos());
                    if mode == FieldMode::Repel {
                        tx += (dx / d) * 40.0 * fall;
                        ty += (dy / d) * 40.0 * fall;
                    } else {
                        tx += (m.sx + (dx / d) * 54.0 - tx) * fall;
                        ty += (m.sy + (dy / d) * 54.0 - ty) * fall;
                    }
                    want = want.max(fall);
                }
            }
            let (chase, cap) = (k, max * f);
            let (mut sx, mut sy) = ((tx - p.x) * chase * f, (ty - p.y) * chase * f);
            let len = sx.hypot(sy);
            if len > cap {
                sx = sx / len * cap;
                sy = sy / len * cap;
            }
            p.x += sx;
            p.y += sy;
            p.sx += (len / f - p.sx) * 0.2;
            p.heat += (want - p.heat) * 0.08;
            fastest = fastest.max(sx.hypot(sy));
        }
        // The field has settled when the pointer it eases towards has arrived and no particle
        // is moving any more: it then holds the positions it reached. Two frames of one
        // instant cannot say that anything has settled.
        if dt <= 0.0 {
            return true;
        }
        (m.x - m.sx).hypot(m.y - m.sy) > SETTLED_POINTER || fastest / dt > SETTLED_SPEED
    }

    fn draw_particles(&self, painter: &Painter, frame: &Frame<'_>) {
        let pal = frame.pal;
        let dark = pal.theme == Theme::Dark;
        let calm = if dark {
            with_alpha(pal.ink, 0.46)
        } else {
            with_alpha(pal.ink2, 0.5)
        };
        let hot = with_alpha(pal.accent, 0.92);
        let origin = frame.map.min;
        painter.extend(self.parts.iter().filter_map(|p| {
            let c = if p.g != '\0' {
                p.g
            } else if p.sx < 0.35 {
                '·'
            } else if p.sx < 1.4 {
                '+'
            } else {
                '*'
            };
            let galley = self.glyphs12.get(c)?;
            let size = galley.size();
            let at = Pos2::new(origin.x + p.x - size.x / 2.0, origin.y + p.y - size.y / 2.0);
            let color = if p.heat > 0.28 { hot } else { calm };
            Some(TextShape::new(at, Arc::clone(galley), color).into())
        }));
    }

    /// How many particles there are (for tests and the resource report).
    pub fn particles(&self) -> usize {
        self.parts.len()
    }

    /// How far the furthest particle is from its own resting place, in points: zero when the
    /// field is at rest, tens of points while the pointer holds it away. For tests and the
    /// resource report.
    pub fn displacement(&self) -> f32 {
        self.parts
            .iter()
            .map(|p| (p.x - p.hx).hypot(p.y - p.hy))
            .fold(0.0, f32::max)
    }

    /// How many cloud cells are cached.
    pub fn cloud_cells(&self) -> usize {
        self.cells.len()
    }
}

fn with_alpha(c: Color32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(
        c.r(),
        c.g(),
        c.b(),
        (a.clamp(0.0, 1.0) * 255.0).round() as u8,
    )
}

fn hash(i: i32, j: i32) -> f32 {
    let mut h = i.wrapping_mul(374_761_393) ^ j.wrapping_mul(668_265_263);
    h = (h ^ ((h as u32) >> 13) as i32).wrapping_mul(1_274_126_177);
    (((h ^ ((h as u32) >> 16) as i32) as u32) as f32) / 4_294_967_295.0
}

fn vnoise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor(), y.floor());
    let (xf, yf) = (x - xi, y - yi);
    let (u, v) = (xf * xf * (3.0 - 2.0 * xf), yf * yf * (3.0 - 2.0 * yf));
    let (xi, yi) = (xi as i32, yi as i32);
    let (a, b, c, d) = (
        hash(xi, yi),
        hash(xi + 1, yi),
        hash(xi, yi + 1),
        hash(xi + 1, yi + 1),
    );
    a + (b - a) * u + (c - a) * v + (a - b - c + d) * u * v
}

fn fbm(x: f32, y: f32) -> f32 {
    vnoise(x, y) * 0.55
        + vnoise(x * 2.03 + 17.0, y * 2.03 + 5.0) * 0.3
        + vnoise(x * 4.1 + 3.0, y * 4.1 + 11.0) * 0.15
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_settings::LinkStyle;

    #[test]
    fn default_isolines_emit_ascii_text_not_dashed_paths() {
        assert_eq!(Look::default().link_style, LinkStyle::Ascii);
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let mut field = Field::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            field.link(
                ui.painter(),
                Pos2::new(20.0, 20.0),
                Pos2::new(110.0, 20.0),
                Color32::WHITE,
            );
        });
        let glyphs: Vec<&str> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert_eq!(glyphs.iter().filter(|&&glyph| glyph == "+").count(), 2);
        assert!(glyphs.contains(&"-"));
        assert!(!output
            .shapes
            .iter()
            .any(|shape| matches!(shape.shape, Shape::Path(_))));
        output.drop_without_applying_deltas();
    }

    /// A frame of the map the size the window is drawn at.
    fn map_input(t: f32) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(1280.0, 720.0))),
            time: Some(f64::from(t)),
            ..egui::RawInput::default()
        }
    }

    /// One frame of a field held by a still pointer; `true` while it still moves.
    fn held_frame(
        ctx: &egui::Context,
        field: &mut Field,
        pal: &Palette,
        look: Look,
        pointer: Pos2,
        t: f32,
    ) -> bool {
        let mut moving = false;
        ctx.run_ui(map_input(t), |ui| {
            moving = field.paint(
                ui.painter(),
                &Frame {
                    map: ui.ctx().content_rect(),
                    pal,
                    look,
                    simulate: true,
                    t,
                    pointer: Some(pointer),
                },
            );
        })
        .drop_without_applying_deltas();
        moving
    }

    #[test]
    fn leaving_the_map_or_window_freezes_both_layers_and_resumes_without_wall_time() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let look = Look::default();
        let pal = Palette::of(look.theme);
        let pointer = Pos2::new(400.0, 300.0);
        for paused_pointer in [None, Some(pointer)] {
            let mut paused = Field::new();
            let mut control = Field::new();
            for n in 0..=8 {
                let t = n as f32 / 32.0;
                held_frame(&ctx, &mut paused, &pal, look, pointer, t);
                held_frame(&ctx, &mut control, &pal, look, pointer, t);
            }
            let particles = paused.parts.clone();
            let clouds = paused.cells.clone();
            let animation_t = paused.animation_t;
            assert!(paused.displacement() > 0.0);
            assert!(!clouds.is_empty());

            // Both a missing map pointer and an inactive window retaining its pointer
            // preserve glyphs, heat, and the clouds' pointer distortion.
            let mut frozen_shapes = None;
            for t in [0.5, 8.0, 64.0] {
                let output = ctx.run_ui(map_input(t), |ui| {
                    assert!(!paused.paint(
                        ui.painter(),
                        &Frame {
                            map: ui.ctx().content_rect(),
                            pal: &pal,
                            look,
                            simulate: false,
                            t,
                            pointer: paused_pointer,
                        },
                    ));
                });
                let shapes = format!("{:?}", output.shapes);
                if let Some(frozen) = &frozen_shapes {
                    assert_eq!(&shapes, frozen);
                } else {
                    frozen_shapes = Some(shapes);
                }
                output.drop_without_applying_deltas();
                assert_eq!(paused.parts, particles);
                assert_eq!(paused.cells, clouds);
                assert_eq!(paused.animation_t, animation_t);
            }

            assert!(held_frame(&ctx, &mut paused, &pal, look, pointer, 64.25));
            assert_eq!(paused.parts, particles);
            assert_eq!(paused.cells, clouds);
            assert_eq!(paused.animation_t, animation_t);
            // The next tick matches uninterrupted animation, not elapsed wall time.
            held_frame(&ctx, &mut paused, &pal, look, pointer, 64.28125);
            held_frame(&ctx, &mut control, &pal, look, pointer, 0.28125);
            assert_eq!(paused.parts, control.parts);
            assert_eq!(paused.cells, control.cells);
            assert_eq!(paused.animation_t, control.animation_t);
        }
    }

    /// The mean distance of the particles from their homes.
    fn mean_displacement(field: &Field) -> f32 {
        let sum: f32 = field
            .parts
            .iter()
            .map(|p| (p.x - p.hx).hypot(p.y - p.hy))
            .sum();
        sum / field.parts.len().max(1) as f32
    }

    #[test]
    fn a_held_pointer_keeps_the_particles_and_stops_the_frames_once_they_have_converged() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let look = Look::default();
        let pal = Palette::of(look.theme);
        let pointer = Pos2::new(400.0, 300.0);
        let mut field = Field::new();
        let mut converged_at = None;
        // Five seconds of a pointer that does not move: the window's clock moves, the pointer
        // does not.
        for n in 0..=150 {
            let t = n as f32 / 30.0;
            let moving = held_frame(&ctx, &mut field, &pal, look, pointer, t);
            if n >= 30 {
                // Well past the point where the old window faded the pointer out at 1.5 s and
                // snapped every particle back to its own place.
                assert!(
                    field.displacement() > 20.0,
                    "the pointer holds the particles away from home: {t} s"
                );
            }
            if !moving && converged_at.is_none() {
                converged_at = Some(t);
            }
        }
        let converged_at = converged_at.expect("the field converges within five seconds");
        assert!(
            converged_at < 2.0,
            "convergence takes a second, not five: {converged_at}"
        );
        // Frames drawn after it has settled move nothing and ask for nothing.
        let held = mean_displacement(&field);
        for n in 151..=200 {
            let t = n as f32 / 30.0;
            assert!(
                !held_frame(&ctx, &mut field, &pal, look, pointer, t),
                "a settled field asks for no more frames: {t} s"
            );
        }
        assert!(
            mean_displacement(&field) > held / 2.0,
            "a settled field holds where it is: {} of {held}",
            mean_displacement(&field)
        );
        assert!(field.displacement() > 20.0);

        // Leaving the map preserves the last particle positions instead of settling home.
        let frozen = mean_displacement(&field);
        let mut settled = false;
        ctx.run_ui(map_input(7.0), |ui| {
            settled = field.paint(
                ui.painter(),
                &Frame {
                    map: ui.ctx().content_rect(),
                    pal: &pal,
                    look,
                    simulate: false,
                    t: 7.0,
                    pointer: None,
                },
            );
        })
        .drop_without_applying_deltas();
        assert!(!settled);
        assert_eq!(mean_displacement(&field), frozen);
    }

    #[test]
    fn the_speed_slider_moves_the_field_faster() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let pointer = Pos2::new(400.0, 300.0);
        let moved_by = |speed: f32| {
            let look = Look {
                speed,
                ..Look::default()
            };
            let pal = Palette::of(look.theme);
            let mut field = Field::new();
            for n in 0..=10 {
                held_frame(&ctx, &mut field, &pal, look, pointer, n as f32 / 30.0);
            }
            mean_displacement(&field)
        };
        let (calm, lively) = (moved_by(0.3), moved_by(2.0));
        assert!(lively > calm * 1.5, "{calm} then {lively}");
    }

    /// Every glyph drawn on the segment `from`→`to`, by a frame of `style` with a link there.
    fn glyphs_on_link(style: LinkStyle, from: Pos2, to: Pos2) -> Vec<String> {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let look = Look {
            link_style: style,
            clouds: false,
            ..Look::default()
        };
        let pal = Palette::of(look.theme);
        let mut field = Field::new();
        let mut found = Vec::new();
        let output = ctx.run_ui(map_input(0.0), |ui| {
            field.paint(
                ui.painter(),
                &Frame {
                    map: ui.ctx().content_rect(),
                    pal: &pal,
                    look,
                    // Still: the particles stay at home as '·' and cannot be mistaken for a
                    // glyph of the link.
                    simulate: false,
                    t: 0.0,
                    pointer: None,
                },
            );
            // `map_view` draws the chain for «ASCII-изолиния» and its own dashed line for
            // «Линия»: what is under test here is everything the field itself draws.
            if style == LinkStyle::Ascii {
                field.link(ui.painter(), from, to, Color32::WHITE);
            }
        });
        fn walk(shape: &Shape, from: Pos2, to: Pos2, found: &mut Vec<String>) {
            match shape {
                Shape::Text(text) => {
                    let glyph = text.galley.text();
                    // The particles and the clouds of this frame are '·' and nothing else; a
                    // '+', '-', '|' or '•' on the link can only be the link's own drawing.
                    if matches!(glyph, "+" | "-" | "|" | "•") {
                        let at = text.pos + text.galley.size() / 2.0;
                        let along = (to - from).normalized();
                        let offset = at - from - along * ((at - from).dot(along));
                        if offset.length() < 8.0 && (at - from).dot(along) >= -8.0 {
                            found.push(glyph.to_owned());
                        }
                    }
                }
                Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, from, to, found)),
                _ => {}
            }
        }
        for clipped in &output.shapes {
            walk(&clipped.shape, from, to, &mut found);
        }
        output.drop_without_applying_deltas();
        found
    }

    #[test]
    fn the_ascii_style_draws_glyphs_on_the_link_and_the_line_style_draws_none() {
        let (from, to) = (Pos2::new(100.0, 200.0), Pos2::new(500.0, 200.0));
        let ascii = glyphs_on_link(LinkStyle::Ascii, from, to);
        assert!(
            ascii.iter().filter(|glyph| *glyph == "+").count() >= 2,
            "the isoline ends in two '+' marks: {ascii:?}"
        );
        assert!(
            ascii.iter().filter(|glyph| *glyph == "-").count() > 10,
            "its middle is the dashed chain of a glyph per {LINK_STEP} points: {ascii:?}"
        );
        assert!(
            ascii.iter().any(|glyph| glyph == "•"),
            "and the input of an active link flows through it as dots: {ascii:?}"
        );
        assert_eq!(
            glyphs_on_link(LinkStyle::Line, from, to),
            Vec::<String>::new(),
            "«Линия» is a plain line: the field draws no character on it"
        );
    }

    #[test]
    fn the_noise_is_deterministic_and_in_range() {
        for k in 0..200 {
            let (x, y) = (k as f32 * 0.37, k as f32 * 0.91);
            let v = fbm(x, y);
            assert!((0.0..=1.0).contains(&v), "{v}");
            assert_eq!(v, fbm(x, y));
        }
    }

    #[test]
    fn the_buffers_are_sized_once_per_window_size() {
        let mut field = Field::new();
        field.resize(1000.0, 500.0);
        assert_eq!(field.particles(), 500);
        let capacity = field.parts.capacity();
        field.resize(1000.4, 500.2);
        assert_eq!(field.parts.capacity(), capacity);
        field.resize(30000.0, 20000.0);
        assert_eq!(field.particles(), MAX_PARTICLES);
    }

    #[test]
    fn a_shape_takes_particles_and_releasing_frees_them() {
        let mut field = Field::new();
        field.resize(800.0, 400.0);
        field.form(Rect::from_min_size(
            Pos2::new(100.0, 100.0),
            egui::vec2(140.0, 80.0),
        ));
        let held = field.parts.iter().filter(|p| p.g != '\0').count();
        assert!(held > 20, "{held}");
        field.release();
        assert!(field.parts.iter().all(|p| p.g == '\0'));
    }

    #[test]
    fn an_outline_moves_with_the_displayed_card_without_particle_reassignment() {
        let mut field = Field::new();
        field.resize(800.0, 400.0);
        let card = Rect::from_min_size(Pos2::new(100.0, 100.0), egui::vec2(140.0, 80.0));
        field.form(card);
        let before = field.parts.clone();
        let delta = egui::vec2(96.0, -27.0);
        field.retarget_form(card.translate(delta));
        for (old, new) in before.iter().zip(&field.parts) {
            assert_eq!(old.g, new.g);
            let movement = if old.g == '\0' {
                egui::Vec2::ZERO
            } else {
                delta
            };
            assert_eq!(new.tx, old.tx + movement.x);
            assert_eq!(new.ty, old.ty + movement.y);
            assert_eq!(new.x, old.x + movement.x);
            assert_eq!(new.y, old.y + movement.y);
        }
        assert_eq!(field.form_rect, Some(card.translate(delta)));
    }
}
