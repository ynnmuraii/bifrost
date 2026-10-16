//! Persistent, edge-to-edge ASCII bridges, independent of background particles.
use std::sync::Arc;

use eframe::egui::{Color32, Galley, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2};
use eframe::epaint::TextShape;

use super::theme::{fam, font, font_epoch, Palette};

const GLYPHS: &str = "|:+#<>^v";

#[derive(Default)]
pub struct Bridge {
    glyphs: Vec<(char, Arc<Galley>)>,
    key: (u32, u32, usize),
    clock: f32,
    last: Option<f32>,
    latency_key: Option<(ui_text::Lang, Option<u32>, u32, usize)>,
    latency: Option<Arc<Galley>>,
}

impl Bridge {
    pub fn tick(&mut self, now: f32, animate: bool) {
        if animate {
            if let Some(last) = self.last {
                self.clock += (now - last).clamp(0.0, 0.1);
            }
            self.last = Some(now);
        } else {
            self.last = None;
        }
    }

    /// Only a changed Status value, language or pixel scale rebuilds the pill's galley.
    pub fn set_latency(&mut self, painter: &Painter, lang: ui_text::Lang, rtt_us: Option<u32>) {
        let key = (
            lang,
            rtt_us,
            painter.ctx().pixels_per_point().to_bits(),
            font_epoch(painter),
        );
        if self.latency_key == Some(key) {
            return;
        }
        self.latency_key = Some(key);
        self.latency = rtt_us.filter(|rtt| *rtt > 0).map(|rtt| {
            let label = latency_label(lang, rtt);
            painter.layout_no_wrap(label, font(10.5, fam::mono()), Color32::PLACEHOLDER)
        });
    }

    pub fn paint_latency(&self, painter: &Painter, center: Pos2, pal: &Palette) {
        let Some(galley) = &self.latency else { return };
        let size = galley.size() + Vec2::new(12.0, 5.0);
        let rect = Rect::from_center_size(center, size);
        painter.rect(
            rect,
            (size.y / 2.0) as u8,
            pal.card,
            Stroke::new(1.0, pal.line),
            StrokeKind::Inside,
        );
        painter.galley(rect.min + Vec2::new(6.0, 2.5), Arc::clone(galley), pal.ink2);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &mut self,
        painter: &Painter,
        a: Pos2,
        b: Pos2,
        forward: bool,
        horizontal: bool,
        zoom: f32,
        band_heights: (f32, f32),
        color: Color32,
    ) {
        let size = (9.0 * zoom).clamp(9.0, 12.0);
        let key = (
            size.to_bits(),
            painter.ctx().pixels_per_point().to_bits(),
            font_epoch(painter),
        );
        if self.key != key || self.glyphs.is_empty() {
            self.key = key;
            self.glyphs.clear();
            for ch in GLYPHS.chars() {
                self.glyphs.push((
                    ch,
                    painter.layout_no_wrap(
                        ch.to_string(),
                        font(size, fam::mono()),
                        Color32::PLACEHOLDER,
                    ),
                ));
            }
        }
        let arrow = if horizontal {
            if (b.x >= a.x) == forward {
                '>'
            } else {
                '<'
            }
        } else if (b.y >= a.y) == forward {
            'v'
        } else {
            '^'
        };
        let (start, delta, _) = geometry(a, b, size, horizontal);
        let step = size * 0.52;
        let columns = (delta.length() / step).floor().max(1.0) as usize;
        let height = band_heights.0.max(band_heights.1).max(1.0);
        let rows = (height / (size * 0.72)).ceil() as usize | 1;
        let across = if horizontal { Vec2::Y } else { Vec2::X };
        let phase = (self.clock * 7.0) as usize;
        painter.extend((0..columns * rows).filter_map(|i| {
            let row = i / columns;
            let column = i % columns;
            let u = (column as f32 + 0.5) / columns as f32;
            let v = (row as f32 + 0.5) / rows as f32 * 2.0 - 1.0;
            let middle = (std::f32::consts::PI * u).sin();
            let envelope = 1.0;
            let flow_column = if forward { columns - column } else { column };
            let seed =
                (flow_column + phase).wrapping_mul(73_856_093) ^ row.wrapping_mul(19_349_663);
            let noise = (seed % 100) as f32 / 100.0;
            let density = (1.0 - (v / envelope).abs()).max(0.0);
            if noise > 0.25 + density * 0.72 || v.abs() > envelope {
                return None;
            }
            let ch = if row == rows / 2 && (flow_column + phase) % 7 < 3 {
                arrow
            } else {
                ['|', ':', '+', '#'][(seed / 101) % 4]
            };
            let galley = &self.glyphs.iter().find(|(c, _)| *c == ch)?.1;
            let jitter = ((seed / 401 % 100) as f32 / 100.0 - 0.5) * step * 0.32;
            let local_height = band_heights.0 + (band_heights.1 - band_heights.0) * u;
            let at = start + delta * u + across * (v * local_height * 0.5 + jitter)
                - galley.size() / 2.0;
            let shimmer = 0.95 + 0.05 * (self.clock * 3.0 + flow_column as f32 * 0.6).sin();
            let alpha = (0.30 + density * 0.40) * (0.65 + middle * 0.35);
            Some(
                TextShape::new(
                    at,
                    Arc::clone(galley),
                    color.gamma_multiply(alpha * shimmer),
                )
                .into(),
            )
        }));
    }
}

