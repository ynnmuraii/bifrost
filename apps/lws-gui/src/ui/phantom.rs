//! Quiet materialisation: fixed sparse glyphs, revealed by a slow travelling wave.
use std::sync::Arc;

use eframe::egui::{Color32, Galley, Painter, Pos2, Rect};
use eframe::epaint::TextShape;

use super::theme::{fam, font, font_epoch, Palette};

const GLYPHS: &str = ".,:-+·";
const PERIOD: f32 = 3.8;

#[derive(Default)]
pub(super) struct PhantomArt {
    clock: f32,
    last: Option<f32>,
    key: (u32, u32, usize),
    glyphs: Vec<Arc<Galley>>,
}

fn cell(seed: u32, x: u32, y: u32) -> u32 {
    let mut value = seed ^ x.wrapping_mul(0x9e3779b9) ^ y.wrapping_mul(0x85ebca6b);
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb352d);
    value ^ (value >> 15)
}

fn shimmer(time: f32, phase: f32) -> f32 {
    let wave = ((time / PERIOD - phase) * std::f32::consts::TAU).cos();
    0.16 + 0.32 * ((wave + 1.0) * 0.5).powi(4)
}

impl PhantomArt {
    pub fn tick(&mut self, now: f32, active: bool) {
        if active {
            if let Some(last) = self.last {
                self.clock += (now - last).clamp(0.0, 0.1);
            }
            self.last = Some(now);
        } else {
            self.last = None;
        }
    }

    pub fn paint(
        &mut self,
        painter: &Painter,
        rect: Rect,
        pal: &Palette,
        id: &str,
        open: bool,
        hovered: bool,
    ) {
        // Scale with the display itself, including auto-fit layouts, not only the zoom control.
        let size = (rect.width() / 16.0)
            .clamp(6.0, 18.0)
            .min(rect.height() * 0.25);
        let key = (
            size.to_bits(),
            painter.ctx().pixels_per_point().to_bits(),
            font_epoch(painter),
        );
        if self.key != key || self.glyphs.is_empty() {
            self.key = key;
            self.glyphs.clear();
            for ch in GLYPHS.chars() {
                self.glyphs.push(painter.layout_no_wrap(
                    ch.to_string(),
                    font(size, fam::mono()),
                    Color32::PLACEHOLDER,
                ));
            }
        }
        let seed = id.bytes().fold(2166136261_u32, |hash, byte| {
            (hash ^ u32::from(byte)).wrapping_mul(16777619)
        });
        let inner = rect.shrink(size);
        if inner.width() <= 0.0 || inner.height() <= 0.0 {
            return;
        }
        let clipped = painter.with_clip_rect(inner.intersect(painter.clip_rect()));
        let cols = 12_u32;
        let rows = 5_u32;
        clipped.extend(
            (0..rows)
                .flat_map(|y| (0..cols).map(move |x| (x, y)))
                .filter_map(|(x, y)| {
                    let hash = cell(seed, x, y);
                    if !hash.is_multiple_of(4) {
                        return None;
                    }
                    let u = (x as f32 + 0.5) / cols as f32;
                    let v = (y as f32 + 0.5) / rows as f32;
                    let alpha = shimmer(
                        self.clock,
                        u * 0.65 + v * 0.15 + (seed % 100) as f32 / 100.0,
                    ) * if hovered { 1.4 } else { 1.0 };
                    let color = if open && hash.is_multiple_of(3) {
                        pal.accent_ink
                    } else {
                        pal.ink2
                    };
                    let galley = &self.glyphs[(hash / 4) as usize % self.glyphs.len()];
                    let at = Pos2::new(
                        inner.left() + u * inner.width(),
                        inner.top() + v * inner.height(),
                    ) - galley.size() * 0.5;
                    Some(TextShape::new(at, Arc::clone(galley), color.gamma_multiply(alpha)).into())
                }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyphs_remain_present_and_clipped_at_every_map_scale() {
        use eframe::egui::{self, Shape, Vec2};
        for display_scale in [crate::map::BASE_SCALE, crate::map::BASE_SCALE / 3.0] {
            for zoom in [0.315, 0.45, 0.8, 1.0, 1.5, 3.0] {
                let ctx = egui::Context::default();
                super::super::install_fonts(&ctx);
                let card = Rect::from_min_size(
                    Pos2::new(20.0, 20.0),
                    Vec2::new(1920.0, 1080.0) * (display_scale as f32 * zoom),
                );
                let mut art = PhantomArt::default();
                let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    art.paint(
                        ui.painter(),
                        card,
                        &Palette::of(crate::gui_settings::Theme::Light),
                        "HTPC",
                        true,
                        false,
                    );
                });
                let clips: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        matches!(&shape.shape, Shape::Text(_)).then_some(shape.clip_rect)
                    })
                    .collect();
                output.drop_without_applying_deltas();
                assert!(!clips.is_empty(), "Glyphs survive zoom {zoom}");
                assert!(clips.iter().all(|clip| card.contains_rect(*clip)));
            }
        }
    }

    #[test]
    fn pattern_and_wave_are_deterministic_and_bounded() {
        for seed in [0, 7, u32::MAX] {
            for x in 0..16 {
                for y in 0..7 {
                    assert_eq!(cell(seed, x, y), cell(seed, x, y));
                    for time in [0.0, 1.3, 2.7, 1000.0] {
                        let alpha = shimmer(time, x as f32 / 16.0);
                        assert!((0.16..=0.48).contains(&alpha));
                        assert_eq!(alpha, shimmer(time, x as f32 / 16.0));
                    }
                }
            }
        }
        assert!((shimmer(0.0, 0.2) - shimmer(PERIOD, 0.2)).abs() < 0.00001);
        assert_ne!(shimmer(0.0, 0.2), shimmer(1.3, 0.2));
        assert_ne!(cell(7, 0, 0), cell(7, 1, 0));
    }

    #[test]
    fn inactive_clock_freezes_and_resumes_without_jumping() {
        let mut art = PhantomArt::default();
        art.tick(0.0, true);
        art.tick(0.05, true);
        art.tick(10.0, false);
        assert_eq!(art.clock, 0.05);
        art.tick(20.0, true);
        assert_eq!(art.clock, 0.05);
        art.tick(20.05, true);
        assert!((art.clock - 0.1).abs() < 0.00001);
    }
}
