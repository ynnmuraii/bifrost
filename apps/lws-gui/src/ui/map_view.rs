//! The monitor map, draggable computers, links and quiet placement feedback.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::field::{Flow, Frame};
use super::theme::{fam, font, rgba, Palette};
use super::widgets::{self, Kind};
use super::{act, Env, UiState, FIELD_FRAME};
use crate::gui_settings::{FieldMode, LinkStyle, Look, PAN_MAX};
use crate::map::{Dir, STAGE_H, STAGE_W};
use crate::view_model::layout::{LinkState, MapLink, MapModel, MapNode};
use crate::view_model::{Action, ViewModel};

/// Stage → screen.
#[derive(Clone, Copy)]
struct Xf {
    origin: Pos2,
    s: f32,
}

impl Xf {
    fn new(rect: Rect, zoom_percent: u16) -> Self {
        let base = (((rect.width() - 48.0) / STAGE_W as f32)
            .min((rect.height() - 190.0) / STAGE_H as f32))
        .clamp(0.45, 1.0);
        let s = base * f32::from(zoom_percent.clamp(70, 150)) / 100.0;
        Self {
            origin: rect.center() - Vec2::new(STAGE_W as f32, STAGE_H as f32) * (s / 2.0),
            s,
        }
    }

    fn stage_delta(self, delta: Vec2) -> crate::map::Pt {
        crate::map::Pt {
            x: f64::from(delta.x / self.s),
            y: f64::from(delta.y / self.s),
        }
    }

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
    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| rect.contains(*p) && vm.modal.is_none() && !vm.appearance_open);
    let animate = st.field.active(
        pointer,
        t,
        env.animate && vm.modal.is_none() && !vm.appearance_open && vm.look.field != FieldMode::Off,
    );
    let mut xf = Xf::new(rect, vm.look.zoom_percent);
    xf.origin += pan(ui, vm, rect, actions);
    let s = xf.s;

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
        },
    );

    header(&painter, pal, vm, rect);
    zoom_controls(ui, pal, vm, st, rect, actions);

    for link in &model.links {
        draw_link(
            &painter,
            pal,
            &model,
            link,
            &shown,
            xf,
            vm.look.link_style,
            &mut st.field,
        );
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
            vm.modal.is_none() && !vm.appearance_open,
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

    if st.drag.is_some() {
        let (released, down, focused) = ui.input(|i| {
            (
                i.pointer.button_released(egui::PointerButton::Primary),
                i.pointer.button_down(egui::PointerButton::Primary),
                i.focused,
            )
        });
        if released && focused && vm.modal.is_none() && !vm.appearance_open {
            if let Some(drag) = st.drag.take() {
                actions.push(Action::DropNode(drag.node_id));
            }
        } else if !down || !focused || vm.modal.is_some() || vm.appearance_open {
            st.drag = None;
            actions.push(Action::CancelDrag);
        }
    }
    banner(ui, pal, vm, rect, actions);
    footer(ui, pal, rect);
    animate.then_some(FIELD_FRAME)
}

