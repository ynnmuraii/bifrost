//! «Карта мониторов»: the field behind, the computers you drag, the links, the legend and the
//! three buttons.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};
use eframe::epaint::TextShape;

use super::field::{Flow, Frame};
use super::theme::{fam, font, rgba, Palette};
use super::widgets::{self, Kind};
use super::{act, Env, UiState, FIELD_FRAME};
use crate::gui_settings::FieldMode;
use crate::map::{Dir, STAGE_H, STAGE_W};
use crate::view_model::layout::{LinkState, MapLink, MapModel, MapNode};
use crate::view_model::{Action, ViewModel};

/// How long the particle ribbon of «Передать ввод» lasts, seconds.
const RIBBON_FOR: f32 = 1.8;

/// Stage → screen.
#[derive(Clone, Copy)]
struct Xf {
    origin: Pos2,
    s: f32,
}

impl Xf {
    fn p(&self, x: f64, y: f64) -> Pos2 {
        Pos2::new(
            self.origin.x + x as f32 * self.s,
            self.origin.y + y as f32 * self.s,
        )
    }
}

/// Draws the map region; returns when the next frame is due for the field, if it moves.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    rect: Rect,
    env: Env,
    t: f32,
    actions: &mut Vec<Action>,
) -> Option<Duration> {
    let painter = ui.painter().with_clip_rect(rect);
    let model = vm.map_model();
    let animate = env.animate && vm.look.field != FieldMode::Off && vm.modal.is_none();
    let s = (((rect.width() - 48.0) / STAGE_W as f32)
        .min((rect.height() - 190.0) / STAGE_H as f32))
    .clamp(0.45, 1.0);
    let xf = Xf {
        origin: Pos2::new(
            rect.center().x - STAGE_W as f32 * s / 2.0,
            rect.center().y - STAGE_H as f32 * s / 2.0,
        ),
        s,
    };

    // Where each computer is drawn now: animated towards the model unless it is being dragged.
    let shown: Vec<Pos2> = model
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let dragging = st.drag.as_ref().is_some_and(|d| d.node_id == node.node_id);
            let time = if dragging { 0.0 } else { 0.45 };
            let x = ui.ctx().animate_value_with_time(
                egui::Id::new(("node-x", i)),
                node.at.x as f32,
                time,
            );
            let y = ui.ctx().animate_value_with_time(
                egui::Id::new(("node-y", i)),
                node.at.y as f32,
                time,
            );
            xf.p(f64::from(x), f64::from(y))
        })
        .collect();

    // The links input flows through, for the field.
    let flows: Vec<Flow> = model
        .links
        .iter()
        .filter(|l| l.state == LinkState::Active)
        .map(|l| {
            let (a, b) = link_ends(&model, l, &shown, xf);
            let (from, to) = if l.forward { (a, b) } else { (b, a) };
            Flow {
                from,
                to,
                horizontal: l.dir == Dir::Horizontal,
            }
        })
        .collect();

    // The ribbon of «Передать ввод»: from the computer that had the input to the next one.
    let ribbon = match &st.ribbon {
        Some((from, to)) if animate && (st.ribbon_hover || t < st.ribbon_until) => {
            let center = |id: &str| {
                model.nodes.iter().position(|n| n.node_id == id).map(|i| {
                    shown[i]
                        + Vec2::new(model.nodes[i].w as f32, model.nodes[i].h as f32) * (s / 2.0)
                })
            };
            center(from).zip(center(to))
        }
        _ => None,
    };

    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| rect.contains(*p) && vm.modal.is_none());
    st.field.paint(
        &painter,
        &Frame {
            map: rect,
            pal,
            look: vm.look,
            animate,
            t,
            pointer,
            flows: &flows,
            ribbon,
        },
    );

    header(&painter, pal, vm, rect);

    for link in &model.links {
        draw_link(&painter, pal, &model, link, &shown, xf);
    }
    let mut hovered: Option<(usize, usize)> = None;
    for (i, node) in model.nodes.iter().enumerate() {
        node_ui(
            ui,
            &painter,
            pal,
            st,
            node,
            i,
            shown[i],
            xf,
            &mut hovered,
            actions,
        );
    }
    if hovered != st.hover {
        match hovered {
            Some((n, m)) if animate && st.drag.is_none() => {
                let node = &model.nodes[n];
                let r = node.mons[m].rect;
                let local = Rect::from_min_size(
                    Pos2::new(
                        shown[n].x - rect.min.x + r.x as f32 * s,
                        shown[n].y - rect.min.y + r.y as f32 * s,
                    ),
                    Vec2::new(r.w as f32 * s, r.h as f32 * s),
                );
                st.field.form(local);
            }
            _ => st.field.release(),
        }
        st.hover = hovered;
    }
    if model.nodes.is_empty() {
        let galley = painter.layout_no_wrap(
            if vm.connection.is_connected() {
                "Раскладка ещё не получена".to_owned()
            } else {
                "Нет данных от службы".to_owned()
            },
            font(14.0, fam::sans()),
            pal.ink2,
        );
        let at = rect.center() - galley.size() / 2.0;
        painter.galley(at, galley, pal.ink2);
    }

    banner(ui, pal, vm, rect, actions);
    footer(ui, pal, vm, st, &model, rect, t, actions);

    let ribbon_live = st.ribbon.is_some() && (st.ribbon_hover || t < st.ribbon_until) && animate;
    (animate || ribbon_live).then_some(FIELD_FRAME)
}

