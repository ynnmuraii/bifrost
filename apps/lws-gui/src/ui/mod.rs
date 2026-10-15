//! The window of the mockup: top bar, monitor map, pairing dock, left rail and the modals.
//!
//! Everything here is drawing: widgets in, [`Action`]s out. What an action does is the view
//! model's and the bridge's business (`crate::view_model`), and where the geometry of the map
//! comes from is `crate::map`; this module only paints what those say and reports what the user
//! asked for.
//!
//! # Repaint
//!
//! Nothing in the drawing code asks for a frame on its own. [`draw`] returns when the *next*
//! frame is due ([`Output::repaint_after`]) and only for things that really change with time:
//! the particle field (30 fps, only while [`Env::animate`]) and the pairing countdown (1 Hz,
//! only while a window is open). A window with the field off and no pairing window open draws
//! when the user does something or the service says something, and never otherwise.

mod appearance;
mod dock;
mod field;
mod map_view;
mod modal;
mod theme;
mod widgets;

use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, Pos2, Rect, Sense, Stroke, UiBuilder, Vec2};

pub use field::{Field, Frame as FieldFrame};
pub use theme::{install_fonts, Palette};

use crate::gui_settings::Theme;
use crate::map::Pt;
use crate::view_model::{Action, Modal, ViewModel};
use theme::{fam, font};
use widgets::{Dot, Icon, Kind, Run};

/// The top bar's height.
const TOP_H: f32 = 60.0;
/// The rail's width.
const RAIL_W: f32 = 64.0;
/// Below this window width the dock is 2 × 2 and the peer pills are hidden.
const NARROW: f32 = 1180.0;
/// How long the field asks to wait between frames. eframe's event loop wakes a little before
/// the requested time, so 50 ms measured as 28 frames a second on Windows (33 ms measured as
/// 58 — twice the cap), and that is the number the 30 fps ceiling is kept to.
pub const FIELD_FRAME: Duration = Duration::from_millis(50);

/// What the app tells the drawing code about the window.
#[derive(Debug, Clone, Copy)]
pub struct Env {
    /// Now.
    pub now: Instant,
    /// The window is visible and focused: the only time the field may move.
    pub animate: bool,
}

/// What a frame produced.
#[derive(Debug, Default)]
pub struct Output {
    /// What the user asked for.
    pub actions: Vec<Action>,
    /// When the next frame is needed even if nothing happens; `None` for «only on events».
    pub repaint_after: Option<Duration>,
}

/// A computer being dragged.
#[derive(Debug, Clone)]
struct Drag {
    node_id: String,
    start: Pt,
    grab: Pos2,
}

/// The state of the drawing, as opposed to the state of the window: the field, the drag in
/// progress and the cached palette.
pub struct UiState {
    started: Instant,
    /// The particle field.
    pub field: Field,
    drag: Option<Drag>,
    hover: Option<(usize, usize)>,
    applied: Option<Theme>,
    modal_open: Option<(Modal, f32)>,
}

impl UiState {
    /// A fresh drawing state; the clock of the field starts now.
    pub fn new(now: Instant) -> Self {
        UiState {
            started: now,
            field: Field::new(),
            drag: None,
            hover: None,
            applied: None,
            modal_open: None,
        }
    }

    fn t(&self, now: Instant) -> f32 {
        now.saturating_duration_since(self.started).as_secs_f32()
    }
}