fn pan(ui: &egui::Ui, vm: &ViewModel, rect: Rect, actions: &mut Vec<Action>) -> Vec2 {
    let id = egui::Id::new("map-pan");
    let mut drag = ui.ctx().data_mut(|d| d.get_temp::<(Pos2, Vec2)>(id));
    let (pressed, down, released, pointer, focused) = ui.input(|i| {
        (
            i.pointer.button_pressed(egui::PointerButton::Secondary),
            i.pointer.button_down(egui::PointerButton::Secondary),
            i.pointer.button_released(egui::PointerButton::Secondary),
            i.pointer.interact_pos(),
            i.focused,
        )
    });
    let saved = Vec2::new(f32::from(vm.look.pan_x), f32::from(vm.look.pan_y));
    let enabled = vm.modal.is_none() && !vm.appearance_open && focused;
    if pressed && enabled {
        if let Some(p) = pointer.filter(|p| rect.contains(*p)) {
            drag = Some((p, saved));
        }
    }
    let mut offset = saved;
    if let Some((start, origin)) = drag {
        if let Some(p) = pointer {
            offset = (origin + (p - start)).clamp(
                Vec2::splat(-f32::from(PAN_MAX)),
                Vec2::splat(f32::from(PAN_MAX)),
            );
        }
        if released && enabled {
            let look = Look {
                pan_x: offset.x.round() as i16,
                pan_y: offset.y.round() as i16,
                ..vm.look
            };
            if look != vm.look {
                actions.push(Action::SetLook(look));
            }
        }
        if !down || !enabled {
            drag = None;
            if !released || !enabled {
                offset = saved;
            }
        } else {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }
    ui.ctx().data_mut(|d| {
        if let Some(drag) = drag {
            d.insert_temp(id, drag);
        } else {
            d.remove::<(Pos2, Vec2)>(id);
        }
    });
    offset
}

fn zoom_controls(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &UiState,
    rect: Rect,
    actions: &mut Vec<Action>,
) {
    let current = vm.look.zoom_percent.clamp(70, 150);
    let mut wanted = current;
    let mut controls = ui.new_child(
        UiBuilder::new()
            .id_salt("map-zoom")
            .max_rect(Rect::from_min_size(
                rect.min + Vec2::new(24.0, 94.0),
                Vec2::new(180.0, 36.0),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let enabled = vm.modal.is_none() && !vm.appearance_open && st.drag.is_none();
    if widgets::button(
        &mut controls,
        pal,
        "−",
        Kind::Ghost,
        enabled && current > 70,
    )
    .clicked()
    {
        wanted = current.saturating_sub(5).max(70);
    }
    widgets::text(
        &mut controls,
        &format!("{current}%"),
        12.0,
        fam::mono(),
        pal.ink2,
    );
    if widgets::button(
        &mut controls,
        pal,
        "+",
        Kind::Ghost,
        enabled && current < 150,
    )
    .clicked()
    {
        wanted = (current + 5).min(150);
    }
    if enabled {
        let wheel = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .filter(|point| rect.contains(*point))
                .map_or(0.0, |_| {
                    input
                        .raw
                        .events
                        .iter()
                        .filter_map(|event| match event {
                            egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                            _ => None,
                        })
                        .sum::<f32>()
                })
        });
        if wheel > 0.0 {
            wanted = (current + 5).min(150);
        } else if wheel < 0.0 {
            wanted = current.saturating_sub(5).max(70);
        }
    }
    if wanted != current {
        actions.push(Action::SetLook(Look {
            zoom_percent: wanted,
            ..vm.look
        }));
    }
}

/// The map heading and monitor count.
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

#[allow(clippy::too_many_arguments)]
fn draw_link(
    painter: &egui::Painter,
    pal: &Palette,
    model: &MapModel,
    link: &MapLink,
    shown: &[Pos2],
    xf: Xf,
    style: LinkStyle,
    field: &mut super::field::Field,
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
    if style == LinkStyle::Ascii {
        field.link(painter, a, b, color);
    } else {
        painter.add(Shape::dashed_line(
            &[a, b],
            Stroke::new(1.5, color),
            5.0,
            3.5,
        ));
    }
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
    let box_left = |index: usize, carry: Vec2| {
        xf.p(model.nodes[index].at.x, model.nodes[index].at.y).x + carry.x
    };
    let pill = link_pill(
        horizontal,
        mid,
        size,
        box_left(link.a, carry_a).min(box_left(link.b, carry_b)),
    );
    painter.rect(
        pill,
        egui::CornerRadius::same((size.y / 2.0) as u8),
        pal.card_glass,
        Stroke::new(1.0, if active { pal.accent } else { pal.hair }),
        StrokeKind::Inside,
    );
    painter.galley(pill.min + Vec2::new(8.0, 2.0), galley, pal.ink2);
}

/// Where a link's label pill goes.
///
/// A side-by-side link's label sits above the line's midpoint. A stacked link's label sits
/// beside the line, to the **left** of the pair's boxes, and that is the one place it can
/// never cover anything: a computer's caption is left-aligned at its box's left edge, so every
/// caption, however wide it is, starts to the right of this rect.
fn link_pill(horizontal: bool, mid: Pos2, size: Vec2, boxes_left: f32) -> Rect {
    if horizontal {
        Rect::from_min_size(Pos2::new(mid.x - size.x / 2.0, mid.y - 7.0 - size.y), size)
    } else {
        Rect::from_min_size(
            Pos2::new(boxes_left - 8.0 - size.x, mid.y - size.y / 2.0),
            size,
        )
    }
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
    enabled: bool,
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
        let response = ui.interact(
            rect,
            id,
            if enabled {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            },
        );
        if enabled && (response.hovered() || response.dragged()) {
            ui.ctx().set_cursor_icon(if response.dragged() {
                egui::CursorIcon::Grabbing
            } else {
                egui::CursorIcon::Grab
            });
        }
        if enabled && response.has_focus() {
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
        if enabled && response.drag_started_by(egui::PointerButton::Primary) {
            if let Some(grab) = response.interact_pointer_pos() {
                st.drag = Some(super::Drag {
                    node_id: node.node_id.clone(),
                    start: node.at,
                    grab,
                });
                st.field.release();
            }
        }
        if enabled && response.dragged_by(egui::PointerButton::Primary) {
            if let (Some(drag), Some(p)) = (&st.drag, response.interact_pointer_pos()) {
                if drag.node_id == node.node_id {
                    let delta = xf.stage_delta(p - drag.grab);
                    actions.push(Action::MoveNode {
                        node_id: node.node_id.clone(),
                        x: drag.start.x + delta.x,
                        y: drag.start.y + delta.y,
                    });
                }
            }
        }
        if enabled && response.hovered() && st.drag.is_none() {
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
    painter.circle_filled(
        Pos2::new(at.x + 3.0 * fs, y + h / 2.0),
        3.0 * fs,
        if node.online { pal.ok } else { pal.ink2 },
    );
    painter.galley(Pos2::new(at.x + 12.0 * fs, y), name, pal.ink);
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
        let galley = ui.painter().layout(
            outcome.line(),
            font(14.0, fam::sans_medium()),
            pal.ink,
            (rect.width() - 92.0).max(40.0),
        );
        let size = galley.size() + Vec2::new(52.0, 24.0);
        let pill = Rect::from_min_size(Pos2::new(center - size.x / 2.0, y), size);
        let edge = if outcome.ok { pal.ok } else { pal.accent };
        let response = ui
            .interact(pill, egui::Id::new("outcome"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let painter = ui.painter();
        painter.rect(
            pill,
            egui::CornerRadius::same(12),
            pal.card,
            Stroke::new(1.0, edge),
            StrokeKind::Inside,
        );
        painter.galley(pill.min + Vec2::new(16.0, 12.0), galley, pal.ink);
        widgets::paint_icon(
            painter,
            widgets::Icon::Close,
            Pos2::new(pill.max.x - 14.0, pill.center().y),
            9.0,
            pal.ink2,
        );
        let id = egui::Id::new("toast-held");
        let held = ui
            .ctx()
            .data_mut(|d| d.get_temp::<bool>(id).unwrap_or(false));
        if held != response.hovered() {
            actions.push(Action::HoldToast(response.hovered()));
            ui.ctx().data_mut(|d| d.insert_temp(id, response.hovered()));
        }
        if response.clicked() {
            actions.push(Action::DismissOutcome);
        }
    } else {
        let id = egui::Id::new("toast-held");
        if ui
            .ctx()
            .data_mut(|d| d.get_temp::<bool>(id).unwrap_or(false))
        {
            actions.push(Action::HoldToast(false));
        }
        ui.ctx().data_mut(|d| d.remove::<bool>(id));
        if let Some(flash) = &vm.flash {
            let galley = ui.painter().layout(
                flash.text.clone(),
                font(12.0, fam::sans_medium()),
                pal.ink2,
                (rect.width() - 48.0).max(40.0),
            );
            ui.painter().galley(
                Pos2::new(center - galley.size().x / 2.0, y),
                galley,
                pal.ink2,
            );
        }
    }
}

/// Text-only legend; placement is saved on a valid drop.
fn footer(ui: &mut egui::Ui, pal: &Palette, rect: Rect) {
    let area = Rect::from_min_max(
        Pos2::new(rect.min.x + 24.0, rect.max.y - 48.0),
        Pos2::new(rect.max.x - 24.0, rect.max.y - 12.0),
    );
    let mut bar = ui.new_child(UiBuilder::new().id_salt("map-foot").max_rect(area));
    bar.horizontal_wrapped(|ui| {
        for label in [
            "Связь краёв",
            "Сюда идёт ввод",
            "Размер приблизительный",
            "Тяните компьютер — края примагнитятся",
        ] {
            egui::Frame::NONE
                .fill(pal.card_glass)
                .stroke(pal.hairline())
                .corner_radius(14)
                .inner_margin(egui::Margin::symmetric(11, 6))
                .show(ui, |ui| {
                    widgets::text(ui, label, 10.5, fam::mono(), pal.ink2);
                });
        }
    });
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    /// The caption of a computer is left-aligned at its box's left edge and can be wider than
    /// the box itself, which is what made a stacked link's label collide with it.
    fn caption(box_rect: Rect) -> Rect {
        Rect::from_min_size(
            Pos2::new(box_rect.min.x, box_rect.max.y + 10.0),
            Vec2::new(box_rect.width() + 64.0, 44.0),
        )
    }

    #[test]
    fn link_labels_never_cover_a_caption_or_a_box() {
        let size = Vec2::new(72.0, 18.0);
        let box_size = Vec2::new(144.0, 96.0);
        let upper = Rect::from_min_size(Pos2::new(320.0, 120.0), box_size);
        let lower = Rect::from_min_size(Pos2::new(320.0, 282.0), box_size);
        // A stacked pair: the label goes to the left of both boxes, clear of every caption.
        let mid = Pos2::new(
            (upper.center().x + lower.center().x) / 2.0,
            (upper.max.y + lower.min.y) / 2.0,
        );
        let pill = link_pill(false, mid, size, upper.min.x.min(lower.min.x));
        for rect in [upper, lower, caption(upper), caption(lower)] {
            assert!(!pill.intersects(rect), "{pill:?} must not touch {rect:?}");
        }
        assert!(pill.max.x <= upper.min.x, "the label is outside the boxes");
        // A side-by-side pair: the label sits above the line, clear of both captions, which
        // are below their boxes.
        let left = Rect::from_min_size(Pos2::new(120.0, 200.0), box_size);
        let right = Rect::from_min_size(Pos2::new(384.0, 200.0), box_size);
        let mid = Pos2::new((left.max.x + right.min.x) / 2.0, left.center().y);
        let pill = link_pill(true, mid, size, left.min.x.min(right.min.x));
        assert_eq!(pill.center().x, mid.x);
        assert!(pill.max.y <= mid.y - 7.0);
        for rect in [left, right, caption(left), caption(right)] {
            assert!(!pill.intersects(rect), "{pill:?} must not touch {rect:?}");
        }
    }

    #[test]
    fn zoomed_drag_deltas_round_trip_without_changing_stage_coordinates() {
        for size in [Vec2::new(1200.0, 520.0), Vec2::new(900.0, 360.0)] {
            let rect = Rect::from_min_size(Pos2::new(40.0, 70.0), size);
            for zoom in [70, 100, 125, 150] {
                let xf = Xf::new(rect, zoom);
                let from = xf.p(120.0, -20.0);
                let to = xf.p(215.0, 140.0);
                let delta = xf.stage_delta(to - from);
                assert!((delta.x - 95.0).abs() < 0.001, "{zoom}%");
                assert!((delta.y - 160.0).abs() < 0.001, "{zoom}%");
                assert!(xf.p(STAGE_W / 2.0, STAGE_H / 2.0).distance(rect.center()) < 0.001);
            }
        }
    }
}