/// The heading: «01 — Карта мониторов», «3 компьютера · 4 экрана» and the revision.
fn header(painter: &egui::Painter, pal: &Palette, vm: &ViewModel, rect: Rect) {
    let left = rect.min + Vec2::new(24.0, 20.0);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        "01 — КАРТА МОНИТОРОВ",
        0.0,
        egui::text::TextFormat {
            font_id: font(11.0, fam::mono()),
            color: pal.ink2,
            extra_letter_spacing: 0.77,
            ..Default::default()
        },
    );
    let cap = painter.layout_job(job);
    let cap_h = cap.size().y;
    painter.galley(left, cap, pal.ink2);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &vm.map_title(),
        0.0,
        egui::text::TextFormat {
            font_id: font(30.0, fam::sans_medium()),
            color: pal.ink,
            extra_letter_spacing: -0.75,
            ..Default::default()
        },
    );
    let title = painter.layout_job(job);
    painter.galley(Pos2::new(left.x, left.y + cap_h + 8.0), title, pal.ink);

    // The revision, right-aligned.
    let right = rect.max.x - 24.0;
    let mut job = egui::text::LayoutJob::default();
    job.append(
        "РАСКЛАДКА",
        0.0,
        egui::text::TextFormat {
            font_id: font(11.0, fam::mono()),
            color: pal.ink2,
            extra_letter_spacing: 0.77,
            ..Default::default()
        },
    );
    let cap = painter.layout_job(job);
    let w = cap.size().x;
    painter.galley(Pos2::new(right - w, left.y), cap, pal.ink2);
    let dirty = vm.editor.dirty();
    let rev = painter.layout_no_wrap(
        vm.revision_text(),
        font(12.0, fam::mono_medium()),
        if dirty { pal.accent } else { pal.ink },
    );
    let w = rev.size().x;
    painter.galley(Pos2::new(right - w, left.y + cap_h + 6.0), rev, pal.ink);
}

/// Where a link is drawn: its model points, carried along with the computers at its two ends
/// while they are still gliding to their new places.
fn link_ends(model: &MapModel, link: &MapLink, shown: &[Pos2], xf: Xf) -> (Pos2, Pos2) {
    let carried = |i: usize, p: crate::map::Pt| {
        let target = xf.p(model.nodes[i].at.x, model.nodes[i].at.y);
        xf.p(p.x, p.y) + (shown[i] - target)
    };
    (carried(link.a, link.from), carried(link.b, link.to))
}