/// Draws one frame of the whole window.
pub fn draw(ui: &mut egui::Ui, vm: &ViewModel, st: &mut UiState, env: Env) -> Output {
    // The faces are installed by the app before its first frame; a host that draws earlier
    // (a test harness) gets them here, one frame late, instead of a panic.
    if !ui
        .ctx()
        .fonts(|fonts| fonts.definitions().families.contains_key(&fam::pixel()))
    {
        install_fonts(ui.ctx());
        ui.ctx().request_repaint();
        return Output::default();
    }
    let pal = Palette::of(vm.look.theme);
    if st.applied != Some(pal.theme) {
        theme::apply(ui.ctx(), &pal);
        st.applied = Some(pal.theme);
    }
    let mut out = Output::default();
    let full = ui.ctx().content_rect();
    ui.painter().rect_filled(full, 0.0, pal.bg);

    let narrow = full.width() < NARROW;
    let body_w = full.width() - RAIL_W;
    let dock_h = dock::height(ui.ctx(), vm, narrow);
    let top = Rect::from_min_size(full.min, Vec2::new(full.width(), TOP_H));
    let rail = Rect::from_min_max(Pos2::new(full.max.x - RAIL_W, top.max.y), full.max);
    let dock_rect = Rect::from_min_max(
        Pos2::new(full.min.x, full.max.y - dock_h),
        Pos2::new(full.min.x + body_w, full.max.y),
    );
    let map = Rect::from_min_max(
        Pos2::new(full.min.x, top.max.y),
        Pos2::new(full.min.x + body_w, dock_rect.min.y),
    );

    let t = st.t(env.now);
    let mut wants = map_view::draw(ui, &pal, vm, st, map, env, t, &mut out.actions);
    dock::draw(ui, &pal, vm, dock_rect, narrow, env.now, &mut out.actions);
    let appearance_anchor = rail_draw(ui, &pal, vm, rail, &mut out.actions);
    topbar(ui, &pal, vm, top, narrow, &mut out.actions);
    appearance::draw(ui, &pal, vm, full, appearance_anchor, &mut out.actions);
    modal::draw(ui, &pal, vm, st, full, t, &mut out.actions);
    if out
        .actions
        .iter()
        .any(|action| matches!(action, Action::StartPairing { .. }))
    {
        ui.ctx()
            .memory_mut(|memory| memory.request_focus(egui::Id::new("pairing-address")));
    }

    // The one thing besides the field that moves by itself: the pairing countdown.
    if vm.pairing.window_deadline.is_some() && vm.pairing.window.is_some() {
        wants = Some(wants.map_or(Duration::from_secs(1), |w| w.min(Duration::from_secs(1))));
    }
    out.repaint_after = wants;
    out
}

/// The top bar: brand, service, route, peers, theme.
fn topbar(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    rect: Rect,
    _narrow: bool,
    actions: &mut Vec<Action>,
) {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, pal.card);
    painter.line_segment(
        [
            Pos2::new(rect.min.x, rect.max.y - 0.5),
            Pos2::new(rect.max.x, rect.max.y - 0.5),
        ],
        pal.hairline(),
    );
    let mut bar = ui.new_child(
        UiBuilder::new()
            .id_salt("topbar")
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let bar = &mut bar;
    bar.spacing_mut().item_spacing.x = 12.0;
    bar.add_space(22.0);

    // Brand: the three-rectangle mark and the pixel wordmark.
    let (mark, _) = bar.allocate_exact_size(Vec2::new(22.0, 14.0), Sense::hover());
    let at = |x: f32, y: f32| Pos2::new(mark.min.x + x, mark.min.y + y);
    painter.rect_filled(
        Rect::from_min_max(at(0.0, 3.0), at(6.0, 11.0)),
        1.5,
        pal.ink,
    );
    painter.rect_stroke(
        Rect::from_min_max(at(9.5, 4.5), at(12.5, 9.5)),
        1.0,
        Stroke::new(1.5, pal.ink),
        egui::StrokeKind::Middle,
    );
    painter.rect_filled(
        Rect::from_min_max(at(16.0, 3.0), at(22.0, 11.0)),
        1.5,
        pal.accent,
    );
    widgets::text(bar, "MULTIDESKFLOW", 16.0, fam::pixel(), pal.ink);
    bar.add_space(-6.0);

    let connected = vm.connection.is_connected() && !vm.silent;
    let address = vm.config.listen_address.as_str();
    let (dot, state) = if vm.silent {
        (Dot::Off, "Служба не отвечает")
    } else if vm.connection.is_connected() {
        (Dot::Ok, "Служба работает")
    } else {
        (Dot::Off, "Служба не запущена")
    };
    let mut runs = vec![Run::plain(state, pal.ink2)];
    if connected && !address.is_empty() {
        runs.push(Run::plain(" · ", pal.ink2));
        runs.push(Run::strong(address, pal.ink));
    }
    widgets::pill(bar, pal, Some(dot), &runs);

    let route = vm.route_text();
    let live = vm.status.is_some() && connected;
    widgets::pill(
        bar,
        pal,
        Some(if live { Dot::Live } else { Dot::Off }),
        &[
            Run::plain("Ввод ", pal.ink2),
            Run::strong(&route, if live { pal.accent } else { pal.ink }),
        ],
    );

    bar.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.add_space(12.0 - 6.0);
        // The theme toggle: a 40 px circle.
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
        let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        let hover = response.hovered();
        let label = if pal.theme == Theme::Dark {
            "Светлая тема"
        } else {
            "Тёмная тема"
        };
        let painter = ui.painter();
        painter.circle(
            rect.center(),
            20.0,
            if hover { pal.ink } else { pal.bg2 },
            if hover {
                Stroke::new(1.0, pal.ink)
            } else {
                pal.hairline()
            },
        );
        widgets::paint_icon(
            painter,
            Icon::Theme,
            rect.center(),
            18.0,
            if hover { pal.bg } else { pal.ink },
        );
        if response.on_hover_text(label).clicked() {
            let mut look = vm.look;
            look.theme = look.theme.toggled();
            actions.push(Action::SetLook(look));
        }
        ui.add_space(6.0);
    });
}

