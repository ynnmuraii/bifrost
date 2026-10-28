//! The components of `mdf.css`: pills, buttons, segmented controls, key caps, dots, icons.
//!
//! Each is an egui-layout-aware function: it measures its text, allocates exactly the size the
//! CSS gives it and paints into that rectangle, so the callers lay things out with plain
//! `ui.horizontal` / `ui.vertical`. Colours and radii come from [`Palette`] only.

use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{
    self, Color32, CursorIcon, Galley, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2,
};

use super::theme::{fam, font, Palette};

/// A piece of text in a run: its colour and family.
#[derive(Clone, Copy)]
pub struct Run<'a> {
    /// The text.
    pub text: &'a str,
    /// Its colour.
    pub color: Color32,
    /// `true` for the emphasised weight (`<b>`).
    pub strong: bool,
}

impl<'a> Run<'a> {
    /// Plain text.
    pub fn plain(text: &'a str, color: Color32) -> Self {
        Run {
            text,
            color,
            strong: false,
        }
    }
    /// Emphasised text.
    pub fn strong(text: &'a str, color: Color32) -> Self {
        Run {
            text,
            color,
            strong: true,
        }
    }
}

/// Lays out runs of one size in the mono face.
pub fn mono_runs(ui: &Ui, runs: &[Run<'_>], size: f32, wrap: f32) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    for run in runs {
        job.append(
            run.text,
            0.0,
            TextFormat {
                font_id: font(
                    size,
                    if run.strong {
                        fam::mono_medium()
                    } else {
                        fam::mono()
                    },
                ),
                color: run.color,
                ..TextFormat::default()
            },
        );
    }
    ui.painter().layout_job(job)
}

/// Lays out runs of one size in the sans face.
pub fn sans_runs(ui: &Ui, runs: &[Run<'_>], size: f32, wrap: f32) -> Arc<Galley> {
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    for run in runs {
        job.append(
            run.text,
            0.0,
            TextFormat {
                font_id: font(
                    size,
                    if run.strong {
                        fam::sans_semibold()
                    } else {
                        fam::sans()
                    },
                ),
                color: run.color,
                ..TextFormat::default()
            },
        );
    }
    ui.painter().layout_job(job)
}

/// Paints `galley` centred on `rect` by its ink, not by the box the font measures it in.
///
/// A galley is as tall as its font's ascent and descent, which are uneven — most of all in the
/// pixel face — and a glyph is not centred in the space it advances either. A badge laid out at
/// `rect.center() - galley.size() / 2.0` therefore sits low and to the left. [`Galley::mesh_bounds`]
/// is the tight box around the glyphs themselves, which is what the eye centres, and it is known
/// as soon as the galley is laid out: no tessellation is needed to ask for it.
pub fn centre_galley(painter: &egui::Painter, rect: Rect, galley: Arc<Galley>, color: Color32) {
    let ink = galley.mesh_bounds;
    // A galley of nothing but blanks has no mesh at all; the font's box is all there is to centre.
    let centre = if ink.is_finite() && ink.is_positive() {
        ink.center().to_vec2()
    } else {
        galley.size() / 2.0
    };
    painter.add(egui::epaint::TextShape::new(
        rect.center() - centre,
        galley,
        color,
    ));
}

/// Paints an already laid-out galley as a widget of its own size.
pub fn show_galley(ui: &mut Ui, galley: Arc<Galley>) -> Response {
    let (rect, response) = ui.allocate_exact_size(galley.size(), Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text())
    });
    if ui.is_rect_visible(rect) {
        ui.painter().add(egui::epaint::TextShape::new(
            rect.min,
            galley,
            Color32::PLACEHOLDER,
        ));
    }
    response
}

/// One line of text.
pub fn text(
    ui: &mut Ui,
    text: &str,
    size: f32,
    family: egui::FontFamily,
    color: Color32,
) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font(size, family), color);
    show_galley(ui, galley)
}

/// Wrapping text across the available width.
pub fn wrapped(
    ui: &mut Ui,
    text: &str,
    size: f32,
    family: egui::FontFamily,
    color: Color32,
) -> Response {
    let galley = ui.painter().layout(
        text.to_owned(),
        font(size, family),
        color,
        ui.available_width(),
    );
    show_galley(ui, galley)
}