fn latency_label(lang: ui_text::Lang, rtt_us: u32) -> String {
    if rtt_us < 1_000 {
        ui_text::text(lang, ui_text::Key::SubMillisecondLatency).to_owned()
    } else {
        let ms = (u64::from(rtt_us) + 500) / 1_000;
        ui_text::fill(lang, ui_text::Key::ApproxLatencyMs, &[&ms])
    }
}

fn geometry(a: Pos2, b: Pos2, size: f32, horizontal: bool) -> (Pos2, Vec2, usize) {
    let mut delta = b - a;
    let mut start = a;
    // A touching seam has no gap: extend a short bridge evenly over both linked edges.
    let normal_gap = if horizontal {
        delta.x.abs()
    } else {
        delta.y.abs()
    };
    if normal_gap < 0.1 {
        let direction = if horizontal { Vec2::X } else { Vec2::Y };
        start = a - direction * size;
        delta += direction * size * 2.0;
    }
    let step = size * if horizontal { 0.65 } else { 1.2 };
    let columns = (delta.length() / step).floor().max(1.0) as usize;
    (start, delta, columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;

    #[test]
    fn touching_independent_paddles_keep_a_narrow_normal_seam() {
        for horizontal in [true, false] {
            let a = egui::pos2(100.0, 100.0);
            let b = a + if horizontal {
                Vec2::new(0.0, 30.0)
            } else {
                Vec2::new(30.0, 0.0)
            };
            let (start, delta, _) = geometry(a, b, 9.0, horizontal);
            assert_eq!(start + delta * 0.5, a + (b - a) * 0.5);
            assert_eq!(if horizontal { delta.x } else { delta.y }, 18.0);
            assert_eq!(if horizontal { delta.y } else { delta.x }, 30.0);
        }
    }

    #[test]
    fn cached_glyphs_refresh_when_the_font_atlas_is_rebuilt_at_the_same_scale() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let mut bridge = Bridge::default();
        let mut paint = || {
            ctx.run_ui(egui::RawInput::default(), |ui| {
                bridge.paint(
                    ui.painter(),
                    Pos2::ZERO,
                    egui::pos2(100.0, 0.0),
                    true,
                    true,
                    1.0,
                    (100.0, 100.0),
                    Color32::WHITE,
                );
            })
            .drop_without_applying_deltas();
            bridge.key
        };
        let before = paint();
        ctx.set_fonts(egui::FontDefinitions::default());
        let after = paint();
        assert_eq!(before.1, after.1);
        assert_ne!(before.2, after.2);
    }

    #[test]
    fn latency_is_rounded_and_localized_without_inventing_transport() {
        assert_eq!(latency_label(ui_text::Lang::En, 999), "<1 ms");
        assert_eq!(latency_label(ui_text::Lang::Ru, 999), "<1 мс");
        assert_eq!(latency_label(ui_text::Lang::En, 1_499), "≈ 1 ms");
        assert_eq!(latency_label(ui_text::Lang::Ru, 1_500), "≈ 2 мс");
        assert_eq!(latency_label(ui_text::Lang::En, u32::MAX), "≈ 4294967 ms");
    }

    #[test]
    fn unchanged_status_reuses_the_latency_galley() {
        let ctx = egui::Context::default();
        let mut bridge = Bridge::default();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            bridge.set_latency(ui.painter(), ui_text::Lang::En, Some(1_500));
            let first = Arc::clone(bridge.latency.as_ref().unwrap());
            bridge.set_latency(ui.painter(), ui_text::Lang::En, Some(1_500));
            assert!(Arc::ptr_eq(&first, bridge.latency.as_ref().unwrap()));
            bridge.set_latency(ui.painter(), ui_text::Lang::En, None);
            assert!(bridge.latency.is_none());
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn cells_span_horizontal_vertical_gaps_and_touching_seams() {
        for (a, b, horizontal) in [
            (egui::pos2(0.0, 0.0), egui::pos2(100.0, 0.0), true),
            (egui::pos2(0.0, 0.0), egui::pos2(0.0, 100.0), false),
            (Pos2::ZERO, egui::pos2(12.0, 0.0), true),
            (Pos2::ZERO, egui::pos2(0.0, 12.0), false),
            (Pos2::ZERO, Pos2::ZERO, true),
            (Pos2::ZERO, Pos2::ZERO, false),
        ] {
            let (start, delta, columns) = geometry(a, b, 12.0, horizontal);
            assert_eq!(start + delta * 0.5, a + (b - a) * 0.5);
            assert_eq!(delta.y == 0.0, horizontal);
            assert_eq!(delta.x == 0.0, !horizontal);
            if a != b {
                assert_eq!(start, a);
                assert_eq!(start + delta, b);
            } else {
                assert_eq!(delta.length(), 24.0);
            }
            for i in 0..columns {
                let p = start + delta * ((i as f32 + 0.5) / columns as f32);
                let u = (p - start).dot(delta) / delta.length_sq();
                assert!((0.0..=1.0).contains(&u));
            }
        }
    }

    #[test]
    fn frozen_clock_does_not_catch_up() {
        let mut bridge = Bridge::default();
        bridge.tick(1.0, true);
        bridge.tick(1.033, true);
        let held = bridge.clock.to_bits();
        bridge.tick(20.0, false);
        bridge.tick(30.0, false);
        assert_eq!(held, bridge.clock.to_bits());
        bridge.tick(40.0, true);
        assert_eq!(held, bridge.clock.to_bits());
    }

    #[test]
    fn frozen_bridge_shapes_are_byte_identical_and_request_no_repaint() {
        let ctx = egui::Context::default();
        let mut bridge = Bridge::default();
        let mut render = |now| {
            bridge.tick(now, false);
            ctx.run_ui(egui::RawInput::default(), |ui| {
                bridge.set_latency(ui.painter(), ui_text::Lang::Ru, Some(1_500));
                bridge.paint(
                    ui.painter(),
                    egui::pos2(10.0, 100.0),
                    egui::pos2(150.0, 100.0),
                    true,
                    true,
                    1.0,
                    (120.0, 120.0),
                    Color32::WHITE,
                );
                bridge.paint_latency(
                    ui.painter(),
                    egui::pos2(80.0, 100.0),
                    &Palette::of(crate::gui_settings::Theme::Dark),
                );
            })
        };
        render(0.0).drop_without_applying_deltas();
        let first = render(10.0);
        let second = render(20.0);
        assert_eq!(
            format!("{:?}", first.shapes),
            format!("{:?}", second.shapes)
        );
        assert!(second
            .viewport_output
            .values()
            .all(|v| v.repaint_delay != std::time::Duration::ZERO));
    }
}
