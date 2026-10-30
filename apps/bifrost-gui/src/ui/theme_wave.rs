//! A short, reversible cell reveal of cached theme visuals. Snapshots are taken only on
//! a theme change; row meshes and their four opacity levels are prepared once, not per frame.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, epaint, Color32, Pos2, Rect, Shape, Vec2};

use super::theme::{fam, font, Palette};
use crate::gui_settings::Theme;

const CELL: Vec2 = Vec2::new(16.0, 20.0);
const SWEEP: f32 = 0.42;
const BAND: f32 = 64.0;
const GLYPHS: &[u8] = b".:-=+*#%@";

struct TileMesh {
    clip: Rect,
    opacity: [Arc<epaint::Mesh>; 4],
}

struct Snapshot {
    rows: Vec<Vec<TileMesh>>,
    font_texture: egui::TextureHandle,
}

impl Snapshot {
    fn capture(ui: &egui::Ui, full: Rect) -> Self {
        let shapes: Vec<_> = ui.ctx().graphics(|graphics| {
            graphics
                .get(ui.layer_id())
                .map(|list| list.all_entries().cloned().collect())
                .unwrap_or_default()
        });
        let row_count = (full.height() / CELL.y).ceil() as usize;
        let mut rows: Vec<Vec<TileMesh>> = (0..row_count).map(|_| Vec::new()).collect();
        let primitives = ui.ctx().tessellate(shapes, ui.ctx().pixels_per_point());
        // Font-atlas UVs can change on the next frame. Keep the snapshot's atlas with its meshes.
        let font_texture = ui.ctx().load_texture(
            "theme-wave-fonts",
            ui.ctx().fonts(|fonts| fonts.image()),
            egui::TextureOptions::LINEAR,
        );
        for primitive in primitives {
            let epaint::Primitive::Mesh(mut mesh) = primitive.primitive else {
                continue;
            };
            if mesh.texture_id == egui::TextureId::default() {
                mesh.texture_id = font_texture.id();
            }
            let mut strips: Vec<_> = (0..row_count)
                .map(|_| epaint::Mesh::with_texture(mesh.texture_id))
                .collect();
            for triangle in mesh.indices.as_chunks::<3>().0 {
                let vertices = triangle.map(|index| mesh.vertices[index as usize]);
                let min_y = vertices
                    .iter()
                    .map(|v| v.pos.y)
                    .fold(f32::INFINITY, f32::min)
                    .max(primitive.clip_rect.top())
                    .max(full.top());
                let max_y = vertices
                    .iter()
                    .map(|v| v.pos.y)
                    .fold(f32::NEG_INFINITY, f32::max)
                    .min(primitive.clip_rect.bottom())
                    .min(full.bottom());
                if max_y < min_y {
                    continue;
                }
                let first = ((min_y - full.top()) / CELL.y).floor() as usize;
                let last = (((max_y - full.top()) / CELL.y).floor() as usize)
                    .min(row_count.saturating_sub(1));
                for strip in strips.iter_mut().take(last + 1).skip(first) {
                    let index = strip.vertices.len() as u32;
                    strip.vertices.extend_from_slice(&vertices);
                    strip
                        .indices
                        .extend_from_slice(&[index, index + 1, index + 2]);
                }
            }
            for (row, mesh) in strips.into_iter().enumerate() {
                if mesh.indices.is_empty() {
                    continue;
                }
                let mesh = Arc::new(mesh);
                rows[row].push(TileMesh {
                    clip: primitive.clip_rect.intersect(row_rect(full, row)),
                    opacity: std::array::from_fn(|level| {
                        if level == 3 {
                            return Arc::clone(&mesh);
                        }
                        let mut faded = (*mesh).clone();
                        for vertex in &mut faded.vertices {
                            vertex.color = vertex.color.gamma_multiply((level + 1) as f32 / 4.0);
                        }
                        Arc::new(faded)
                    }),
                });
            }
        }
        Self { rows, font_texture }
    }