/// `.cap`: mono 11 px, upper case, tracked, `--ink-2`.
pub fn cap(ui: &mut Ui, pal: &Palette, label: &str) -> Response {
    let mut job = LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        TextFormat {
            font_id: font(11.0, fam::mono()),
            color: pal.ink2,
            extra_letter_spacing: 0.77,
            ..TextFormat::default()
        },
    );
    let galley = ui.painter().layout_job(job);
    show_galley(ui, galley)
}

/// `.hint`: 12 px, `--ink-2`, wrapping.
pub fn hint(ui: &mut Ui, pal: &Palette, label: &str) -> Response {
    let mut job = LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    job.append(
        label,
        0.0,
        TextFormat {
            font_id: font(12.0, fam::sans()),
            color: pal.ink2,
            line_height: Some(17.4),
            ..TextFormat::default()
        },
    );
    let galley = ui.painter().layout_job(job);
    show_galley(ui, galley)
}

/// The kinds of `.btn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `.btn`.
    Plain,
    /// `.btn.solid`.
    Solid,
    /// `.btn.hot`.
    Hot,
    /// `.btn.ghost`.
    Ghost,
    /// `.btn.link`.
    Link,
}

fn brighten(c: Color32, by: f32) -> Color32 {
    let f = |v: u8| (f32::from(v) * by).min(255.0).round() as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

fn dim(c: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (f32::from(c.a()) * 0.45) as u8)
}

/// `.btn`: 44 px high, 18 px padding, 13 px medium text.
pub fn button(ui: &mut Ui, pal: &Palette, label: &str, kind: Kind, enabled: bool) -> Response {
    button_sized(ui, pal, label, kind, enabled, BTN)
}

/// The small pill: 22 px high, 9 px padding, 12 px text.
pub fn small_button(ui: &mut Ui, pal: &Palette, label: &str, enabled: bool) -> Response {
    button_sized(ui, pal, label, Kind::Plain, enabled, SMALL)
}

/// The action of a notice: a pill that fits inside a banner's frame with room to spare.
pub fn notice_button(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    kind: Kind,
    enabled: bool,
) -> Response {
    button_sized(ui, pal, label, kind, enabled, NOTICE)
}

/// The measured size of a button: the height of its frame, the padding on each
/// side of its label and the size of that label.
#[derive(Clone, Copy)]
struct Metrics {
    height: f32,
    padding: f32,
    text_size: f32,
}

/// `.btn`.
const BTN: Metrics = Metrics {
    height: 44.0,
    padding: 18.0,
    text_size: 13.0,
};

/// The compact framed control.
const SMALL: Metrics = Metrics {
    height: 22.0,
    padding: 9.0,
    text_size: 12.0,
};

/// The control inside a notice.
const NOTICE: Metrics = Metrics {
    height: 32.0,
    padding: 14.0,
    text_size: 13.0,
};