fn draw_link(
    painter: &egui::Painter,
    pal: &Palette,
    model: &MapModel,
    link: &MapLink,
    shown: &[Pos2],
    xf: Xf,
) {
    let (a, b) = link_ends(model, link, shown, xf);
    let carry_a = shown[link.a] - xf.p(model.nodes[link.a].at.x, model.nodes[link.a].at.y);
    let carry_b = shown[link.b] - xf.p(model.nodes[link.b].at.x, model.nodes[link.b].at.y);
    let horizontal = link.dir == Dir::Horizontal;
    let (mut color, mut alpha) = (pal.line, 1.0f32);
    match link.state {
        LinkState::Active => color = pal.accent,
        LinkState::Offline | LinkState::Inactive => alpha = 0.45,
        LinkState::Idle => {}
    }
    let color = fade(color, alpha);
    painter.add(Shape::dashed_line(
        &[a, b],
        Stroke::new(1.5, color),
        5.0,
        3.5,
    ));
    if link.whole_desktop {
        // The whole desktop's edge: a short rail along the shared stretch at both ends.
        let (s0, s1) = (link.span.0, link.span.1);
        let (p0, p1, q0, q1) = if horizontal {
            (
                xf.p(link.from.x - 4.0, s0),
                xf.p(link.from.x - 4.0, s1),
                xf.p(link.to.x + 4.0, s0),
                xf.p(link.to.x + 4.0, s1),
            )
        } else {
            (
                xf.p(s0, link.from.y - 4.0),
                xf.p(s1, link.from.y - 4.0),
                xf.p(s0, link.to.y + 4.0),
                xf.p(s1, link.to.y + 4.0),
            )
        };
        let (p0, p1, q0, q1) = (p0 + carry_a, p1 + carry_a, q0 + carry_b, q1 + carry_b);
        let rail = Stroke::new(
            3.0,
            fade(
                if link.state == LinkState::Active {
                    pal.accent
                } else {
                    pal.line
                },
                alpha * 0.6,
            ),
        );
        painter.line_segment([p0, p1], rail);
        painter.line_segment([q0, q1], rail);
    }
    // The label pill.
    let active = link.state == LinkState::Active;
    let galley = painter.layout_no_wrap(
        link.label.clone(),
        font(10.0, fam::mono()),
        if active { pal.accent } else { pal.ink2 },
    );
    let size = galley.size() + Vec2::new(16.0, 4.0);
    let mid = Pos2::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
    let pill = if horizontal {
        Rect::from_min_size(Pos2::new(mid.x - size.x / 2.0, mid.y - 7.0 - size.y), size)
    } else {
        Rect::from_min_size(Pos2::new(a.x + 8.0, mid.y - size.y / 2.0), size)
    };
    painter.rect(
        pill,
        egui::CornerRadius::same((size.y / 2.0) as u8),
        pal.card_glass,
        Stroke::new(1.0, if active { pal.accent } else { pal.hair }),
        StrokeKind::Inside,
    );
    painter.galley(pill.min + Vec2::new(8.0, 2.0), galley, pal.ink2);
}

fn fade(c: Color32, k: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (f32::from(c.a()) * k).round() as u8)
}