    fn paint(&self, painter: &egui::Painter, row: usize, clip: Rect, opacity: usize) {
        if opacity == 0 || !clip.is_positive() {
            return;
        }
        for mesh in &self.rows[row] {
            let clip = clip.intersect(mesh.clip);
            if clip.is_positive() {
                painter
                    .with_clip_rect(clip)
                    .add(Shape::Mesh(Arc::clone(&mesh.opacity[opacity - 1])));
            }
        }
    }
}

pub(super) struct Wave {
    source: Theme,
    old: Snapshot,
    new: Option<Snapshot>,
    full: Rect,
    progress: f32,
    forward: bool,
    at: Instant,
    started: bool,
    pixels_per_point: f32,
    glyphs: [epaint::Mesh; 9],
}

impl Wave {
    pub(super) fn begin(ui: &egui::Ui, source: Theme, full: Rect, now: Instant) -> Self {
        let galleys: [_; 9] = std::array::from_fn(|index| {
            ui.painter().layout_no_wrap(
                (GLYPHS[index] as char).to_string(),
                font(13.0, fam::mono()),
                Color32::WHITE,
            )
        });
        let glyphs = galleys.map(|galley| {
            ui.ctx()
                .tessellate(
                    vec![epaint::ClippedShape {
                        clip_rect: Rect::EVERYTHING,
                        shape: Shape::galley(Pos2::ZERO, galley, Color32::WHITE),
                    }],
                    ui.ctx().pixels_per_point(),
                )
                .into_iter()
                .find_map(|primitive| match primitive.primitive {
                    epaint::Primitive::Mesh(mesh) => Some(mesh),
                    epaint::Primitive::Callback(_) => None,
                })
                .expect("ASCII density glyph produces a font mesh")
        });
        Self {
            source,
            old: Snapshot::capture(ui, full),
            new: None,
            full,
            progress: 0.0,
            forward: true,
            at: now,
            started: false,
            pixels_per_point: ui.ctx().pixels_per_point(),
            glyphs,
        }
    }

    pub(super) fn retarget(&mut self, theme: Theme) {
        self.forward = theme != self.source;
    }

    pub(super) fn prepare_retarget(&mut self, ui: &egui::Ui, theme: Theme) {
        // The destination snapshot is needed only if a user actually reverses the wave.
        if theme != self.source && self.new.is_none() {
            self.new = Some(Snapshot::capture(ui, self.full));
        }
    }