fn button_sized(
    ui: &mut Ui,
    pal: &Palette,
    label: &str,
    kind: Kind,
    enabled: bool,
    metrics: Metrics,
) -> Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        font(metrics.text_size, fam::sans_medium()),
        Color32::PLACEHOLDER,
    );
    let (w, h) = if kind == Kind::Link {
        (galley.size().x, galley.size().y)
    } else {
        (galley.size().x + metrics.padding * 2.0, metrics.height)
    };
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(w, h), sense);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled && ui.is_enabled(), label)
    });
    if enabled {
        response.clone().on_hover_cursor(CursorIcon::PointingHand);
    }
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let hover = enabled && response.hovered();
    let down = enabled && response.is_pointer_button_down_on();
    let (mut fill, mut border, mut ink) = match kind {
        Kind::Plain => (pal.bg2, pal.hair, pal.ink),
        Kind::Solid => (pal.ink, pal.ink, pal.bg),
        Kind::Hot => (pal.accent, pal.accent, pal.on_accent),
        Kind::Ghost => (Color32::TRANSPARENT, Color32::TRANSPARENT, pal.ink),
        Kind::Link => (Color32::TRANSPARENT, Color32::TRANSPARENT, pal.ink2),
    };
    if hover {
        match kind {
            Kind::Plain => (fill, border, ink) = (pal.ink, pal.ink, pal.bg),
            Kind::Solid => (fill, border) = (brighten(pal.ink, 1.15), brighten(pal.ink, 1.15)),
            Kind::Hot => (fill, border) = (brighten(pal.accent, 1.08), brighten(pal.accent, 1.08)),
            Kind::Ghost => (fill, border, ink) = (pal.bg2, pal.hair, pal.ink),
            Kind::Link => ink = pal.ink,
        }
    }
    if down {
        fill = brighten(fill, 0.94);
    }
    if !enabled {
        (fill, border, ink) = (dim(fill), dim(border), dim(ink));
    }
    let painter = ui.painter();
    if kind != Kind::Link {
        let rounding = pal.rad_btn(rect.height());
        if hover && kind == Kind::Hot {
            painter.add(
                egui::epaint::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: pal.glow,
                }
                .as_shape(rect, rounding),
            );
        }
        painter.rect(
            rect,
            rounding,
            fill,
            Stroke::new(1.0, border),
            StrokeKind::Inside,
        );
    }
    let at = Pos2::new(
        rect.center().x - galley.size().x / 2.0,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.add(egui::epaint::TextShape::new(at, Arc::clone(&galley), ink));
    if kind == Kind::Link {
        let y = rect.max.y + 1.0;
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(1.0, ink),
        );
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.expand(2.0),
            pal.rad_btn(rect.height() + 4.0),
            Stroke::new(2.0, pal.accent),
            StrokeKind::Outside,
        );
    }
    response
}

/// How long the selected pill of a segmented control takes to slide to the option clicked.
const SEG_SLIDE: f32 = 0.15;

/// `.seg`: a segmented control; returns the index that was clicked.
///
/// The selected pill slides to the new option over [`SEG_SLIDE`] seconds — the frame that
/// arrives with the value it was asked for is the last one this asks for.
pub fn seg(ui: &mut Ui, pal: &Palette, items: &[&str], on: usize) -> Option<usize> {
    let galleys: Vec<Arc<Galley>> = items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            ui.painter().layout_no_wrap(
                (*label).to_owned(),
                font(
                    13.0,
                    if i == on {
                        fam::sans_medium()
                    } else {
                        fam::sans()
                    },
                ),
                Color32::PLACEHOLDER,
            )
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + 28.0).collect();
    let total = widths.iter().sum::<f32>() + 2.0 * (items.len().saturating_sub(1)) as f32 + 6.0;
    let (outer, _) = ui.allocate_exact_size(Vec2::new(total, 44.0), Sense::hover());
    let rounding = pal.rad_btn(44.0 + 6.0);
    ui.painter()
        .rect(outer, rounding, pal.bg2, pal.hairline(), StrokeKind::Inside);
    // Where the pill belongs now, and where it is on its way there.
    let on = on.min(items.len().saturating_sub(1));
    let mut target_x = outer.min.x + 3.0;
    for width in &widths[..on] {
        target_x += width + 2.0;
    }
    let id = ui.id().with(("seg-pill", items));
    let x = ui
        .ctx()
        .animate_value_with_time(id.with("x"), target_x, SEG_SLIDE);
    let width = ui
        .ctx()
        .animate_value_with_time(id.with("w"), widths[on], SEG_SLIDE);
    let pill = Rect::from_min_size(Pos2::new(x, outer.min.y + 3.0), Vec2::new(width, 38.0));
    ui.painter().rect_filled(pill, pal.rad_btn(38.0), pal.ink);
    // The option the pill is closest to carries the pill's own ink, so exactly one label is
    // legible even while the pill is between two of them.
    let centre = pill.center().x;
    let (mut nearest, mut distance) = (0usize, f32::INFINITY);
    let mut centre_x = outer.min.x + 3.0;
    for (i, width) in widths.iter().enumerate() {
        let here = (centre_x + width / 2.0 - centre).abs();
        if here < distance {
            distance = here;
            nearest = i;
        }
        centre_x += width + 2.0;
    }
    let mut clicked = None;
    let mut x = outer.min.x + 3.0;
    for (i, galley) in galleys.iter().enumerate() {
        let cell = Rect::from_min_size(Pos2::new(x, outer.min.y + 3.0), Vec2::new(widths[i], 38.0));
        x += widths[i] + 2.0;
        let response = ui
            .interact(cell, ui.id().with(("seg", i, items[i])), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::WidgetType::RadioButton,
                ui.is_enabled(),
                i == on,
                items[i],
            )
        });
        if response.clicked() {
            clicked = Some(i);
        }
        let painter = ui.painter();
        let ink = if i == nearest {
            pal.bg
        } else if response.hovered() {
            pal.ink
        } else {
            pal.ink2
        };
        let at = Pos2::new(
            cell.center().x - galley.size().x / 2.0,
            cell.center().y - galley.size().y / 2.0,
        );
        painter.add(egui::epaint::TextShape::new(at, Arc::clone(galley), ink));
    }
    clicked
}