/// One computer: its displays (draggable), and the caption under it.
#[allow(clippy::too_many_arguments)]
fn node_ui(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    pal: &Palette,
    st: &mut UiState,
    node: &MapNode,
    index: usize,
    at: Pos2,
    xf: Xf,
    hovered: &mut Option<(usize, usize)>,
    actions: &mut Vec<Action>,
) {
    let s = xf.s;
    let fs = s.clamp(0.8, 1.0);
    let k = if node.online { 1.0 } else { 0.62 };
    for (m, mon) in node.mons.iter().enumerate() {
        let rect = Rect::from_min_size(
            at + Vec2::new(mon.rect.x as f32 * s, mon.rect.y as f32 * s),
            Vec2::new(mon.rect.w as f32 * s, mon.rect.h as f32 * s),
        );
        let id = egui::Id::new(("mon", &node.node_id, m));
        let response = ui.interact(rect, id, Sense::click_and_drag());
        if response.hovered() || response.dragged() {
            ui.ctx().set_cursor_icon(if response.dragged() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
        if response.has_focus() {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        ..Default::default()
                    },
                );
            });
            let (step, dx, dy) = ui.input(|i| {
                let step = if i.modifiers.shift { 32.0 } else { 8.0 };
                (
                    step,
                    f64::from(
                        i.key_pressed(egui::Key::ArrowRight) as i8
                            - i.key_pressed(egui::Key::ArrowLeft) as i8,
                    ),
                    f64::from(
                        i.key_pressed(egui::Key::ArrowDown) as i8
                            - i.key_pressed(egui::Key::ArrowUp) as i8,
                    ),
                )
            });
            if dx != 0.0 || dy != 0.0 {
                actions.push(Action::NudgeNode {
                    node_id: node.node_id.clone(),
                    dx: dx * step,
                    dy: dy * step,
                });
            }
        }
        if response.drag_started() {
            if let Some(grab) = response.interact_pointer_pos() {
                st.drag = Some(super::Drag {
                    node_id: node.node_id.clone(),
                    start: node.at,
                    grab,
                });
                st.field.release();
            }
        }
        if response.dragged() {
            if let (Some(drag), Some(p)) = (&st.drag, response.interact_pointer_pos()) {
                if drag.node_id == node.node_id {
                    actions.push(Action::MoveNode {
                        node_id: node.node_id.clone(),
                        x: drag.start.x + f64::from((p.x - drag.grab.x) / s),
                        y: drag.start.y + f64::from((p.y - drag.grab.y) / s),
                    });
                }
            }
        }
        if response.drag_stopped() {
            st.drag = None;
        }
        if response.hovered() && st.drag.is_none() {
            *hovered = Some((index, m));
        }
        if !painter.clip_rect().intersects(rect) {
            continue;
        }

        let hot = response.hovered() || response.dragged() || response.has_focus();
        let dragging = response.dragged();
        let shadow = if dragging {
            egui::epaint::Shadow {
                offset: [0, 30],
                blur: 80,
                spread: 0,
                color: shadow_color(pal, 0.16),
            }
        } else {
            egui::epaint::Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: shadow_color(pal, 0.07),
            }
        };
        let radius = pal.rad_sm();
        if node.target {
            painter.add(
                egui::epaint::Shadow {
                    offset: [0, 16],
                    blur: 44,
                    spread: 0,
                    color: pal.glow,
                }
                .as_shape(rect, radius),
            );
        } else {
            painter.add(shadow.as_shape(rect, radius));
        }
        let border = if node.target || hot {
            pal.accent
        } else {
            pal.line
        };
        painter.rect_filled(rect, radius, fade(pal.card, k));
        if node.target {
            painter.rect_stroke(
                rect.expand(1.5),
                radius,
                Stroke::new(2.0, fade(pal.accent, k)),
                StrokeKind::Outside,
            );
        }
        let stroke = Stroke::new(1.5, fade(border, k));
        if node.approx {
            dashed_rect(painter, rect.shrink(0.75), stroke);
        } else {
            painter.rect_stroke(rect, radius, stroke, StrokeKind::Inside);
        }

        // Contents: number and tag on the first row, resolution at the bottom.
        let clipped = painter.with_clip_rect(rect);
        let num = clipped.layout_no_wrap(mon.num.clone(), font(10.5 * fs, fam::mono()), pal.ink);
        clipped.galley(rect.min + Vec2::new(9.0 * fs, 7.0 * fs), num, pal.ink);
        if !mon.tag.is_empty() {
            let (ink, edge, fill) = if node.target {
                (pal.accent, pal.accent, pal.accent_soft)
            } else {
                (pal.ink2, pal.hair, pal.bg2)
            };
            let tag = clipped.layout_no_wrap(mon.tag.to_owned(), font(9.5 * fs, fam::mono()), ink);
            let size = tag.size() + Vec2::new(14.0 * fs, 2.0);
            let tag_rect = Rect::from_min_size(
                Pos2::new(rect.max.x - 9.0 * fs - size.x, rect.min.y + 7.0 * fs),
                size,
            );
            clipped.rect(
                tag_rect,
                egui::CornerRadius::same((size.y / 2.0) as u8),
                fill,
                Stroke::new(1.0, edge),
                StrokeKind::Inside,
            );
            clipped.galley(tag_rect.min + Vec2::new(7.0 * fs, 1.0), tag, ink);
        }
        let res = clipped.layout_no_wrap(mon.res.clone(), font(10.5 * fs, fam::mono()), pal.ink);
        clipped.galley(
            Pos2::new(rect.min.x + 9.0 * fs, rect.max.y - 7.0 * fs - res.size().y),
            res,
            pal.ink,
        );
    }

    // The caption: name, system line, who has the keyboard.
    let top = at.y + node.h as f32 * s + 10.0 * fs;
    let name = painter.layout_no_wrap(
        node.name.clone(),
        font(13.0 * fs, fam::sans_semibold()),
        fade(pal.ink, k),
    );
    let mut y = top;
    let h = name.size().y;
    painter.galley(Pos2::new(at.x, y), name, pal.ink);
    y += h + 1.0;
    for (text, color, family) in [
        (node.os.as_str(), pal.ink2, fam::mono()),
        (node.focus.as_str(), pal.accent, fam::mono_medium()),
    ] {
        if text.is_empty() {
            continue;
        }
        let galley =
            painter.layout_no_wrap(text.to_owned(), font(10.5 * fs, family), fade(color, k));
        let h = galley.size().y;
        painter.galley(Pos2::new(at.x, y), galley, color);
        y += h + 1.0;
    }
}