    /// Paint above the live UI. False means the sweep is finished (or the window resized).
    pub(super) fn paint(
        &mut self,
        ui: &egui::Ui,
        theme: Theme,
        full: Rect,
        now: Instant,
        theme_settled: bool,
    ) -> bool {
        if full != self.full || self.pixels_per_point != ui.ctx().pixels_per_point() {
            return false;
        }
        // Applying Visuals schedules egui's atlas rebuild for the following pass. Start only
        // after that pass completes, preserving the beginning of the sweep across the hitch.
        let elapsed = if theme_settled && self.started {
            now.saturating_duration_since(self.at).as_secs_f32()
        } else {
            0.0
        };
        self.started = theme_settled;
        self.at = now;
        self.progress = (self.progress + elapsed / SWEEP * if self.forward { 1.0 } else { -1.0 })
            .clamp(0.0, 1.0);
        if (self.forward && self.progress == 1.0) || (!self.forward && self.progress == 0.0) {
            return false;
        }
        let snapshot = if theme != self.source {
            &self.old
        } else {
            let Some(new) = &self.new else {
                return false;
            };
            new
        };
        let painter = ui.painter();
        let origin = Pos2::new(full.right() - 32.0, full.top() + 30.0);
        let farthest = full
            .left_bottom()
            .distance(origin)
            .max(full.left_top().distance(origin));
        let radius = self.progress * (farthest + BAND) - BAND / 2.0;
        let columns = (full.width() / CELL.x).ceil() as usize;
        let old_ink = Palette::of(self.source).accent;
        let new_ink = Palette::of(self.source.toggled()).accent;
        let mut front = epaint::Mesh::with_texture(self.old.font_texture.id());
        let expected_cells = snapshot.rows.len() * (BAND / CELL.x) as usize * 2;
        front.reserve_vertices(expected_cells * 4);
        front.reserve_triangles(expected_cells * 2);
        for row in 0..snapshot.rows.len() {
            let rect = row_rect(full, row);
            let mut start = 0;
            let mut previous = None;
            for column in 0..=columns {
                let opacity = (column < columns).then(|| {
                    let center = Pos2::new(
                        full.left() + (column as f32 + 0.5) * CELL.x,
                        rect.center().y,
                    );
                    let old = old_weight(center.distance(origin), radius);
                    if old > 0.0 && old < 1.0 {
                        // A cell keeps its glyph throughout the pass; only its envelope changes.
                        let glyph = &self.glyphs[(column * 3 + row * 5) % GLYPHS.len()];
                        let ink = blend(new_ink, old_ink, old)
                            .gamma_multiply(4.0 * old * (1.0 - old) * 0.65);
                        let offset = center - glyph.calc_bounds().center();
                        let first = front.vertices.len() as u32;
                        front
                            .vertices
                            .extend(glyph.vertices.iter().map(|vertex| epaint::Vertex {
                                pos: vertex.pos + offset,
                                uv: vertex.uv,
                                color: ink,
                            }));
                        front
                            .indices
                            .extend(glyph.indices.iter().map(|index| index + first));
                    }
                    let level = (old * 4.0).round() as usize;
                    if theme != self.source {
                        level
                    } else {
                        4 - level
                    }
                });
                if opacity != previous {
                    if let Some(level) = previous {
                        let clip = Rect::from_min_max(
                            Pos2::new(full.left() + start as f32 * CELL.x, rect.top()),
                            Pos2::new(
                                (full.left() + column as f32 * CELL.x).min(full.right()),
                                rect.bottom(),
                            ),
                        );
                        snapshot.paint(painter, row, clip, level);
                    }
                    start = column;
                    previous = opacity;
                }
            }
        }
        painter.add(Shape::mesh(front));
        ui.ctx().request_repaint_after(Duration::from_millis(16));
        true
    }
}

fn row_rect(full: Rect, row: usize) -> Rect {
    Rect::from_min_max(
        Pos2::new(full.left(), full.top() + row as f32 * CELL.y),
        Pos2::new(
            full.right(),
            (full.top() + (row + 1) as f32 * CELL.y).min(full.bottom()),
        ),
    )
}

fn old_weight(distance: f32, radius: f32) -> f32 {
    let weight = ((distance - radius) / BAND + 0.5).clamp(0.0, 1.0);
    weight * weight * (3.0 - 2.0 * weight)
}

fn blend(a: Color32, b: Color32, weight: f32) -> Color32 {
    Color32::from_rgb(
        egui::lerp(a.r() as f32..=b.r() as f32, weight) as u8,
        egui::lerp(a.g() as f32..=b.g() as f32, weight) as u8,
        egui::lerp(a.b() as f32..=b.b() as f32, weight) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_is_monotonic_soft_and_finishes_without_flashing() {
        for distance in [0.0, 100.0, 800.0, 1600.0] {
            let mut previous = 1.0;
            for step in 0..=100 {
                let radius = step as f32 / 100.0 * (1600.0 + BAND) - BAND / 2.0;
                let weight = old_weight(distance, radius);
                assert!(weight <= previous);
                previous = weight;
            }
            assert_eq!(previous, 0.0);
        }
        assert_eq!(old_weight(500.0, 500.0), 0.5);
    }
}