/// Service status dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// `.dot.ok`.
    Ok,
    /// `.dot.off`.
    Off,
}

/// Paints a dot centred at `c`.
pub fn paint_dot(painter: &egui::Painter, pal: &Palette, c: Pos2, dot: Dot) {
    match dot {
        Dot::Ok => {
            // The halo is the same green as the dot, so the light theme's darker one carries.
            let halo = super::theme::rgba(pal.ok.r(), pal.ok.g(), pal.ok.b(), 0.22);
            painter.circle_filled(c, 6.5, halo);
            painter.circle_filled(c, 3.5, pal.ok);
        }
        Dot::Off => {
            painter.circle_stroke(c, 2.75, Stroke::new(1.5, pal.ink2));
        }
    }
}

/// `.pill`: 30 px high, mono 11, optional dot, runs of text.
pub fn pill(ui: &mut Ui, pal: &Palette, dot: Option<Dot>, runs: &[Run<'_>]) -> Response {
    let galley = mono_runs(ui, runs, 11.0, f32::INFINITY);
    let dot_w = if dot.is_some() { 7.0 + 8.0 } else { 0.0 };
    let size = Vec2::new(galley.size().x + dot_w + 24.0, 30.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), galley.text())
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(
            rect,
            CornerRadius999::of(30.0),
            pal.bg2,
            pal.hairline(),
            StrokeKind::Inside,
        );
        let mut x = rect.min.x + 12.0;
        if let Some(dot) = dot {
            paint_dot(painter, pal, Pos2::new(x + 3.5, rect.center().y), dot);
            x += dot_w;
        }
        painter.add(egui::epaint::TextShape::new(
            Pos2::new(x, rect.center().y - galley.size().y / 2.0),
            galley,
            Color32::PLACEHOLDER,
        ));
    }
    response
}

/// A `999px` radius as egui's `CornerRadius`.
pub struct CornerRadius999;

impl CornerRadius999 {
    /// The radius of a pill `height` tall.
    pub fn of(height: f32) -> egui::CornerRadius {
        egui::CornerRadius::same((height / 2.0) as u8)
    }
}

/// `.keys`: a chord of `kbd` caps joined by plus signs.
pub fn chord(ui: &mut Ui, pal: &Palette, chord: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 6.0);
        let mut first = true;
        for key in chord.split('+').map(str::trim).filter(|k| !k.is_empty()) {
            if !first {
                text(ui, "+", 12.0, fam::mono(), pal.ink2);
            }
            first = false;
            kbd(ui, pal, &key_label(key));
        }
    });
}

/// The label a key is shown with: arrows as arrows.
pub fn key_label(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "right" | "arrowright" => "→".to_owned(),
        "left" | "arrowleft" => "←".to_owned(),
        "up" | "arrowup" => "↑".to_owned(),
        "down" | "arrowdown" => "↓".to_owned(),
        "ctrl" | "control" => "Ctrl".to_owned(),
        _ => key.to_owned(),
    }
}