/// The left rail: the six sections and the version caption.
fn rail_draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    rect: Rect,
    actions: &mut Vec<Action>,
) -> Rect {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, pal.card);
    painter.line_segment(
        [
            Pos2::new(rect.min.x + 0.5, rect.min.y),
            Pos2::new(rect.min.x + 0.5, rect.max.y),
        ],
        pal.hairline(),
    );
    let icons = [
        Icon::Pairs,
        Icon::Keys,
        Icon::Clip,
        Icon::Diag,
        Icon::Settings,
    ];
    let mut tip: Option<(Rect, &'static str)> = None;
    for (i, modal) in Modal::ALL.iter().enumerate() {
        // Settings sits at the bottom; the other five stack from the top.
        let y = if *modal == Modal::Settings {
            rect.min.y + 12.0 + 4.0 * 50.0 + 6.0
        } else {
            rect.min.y + 12.0 + i as f32 * 50.0
        };
        let btn = Rect::from_min_size(Pos2::new(rect.center().x - 22.0, y), Vec2::splat(44.0));
        let response = ui
            .interact(btn, ui.id().with(("rail", i)), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = vm.modal == Some(*modal);
        let hover = response.hovered() || response.has_focus();
        if on {
            painter.rect(
                btn,
                pal.rad_sm(),
                pal.accent_soft,
                Stroke::new(1.0, pal.accent),
                egui::StrokeKind::Inside,
            );
        } else if hover {
            painter.rect(
                btn,
                pal.rad_sm(),
                pal.bg2,
                pal.hairline(),
                egui::StrokeKind::Inside,
            );
        }
        let ink = if on {
            pal.accent
        } else if hover {
            pal.ink
        } else {
            pal.ink2
        };
        widgets::paint_icon(&painter, icons[i], btn.center(), 20.0, ink);
        let num = painter.layout_no_wrap(
            modal.num().to_owned(),
            font(8.0, fam::mono()),
            Color32::PLACEHOLDER,
        );
        painter.add(egui::epaint::TextShape::new(
            Pos2::new(
                btn.max.x - 3.0 - num.size().x,
                btn.max.y - 2.0 - num.size().y,
            ),
            num,
            Color32::from_rgba_unmultiplied(ink.r(), ink.g(), ink.b(), 150),
        ));
        if hover {
            tip = Some((btn, modal.title()));
        }
        if response.clicked() {
            actions.push(Action::ToggleModal(*modal));
        }
    }
    let appearance = Rect::from_min_size(
        Pos2::new(rect.center().x - 22.0, rect.max.y - 62.0),
        Vec2::splat(44.0),
    );
    let response = ui
        .interact(appearance, ui.id().with("appearance"), Sense::click())
        .on_hover_text("Оформление");
    if response.hovered() || vm.appearance_open {
        painter.rect_filled(appearance, pal.rad_sm(), pal.bg2);
    }
    widgets::paint_icon(&painter, Icon::Theme, appearance.center(), 20.0, pal.ink2);
    if response.clicked() {
        actions.push(Action::ToggleAppearance);
    }
    if let Some((btn, title)) = tip {
        let layer = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            egui::Id::new("rail-tip"),
        ));
        let galley = layer.layout_no_wrap(title.to_owned(), font(12.0, fam::sans_medium()), pal.bg);
        let size = Vec2::new(galley.size().x + 20.0, galley.size().y + 10.0);
        let pill = Rect::from_min_size(
            Pos2::new(btn.min.x - 10.0 - size.x, btn.center().y - size.y / 2.0),
            size,
        );
        layer.rect_filled(
            pill,
            egui::CornerRadius::same((size.y / 2.0) as u8),
            pal.ink,
        );
        layer.galley(
            Pos2::new(pill.min.x + 10.0, pill.min.y + 5.0),
            galley,
            pal.bg,
        );
    }
    appearance
}

/// Convenience for the sub-modules: a plain button that pushes `action` when clicked.
fn act(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    kind: Kind,
    enabled: bool,
    actions: &mut Vec<Action>,
    action: impl FnOnce() -> Action,
) -> egui::Response {
    let response = widgets::button(ui, pal, label, kind, enabled);
    if enabled && response.clicked() {
        actions.push(action());
    }
    response
}
