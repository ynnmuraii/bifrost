//! The ASCII particle field and the ASCII clouds behind the map.
//!
//! This is a port of the mockup's canvas code with the resource rules of the build spec:
//!
//! * the buffers (`parts`, `cells`) are sized once per window size and reused — a frame
//!   allocates nothing here;
//! * glyphs are laid out once per scale factor ([`Glyphs`]) and a particle is one cheap
//!   `TextShape` that shares the glyph's galley, so no `String` or galley is built per frame;
//! * the clouds are rebuilt at most every [`CLOUD_EVERY`] seconds while animating (and only when
//!   the pointer moves while still), never on a plain repaint;
//! * nothing here schedules a repaint: [`crate::app`] does, and only while the window is
//!   visible and focused, at most 30 times a second.

use std::sync::Arc;

use eframe::egui::{self, Color32, Galley, Painter, Pos2, Rect, Shape};
use eframe::epaint::TextShape;

use super::theme::{fam, font, Palette};
use crate::gui_settings::{FieldMode, Look, Pace, Theme};

/// A particle per this many square points of the map.
const POINTS_PER_PARTICLE: f32 = 1000.0;
/// The most particles ever, whatever the window size.
const MAX_PARTICLES: usize = 1800;
/// How often the clouds are rebuilt while the field moves.
const CLOUD_EVERY: f32 = 0.08;
/// Cloud cell size.
const CELL_W: f32 = 7.0;
const CELL_H: f32 = 11.0;
/// Particles of the transfer ribbon, at most.
const RIBBON_MAX: usize = 260;

const BAYER: [f32; 16] = [
    0.0, 8.0, 2.0, 10.0, 12.0, 4.0, 14.0, 6.0, 3.0, 11.0, 1.0, 9.0, 15.0, 7.0, 13.0, 5.0,
];
const RAMP_DARK: &[char] = &[' ', '.', ',', ':', ';', '-', '=', '+', '*', '#', '%', '@'];
const RAMP_LIGHT: &[char] = &[' ', '.', '.', '·', ':', ';', '+', '*', '%', '#', '@'];
/// Every character either of the two layers can draw.
const GLYPH_SET: &str = ".,:;-=+*#%@·|•";

/// One particle: its home, where it is, and what it is doing.
#[derive(Debug, Clone, Copy, Default)]
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
#[derive(Debug, Clone, Copy)]
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
    items: Vec<(char, Arc<Galley>)>,
}