/// One `kbd` cap.
pub fn kbd(ui: &mut Ui, pal: &Palette, label: &str) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font(12.0, fam::mono()), pal.ink);
    let size = Vec2::new(galley.size().x + 18.0, galley.size().y + 12.0 + 2.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label)
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // `border-bottom-width: 3px`: the cap is a 1 px frame with a thicker bottom edge.
        painter.rect(
            rect,
            egui::CornerRadius::same(7),
            pal.bg2,
            Stroke::new(1.0, pal.line),
            StrokeKind::Inside,
        );
        painter.line_segment(
            [
                Pos2::new(rect.min.x + 4.0, rect.max.y - 1.0),
                Pos2::new(rect.max.x - 4.0, rect.max.y - 1.0),
            ],
            Stroke::new(2.0, pal.line),
        );
        // The room the cap leaves for its glyph: inside the 1 px frame and above the 3 px edge,
        // so the label is centred in what the eye reads as the inside of the key.
        let inside = Rect::from_min_max(
            Pos2::new(rect.min.x + 1.0, rect.min.y + 1.0),
            Pos2::new(rect.max.x - 1.0, rect.max.y - 3.0),
        );
        centre_galley(painter, inside, galley, pal.ink);
    }
    response
}

/// `.warn`: an accent-outlined notice with a «!» mark.
pub fn warn(ui: &mut Ui, pal: &Palette, add: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(pal.accent_soft)
        .stroke(Stroke::new(1.0, pal.accent))
        .corner_radius(pal.rad_sm())
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                text(ui, "!", 14.0, fam::pixel(), pal.accent_ink);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    add(ui);
                });
            });
        });
}

/// A horizontal row of controls that wraps (`.duo`).
pub fn duo(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        add(ui);
    });
}

/// The icons of the rail and the top bar, in the 22-unit box of the mockup's SVGs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// «Пары».
    Pairs,
    /// «Клавиши».
    Keys,
    /// «Буфер обмена».
    Clip,
    /// «Диагностика».
    Diag,
    /// «Настройки».
    Settings,
    /// Appearance: a paintbrush, distinct from the theme toggle and service settings.
    Appearance,
    /// ✕.
    Close,
    /// The theme toggle.
    Theme,
}

enum Prim {
    Rect(f32, f32, f32, f32, f32),
    Line(&'static [(f32, f32)]),
}

fn prims(icon: Icon) -> &'static [Prim] {
    use Prim::{Line, Rect as R};
    match icon {
        Icon::Pairs => &[
            R(2.75, 7.75, 7.5, 6.5, 1.5),
            R(11.75, 7.75, 7.5, 6.5, 1.5),
            Line(&[(8.0, 11.0), (14.0, 11.0)]),
        ],
        Icon::Keys => &[
            R(1.75, 5.75, 18.5, 11.5, 2.0),
            Line(&[(5.0, 9.5), (6.0, 9.5)]),
            Line(&[(9.0, 9.5), (10.0, 9.5)]),
            Line(&[(13.0, 9.5), (14.0, 9.5)]),
            Line(&[(17.0, 9.5), (17.5, 9.5)]),
            Line(&[(6.5, 13.5), (15.5, 13.5)]),
        ],
        Icon::Clip => &[
            R(4.75, 3.75, 12.5, 16.5, 2.0),
            Line(&[(8.0, 3.75), (8.0, 1.75), (14.0, 1.75), (14.0, 3.75)]),
            Line(&[(8.0, 10.0), (14.0, 10.0)]),
            Line(&[(8.0, 14.0), (12.0, 14.0)]),
        ],
        Icon::Diag => &[Line(&[
            (1.5, 11.5),
            (6.0, 11.5),
            (8.0, 6.0),
            (12.0, 17.0),
            (14.0, 11.5),
            (20.5, 11.5),
        ])],
        Icon::Settings => &[
            Line(&[(2.5, 6.5), (12.5, 6.5)]),
            Line(&[(17.5, 6.5), (19.5, 6.5)]),
            Line(&[(2.5, 15.5), (5.5, 15.5)]),
            Line(&[(10.5, 15.5), (19.5, 15.5)]),
            R(12.75, 4.25, 4.5, 4.5, 1.0),
            R(5.75, 13.25, 4.5, 4.5, 1.0),
        ],
        Icon::Appearance => &[
            Line(&[
                (9.0, 12.0),
                (16.5, 3.5),
                (19.5, 6.5),
                (11.0, 14.0),
                (9.0, 12.0),
            ]),
            Line(&[
                (9.0, 12.0),
                (6.0, 12.5),
                (4.5, 17.0),
                (2.5, 19.0),
                (8.0, 19.0),
                (11.0, 16.0),
                (11.0, 14.0),
            ]),
        ],
        Icon::Close => &[
            Line(&[(4.0, 4.0), (14.0, 14.0)]),
            Line(&[(14.0, 4.0), (4.0, 14.0)]),
        ],
        Icon::Theme => &[],
    }
}