fn shadow_color(pal: &Palette, a: f32) -> Color32 {
    if pal.theme == crate::gui_settings::Theme::Dark {
        rgba(0, 0, 0, (a * 4.0).min(0.65))
    } else {
        rgba(15, 23, 42, a)
    }
}

/// A dashed rectangle outline (`.mon.is-approx`: the size is a guess).
fn dashed_rect(painter: &egui::Painter, r: Rect, stroke: Stroke) {
    let pts = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    painter.add(Shape::dashed_line(&pts, stroke, 4.0, 3.0));
}

/// The notice at the top: the service is not running, or the last result of an action.
fn banner(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, rect: Rect, actions: &mut Vec<Action>) {
    let mut y = rect.min.y + 18.0;
    let center = rect.center().x;
    if !vm.connection.is_connected() {
        let width = 460.0f32.min(rect.width() - 48.0);
        let area = Rect::from_center_size(Pos2::new(center, y + 22.0), Vec2::new(width, 44.0));
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt("banner-down")
                .max_rect(area)
                .layout(Layout::left_to_right(Align::Center)),
        );
        child.painter().rect(
            area,
            egui::CornerRadius::same(22),
            pal.card,
            Stroke::new(1.0, pal.accent),
            StrokeKind::Inside,
        );
        child.add_space(16.0);
        widgets::text(
            &mut child,
            "Служба не запущена",
            13.0,
            fam::sans_medium(),
            pal.ink,
        );
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(4.0);
            ui.spacing_mut().item_spacing.x = 4.0;
            let starting = vm.settings.service.restart.is_some();
            act(
                ui,
                pal,
                "Запустить",
                Kind::Hot,
                !starting,
                actions,
                || Action::StartCore,
            );
            act(ui, pal, "Ещё раз", Kind::Ghost, true, actions, || {
                Action::ConnectAgain
            });
        });
        y += 54.0;
    }
    if let Some(outcome) = &vm.outcome {
        let galley =
            ui.painter()
                .layout_no_wrap(outcome.line(), font(12.0, fam::sans_medium()), pal.ink);
        let size = galley.size() + Vec2::new(44.0, 16.0);
        let pill = Rect::from_min_size(Pos2::new(center - size.x / 2.0, y), size);
        let edge = if outcome.ok { pal.ok } else { pal.accent };
        let response = ui
            .interact(pill, egui::Id::new("outcome"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let painter = ui.painter();
        painter.rect(
            pill,
            egui::CornerRadius::same((size.y / 2.0) as u8),
            pal.card,
            Stroke::new(1.0, edge),
            StrokeKind::Inside,
        );
        painter.galley(pill.min + Vec2::new(16.0, 8.0), galley, pal.ink);
        widgets::paint_icon(
            painter,
            widgets::Icon::Close,
            Pos2::new(pill.max.x - 14.0, pill.center().y),
            9.0,
            pal.ink2,
        );
        if response.clicked() {
            actions.push(Action::DismissOutcome);
        }
    }
}

/// The legend on the left and the three actions on the right.
#[allow(clippy::too_many_arguments)]
fn footer(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    model: &MapModel,
    rect: Rect,
    t: f32,
    actions: &mut Vec<Action>,
) {
    let area = Rect::from_min_max(
        Pos2::new(rect.min.x + 24.0, rect.max.y - 16.0 - 52.0),
        Pos2::new(rect.max.x - 24.0, rect.max.y - 16.0),
    );
    let mut bar = ui.new_child(
        UiBuilder::new()
            .id_salt("map-foot")
            .max_rect(area)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let bar = &mut bar;
    bar.set_clip_rect(area.expand2(Vec2::new(0.0, 40.0)));
    let transfer = vm.transfer();
    let dirty = model.dirty;
    let editable = vm.editor.editable && vm.connection.is_connected();
    egui::Frame::NONE
        .fill(pal.card)
        .stroke(pal.hairline())
        .corner_radius(pal.rad_btn(52.0))
        .inner_margin(egui::Margin::same(4))
        .shadow(egui::epaint::Shadow {
            offset: [0, 10],
            blur: 30,
            spread: 0,
            color: shadow_color(pal, 0.07),
        })
        .show(bar, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.horizontal(|ui| {
                let can_auto = vm.connection.is_connected();
                act(ui, pal, "Авто", Kind::Ghost, can_auto, actions, || {
                    Action::LayoutAuto
                });
                let apply = widgets::button(
                    ui,
                    pal,
                    "Применить",
                    if dirty { Kind::Solid } else { Kind::Ghost },
                    dirty && editable,
                );
                if apply.clicked() {
                    if let Some(action) = vm.layout_action() {
                        actions.push(action);
                    }
                }
                let label = if transfer.target.is_empty() {
                    "↔ Передать ввод".to_owned()
                } else {
                    format!("↔ Передать ввод → {}", transfer.target)
                };
                let go = widgets::button(ui, pal, &label, Kind::Hot, transfer.enabled);
                if let Some(why) = &transfer.why {
                    go.clone().on_hover_text(why);
                }
                let from = vm
                    .status
                    .as_ref()
                    .map(|_| vm.route_node_id())
                    .unwrap_or_default();
                if go.hovered() {
                    if let (Some(peer), true) = (&transfer.peer, transfer.enabled) {
                        st.ribbon = Some((from.clone(), peer.clone()));
                        st.ribbon_hover = true;
                    }
                } else {
                    st.ribbon_hover = false;
                }
                if go.clicked() {
                    if let Some(peer) = &transfer.peer {
                        st.ribbon = Some((from, peer.clone()));
                        st.ribbon_until = t + RIBBON_FOR;
                        actions.push(Action::SwitchTo(peer.clone()));
                    }
                }
            });
        });
    bar.add_space(12.0);
    // The legend, right to left, so it reads in order from the left edge.
    bar.spacing_mut().item_spacing.x = 6.0;
    let items: [(&str, LegendMark); 4] = [
        ("Тяните компьютер — края примагнитятся", LegendMark::None),
        ("Размер приблизительный", LegendMark::Approx),
        ("Сюда идёт ввод", LegendMark::Target),
        ("Связь краёв", LegendMark::Link),
    ];
    for (label, mark) in items {
        legend_pill(bar, pal, label, mark);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LegendMark {
    None,
    Link,
    Target,
    Approx,
}

/// `.lg`: 28 px high, a small sample of what it explains, then the words.
fn legend_pill(ui: &mut egui::Ui, pal: &Palette, label: &str, mark: LegendMark) {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font(10.5, fam::mono()), pal.ink2);
    let sample = match mark {
        LegendMark::None => 0.0,
        LegendMark::Link => 16.0 + 8.0,
        LegendMark::Target | LegendMark::Approx => 12.0 + 8.0,
    };
    let size = Vec2::new(galley.size().x + sample + 22.0, 28.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect(
        rect,
        egui::CornerRadius::same(14),
        pal.card_glass,
        pal.hairline(),
        StrokeKind::Inside,
    );
    let mut x = rect.min.x + 11.0;
    let cy = rect.center().y;
    match mark {
        LegendMark::None => {}
        LegendMark::Link => {
            painter.add(Shape::dashed_line(
                &[Pos2::new(x, cy), Pos2::new(x + 16.0, cy)],
                Stroke::new(1.5, pal.accent),
                4.0,
                3.0,
            ));
        }
        LegendMark::Target => {
            let r = Rect::from_center_size(Pos2::new(x + 6.0, cy), Vec2::new(12.0, 9.0));
            painter.rect_stroke(
                r.expand(1.5),
                4.0,
                Stroke::new(1.5, pal.accent_soft),
                StrokeKind::Outside,
            );
            painter.rect_stroke(r, 3.0, Stroke::new(1.5, pal.accent), StrokeKind::Inside);
        }
        LegendMark::Approx => {
            let r = Rect::from_center_size(Pos2::new(x + 6.0, cy), Vec2::new(12.0, 9.0));
            dashed_rect(painter, r, Stroke::new(1.5, pal.ink2));
        }
    }
    x += sample;
    painter.add(TextShape::new(
        Pos2::new(x, cy - galley.size().y / 2.0),
        galley,
        pal.ink2,
    ));
}