impl Glyphs {
    fn ensure(&mut self, painter: &Painter, size: f32) {
        let ppp = painter.ctx().pixels_per_point();
        if !self.items.is_empty() && (self.ppp - ppp).abs() < f32::EPSILON {
            return;
        }
        self.ppp = ppp;
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

/// A line input flows along while it is active: from → to, in screen points.
#[derive(Debug, Clone, Copy)]
pub struct Flow {
    /// The start.
    pub from: Pos2,
    /// The end.
    pub to: Pos2,
    /// `true` for a horizontal link (the wobble is vertical).
    pub horizontal: bool,
}

/// What a frame of the field needs to know.
pub struct Frame<'a> {
    /// The map's rectangle on screen.
    pub map: Rect,
    /// The tokens of the theme.
    pub pal: &'a Palette,
    /// The look.
    pub look: Look,
    /// `true` while the field moves this frame (visible, focused, not «Без движения»).
    pub animate: bool,
    /// Seconds since the window opened; only differences matter.
    pub t: f32,
    /// The pointer, when it is over the map.
    pub pointer: Option<Pos2>,
    /// The links input flows through.
    pub flows: &'a [Flow],
    /// A ribbon of particles from one computer to another (screen points).
    pub ribbon: Option<(Pos2, Pos2)>,
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
    cloud_key: Option<(Theme, bool, bool, bool)>,
    moved: bool,
    was_animating: bool,
    shaped: bool,
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

    /// Every particle back at its own place.
    fn settle(&mut self) {
        for p in &mut self.parts {
            p.x = p.hx;
            p.y = p.hy;
            p.heat = 0.0;
            p.sx = 0.0;
            p.g = '\0';
        }
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
    }

    /// Draws one frame, moving the particles first when `frame.animate`.
    pub fn paint(&mut self, painter: &Painter, frame: &Frame<'_>) {
        let map = frame.map;
        self.resize(map.width(), map.height());
        self.glyphs12.ensure(painter, 12.0);
        self.glyphs10.ensure(painter, 10.0);
        self.glyphs11.ensure(painter, 11.0);
        let dt = (frame.t - self.last_t).clamp(0.0, 0.1);
        self.last_t = frame.t;
        let animate = frame.animate && frame.look.field != FieldMode::Off;
        if self.was_animating && !animate {
            self.settle();
        }
        self.was_animating = animate;

        // The pointer, in map coordinates, eased towards the real one.
        let local = frame.pointer.map(|p| (p.x - map.min.x, p.y - map.min.y));
        let had_pointer = self.mouse.on;
        match local {
            Some((x, y)) => {
                if !self.mouse.on {
                    self.mouse.sx = x;
                    self.mouse.sy = y;
                }
                self.moved = (self.mouse.x - x).abs() > 0.5 || (self.mouse.y - y).abs() > 0.5;
                self.mouse.x = x;
                self.mouse.y = y;
                self.mouse.on = true;
            }
            None => {
                self.moved = had_pointer;
                self.mouse.on = false;
            }
        }
        if self.mouse.on {
            let k = 1.0 - (1.0 - 0.16f32).powf((dt * 60.0).max(1.0));
            self.mouse.sx += (self.mouse.x - self.mouse.sx) * k;
            self.mouse.sy += (self.mouse.y - self.mouse.sy) * k;
        }

        // The clouds: only when something about them changed.
        let key = (frame.look.theme, frame.look.clouds, animate, self.mouse.on);
        let due = self.last_cloud < 0.0
            || self.cloud_key != Some(key)
            || (animate && frame.t - self.last_cloud > CLOUD_EVERY)
            || (!animate && self.mouse.on && self.moved);
        if due {
            self.build_clouds(frame, if animate { frame.t } else { 0.0 });
            self.cloud_key = Some(key);
            self.last_cloud = frame.t;
        }
        self.draw_clouds(painter, frame);

        if animate {
            self.step(frame, dt);
        }
        self.draw_particles(painter, frame, animate);
        self.draw_flows(painter, frame, animate);
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
        for cell in &self.cells {
            let at = Pos2::new(origin.x + cell.x, origin.y + cell.y);
            if cell.ch == '\0' {
                painter.add(Shape::rect_filled(
                    Rect::from_min_size(at, egui::vec2(CELL_W, CELL_H)),
                    0.0,
                    ink,
                ));
            } else if let Some(galley) = self.glyphs10.get(cell.ch) {
                let color = if cell.on_solid { pal.bg } else { ink };
                painter.add(TextShape::new(at, Arc::clone(galley), color));
            }
        }
    }

    fn step(&mut self, frame: &Frame<'_>, dt: f32) {
        let live = frame.look.pace == Pace::Live;
        let (k, max, amp) = if live {
            (0.075, 5.5, 6.0)
        } else {
            (0.042, 3.0, 4.0)
        };
        let f = (dt * 60.0).clamp(0.5, 3.0);
        let t = frame.t;
        let mode = frame.look.field;
        let m = self.mouse;
        let radius = 140.0f32;
        let rib = frame.ribbon.map(|(a, b)| {
            let a = Pos2::new(a.x - frame.map.min.x, a.y - frame.map.min.y);
            let b = Pos2::new(b.x - frame.map.min.x, b.y - frame.map.min.y);
            (a, b, Pos2::new((a.x + b.x) / 2.0, a.y.min(b.y) - 110.0))
        });
        let rib_n = ((self.parts.len() as f32 * 0.4) as usize).min(RIBBON_MAX);
        for (i, p) in self.parts.iter_mut().enumerate() {
            let mut tx: f32;
            let mut ty: f32;
            let mut want = 0.0f32;
            let mut ribbon = false;
            if p.g != '\0' {
                tx = p.tx;
                ty = p.ty;
                want = 1.0;
            } else if let (Some((a, b, c)), true) = (rib, i < rib_n) {
                let u = ((i as f32 / rib_n as f32) + t * 0.12).fract();
                let w = 1.0 - u;
                tx = w * w * a.x + 2.0 * w * u * c.x + u * u * b.x;
                ty = w * w * a.y
                    + 2.0 * w * u * c.y
                    + u * u * b.y
                    + ((i % 7) as f32 - 3.0) * 5.0
                    + (u * 9.4 + t * 1.6).sin() * 6.0;
                want = 1.0;
                ribbon = true;
            } else {
                tx = p.hx + (t * p.sp + p.ph).cos() * amp;
                ty = p.hy + (t * p.sp * 0.8 + p.ph).sin() * amp;
            }
            if m.on && p.g == '\0' && !ribbon {
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
            let (mut sx, mut sy) = ((tx - p.x) * k * f, (ty - p.y) * k * f);
            let len = sx.hypot(sy);
            let cap = max * f;
            if len > cap {
                sx = sx / len * cap;
                sy = sy / len * cap;
            }
            p.x += sx;
            p.y += sy;
            p.sx += (len / f - p.sx) * 0.2;
            p.heat += (want - p.heat) * 0.08;
        }
    }

    fn draw_particles(&self, painter: &Painter, frame: &Frame<'_>, animate: bool) {
        let pal = frame.pal;
        let dark = pal.theme == Theme::Dark;
        let calm = if dark {
            with_alpha(pal.ink, 0.46)
        } else {
            with_alpha(pal.ink2, 0.5)
        };
        let hot = with_alpha(pal.accent, 0.92);
        let origin = frame.map.min;
        for p in &self.parts {
            let c = if p.g != '\0' {
                p.g
            } else if !animate || p.sx < 0.35 {
                '·'
            } else if p.sx < 1.4 {
                '+'
            } else {
                '*'
            };
            let Some(galley) = self.glyphs12.get(c) else {
                continue;
            };
            let size = galley.size();
            let at = Pos2::new(origin.x + p.x - size.x / 2.0, origin.y + p.y - size.y / 2.0);
            let color = if p.heat > 0.28 { hot } else { calm };
            painter.add(TextShape::new(at, Arc::clone(galley), color));
        }
    }

    fn draw_flows(&self, painter: &Painter, frame: &Frame<'_>, animate: bool) {
        let accent = frame.pal.accent;
        let t = frame.t;
        for flow in frame.flows {
            let n = 7;
            for q in 0..n {
                let u = if animate {
                    ((q as f32 / n as f32) + t * 0.55).fract()
                } else {
                    q as f32 / n as f32
                };
                let x = flow.from.x + (flow.to.x - flow.from.x) * u;
                let y = flow.from.y + (flow.to.y - flow.from.y) * u;
                let wob = if animate {
                    (u * std::f32::consts::TAU + t * 2.0).sin() * 3.0
                } else {
                    0.0
                };
                let c = if u > 0.35 && u < 0.65 { '•' } else { '·' };
                let Some(galley) = self.glyphs11.get(c) else {
                    continue;
                };
                let size = galley.size();
                let (px, py) = if flow.horizontal {
                    (x, y + wob)
                } else {
                    (x + wob, y)
                };
                let alpha = (u * std::f32::consts::PI).sin();
                painter.add(TextShape::new(
                    Pos2::new(px - size.x / 2.0, py - size.y / 2.0),
                    Arc::clone(galley),
                    with_alpha(accent, alpha),
                ));
            }
        }
    }

    /// How many particles there are (for tests and the resource report).
    pub fn particles(&self) -> usize {
        self.parts.len()
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
}