/// Paints `icon` of `size` points centred at `c`.
pub fn paint_icon(painter: &egui::Painter, icon: Icon, c: Pos2, size: f32, color: Color32) {
    if icon == Icon::Theme {
        // viewBox 18: a circle of r 6.75 with its right half filled.
        let s = size / 18.0;
        painter.circle_stroke(c, 6.75 * s, Stroke::new(1.5 * s.max(0.8), color));
        let n = 14;
        let mut points = Vec::with_capacity(n + 1);
        for k in 0..=n {
            let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * k as f32 / n as f32;
            points.push(Pos2::new(
                c.x + 6.75 * s * a.cos(),
                c.y + 6.75 * s * a.sin(),
            ));
        }
        painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
        return;
    }
    let view = if icon == Icon::Close { 18.0 } else { 22.0 };
    let s = size / view;
    let origin = Pos2::new(c.x - size / 2.0, c.y - size / 2.0);
    let at = |x: f32, y: f32| Pos2::new(origin.x + x * s, origin.y + y * s);
    let stroke = Stroke::new(1.5, color);
    for prim in prims(icon) {
        match prim {
            Prim::Rect(x, y, w, h, r) => {
                painter.rect_stroke(
                    Rect::from_min_max(at(*x, *y), at(x + w, y + h)),
                    egui::CornerRadius::same((r * s).round() as u8),
                    stroke,
                    StrokeKind::Middle,
                );
            }
            Prim::Line([a, b]) => {
                painter.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
            }
            Prim::Line(points) => {
                let pts: Vec<Pos2> = points.iter().map(|(x, y)| at(*x, *y)).collect();
                painter.add(Shape::line(pts, stroke));
            }
        }
    }
}

/// A styled drop-down; returns the index the user picked.
pub fn select(
    ui: &mut Ui,
    pal: &Palette,
    id: &str,
    shown: &str,
    options: &[String],
) -> Option<usize> {
    let mut picked = None;
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 44.0;
        ui.spacing_mut().button_padding = Vec2::new(12.0, 0.0);
        egui::ComboBox::from_id_salt(id)
            .width(220.0)
            .height(260.0)
            .selected_text(egui::RichText::new(shown).size(14.0).color(pal.ink))
            .show_ui(ui, |ui| {
                for (i, option) in options.iter().enumerate() {
                    if ui.selectable_label(option == shown, option).clicked() {
                        picked = Some(i);
                    }
                }
            });
    });
    picked
}

#[cfg(test)]
mod accessibility_tests {
    use super::*;

    #[test]
    fn audit_custom_controls_expose_accessible_names_and_roles() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        ctx.enable_accesskit();
        let pal = Palette::of(crate::gui_settings::Theme::Light);
        let mut output = ctx.run_ui(Default::default(), |ui| {
            button(ui, &pal, "Connect", Kind::Solid, true);
            seg(ui, &pal, &["Light", "Dark"], 0);
        });
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.unwrap();
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::Button && node.label() == Some("Connect")
        }));
        assert!(tree.nodes.iter().any(|(_, node)| {
            node.role() == egui::accesskit::Role::RadioButton && node.label() == Some("Light")
        }));
    }
}
