//! The monitor map, draggable computers, links and quiet placement feedback.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::field::{Flow, Frame, Ribbon};
use super::theme::{fam, font, rgba, Palette};
use super::widgets::{self, Kind};
use super::{Env, UiState, FIELD_FRAME};
use crate::gui_settings::{LinkStyle, Look, PAN_MAX};
use crate::map::{Dir, STAGE_H, STAGE_W};
use crate::view_model::layout::{LinkState, MapLink, MapModel, MapNode};
use crate::view_model::{Action, ViewModel};
use ui_text::{text, Key};

/// How far the ribbon of «ASCII-изолиния» arches above the midpoint of a link, in screen
/// points per unit of zoom — the mockup's own 110 (`docs/design/mockup/Main.dc.html`), which
/// is the stage's scale to the window, not the map's own scale of the virtual coordinates.
const RIBBON_ARCH: f32 = 110.0;
/// The flattest the arch may ever be: a ribbon hugging the cards reads as a scratch.
const RIBBON_ARCH_MIN: f32 = 60.0;

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
    let lang = vm.look.language;
    let painter = ui.painter().with_clip_rect(rect);
    let model = vm.map_model();
    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| rect.contains(*p) && vm.modal.is_none() && !vm.appearance_open);
    // The pointer is what attracts or repels the particles, and it holds them while it stays:
    // the field simulates for as long as it is over the map, and it is the field that says
    // whether it is still moving, so a settled field stops the frames and keeps its state.
    let simulate = pointer.is_some() && env.animate && vm.modal.is_none() && !vm.appearance_open;
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

    // The ribbon of «ASCII-изолиния» arches in screen points, scaled by the map's zoom and
    // never flatter than the mockup's floor.
    let arch = (RIBBON_ARCH * f32::from(vm.look.zoom_percent) / 100.0).max(RIBBON_ARCH_MIN);
    let card = |n: usize| {
        Rect::from_min_size(
            shown[n],
            Vec2::new(model.nodes[n].w as f32 * s, model.nodes[n].h as f32 * s),
        )
    };
    let flows: Vec<Flow> = model
        .links
        .iter()
        .filter(|l| l.state == LinkState::Active)
        .map(|l| {
            let (a, b) = link_ends(&model, l, &shown, xf);
            let (from, to) = if l.forward { (a, b) } else { (b, a) };
            let arc = ribbon_arc(card(l.a), card(l.b), arch);
            let arc = if l.forward {
                arc
            } else {
                Ribbon {
                    a: arc.b,
                    c: arc.c,
                    b: arc.a,
                }
            };
            Flow {
                from,
                to,
                horizontal: l.dir == Dir::Horizontal,
                arc,
            }
        })
        .collect();

    // Hover identity may stay the same while the card glides, zooms or pans.
    // Retarget before painting so the outline uses this frame's displayed geometry.
    if simulate && st.drag.is_none() {
        if let Some((n, m)) = st.hover {
            if let Some(mon) = model.nodes.get(n).and_then(|node| node.mons.get(m)) {
                let local = monitor_rect(shown[n], mon.rect, s).translate(-rect.min.to_vec2());
                st.field.retarget_form(local);
            }
        }
    }

    let moving = st.field.paint(
        &painter,
        &Frame {
            map: rect,
            pal,
            look: vm.look,
            simulate,
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
    if simulate && hovered != st.hover {
        match hovered {
            Some((n, m)) if simulate && st.drag.is_none() => {
                let node = &model.nodes[n];
                let r = node.mons[m].rect;
                let local = monitor_rect(shown[n], r, s).translate(-rect.min.to_vec2());
                st.field.form(local);
            }
            _ => st.field.release(),
        }
        st.hover = hovered;
    }
    if model.nodes.is_empty() {
        let galley = painter.layout_no_wrap(
            if vm.connection.is_connected() {
                text(lang, Key::TheLayoutHasNotArrivedYet).to_owned()
            } else {
                text(lang, Key::NoDataFromTheService).to_owned()
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
    // The field is the only thing here that moves by itself, and it says when it has stopped:
    // a settled field draws its converged particles and asks for no further frames.
    moving.then_some(FIELD_FRAME)
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
    let lang = vm.look.language;
    let left = rect.min + Vec2::new(24.0, 20.0);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text(lang, Key::K01MONITORMAP),
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
    let (mut color, mut alpha) = (pal.ink2, 1.0f32);
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
                xf.p(link.from.x + 4.0, s0),
                xf.p(link.from.x + 4.0, s1),
                xf.p(link.to.x - 4.0, s0),
                xf.p(link.to.x - 4.0, s1),
            )
        } else {
            (
                xf.p(s0, link.from.y + 4.0),
                xf.p(s1, link.from.y + 4.0),
                xf.p(s0, link.to.y - 4.0),
                xf.p(s1, link.to.y - 4.0),
            )
        };
        let (p0, p1, q0, q1) = (p0 + carry_a, p1 + carry_a, q0 + carry_b, q1 + carry_b);
        let rail = Stroke::new(
            3.0,
            fade(
                if link.state == LinkState::Active {
                    pal.accent
                } else {
                    pal.ink2
                },
                alpha * 0.6,
            ),
        );
        painter.line_segment([p0, p1], rail);
        painter.line_segment([q0, q1], rail);
    }
}

fn fade(c: Color32, k: f32) -> Color32 {
    // Color32 stores premultiplied channels; do not premultiply them a second time.
    c.gamma_multiply(k)
}

/// The ribbon of one link: it leaves the edge of the first card facing the second, arrives on
/// the edge of the second, and arches `arch` points over the midpoint between the two.
fn ribbon_arc(first: Rect, second: Rect, arch: f32) -> Ribbon {
    let edge = |from: Rect, to: Rect| {
        let (centre, delta) = (from.center(), to.center() - from.center());
        let half = from.size() / 2.0;
        let tx = if delta.x.abs() > f32::EPSILON {
            half.x / delta.x.abs()
        } else {
            f32::INFINITY
        };
        let ty = if delta.y.abs() > f32::EPSILON {
            half.y / delta.y.abs()
        } else {
            f32::INFINITY
        };
        centre + delta * tx.min(ty).min(1.0)
    };
    let (a, b) = (edge(first, second), edge(second, first));
    Ribbon {
        a,
        c: Pos2::new((a.x + b.x) / 2.0, a.y.min(b.y) - arch),
        b,
    }
}

fn monitor_rect(at: Pos2, r: crate::map::Rect, s: f32) -> Rect {
    Rect::from_min_size(
        at + Vec2::new(r.x as f32 * s, r.y as f32 * s),
        Vec2::new(r.w as f32 * s, r.h as f32 * s),
    )
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
        let rect = monitor_rect(at, mon.rect, s);
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
        (
            node.focus.as_str(),
            if node.target { pal.accent } else { pal.ink2 },
            fam::mono_medium(),
        ),
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
    let lang = vm.look.language;
    let mut y = rect.min.y + 18.0;
    let center = rect.center().x;
    if !vm.connection.is_connected() {
        let start = vm.service_start();
        let waiting = vm.waiting_for_core();
        let width = 460.0f32.min(rect.width() - 48.0);
        let area = Rect::from_center_size(Pos2::new(center, y + 24.0), Vec2::new(width, 48.0));
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt("banner-down")
                .max_rect(area)
                .layout(Layout::left_to_right(Align::Center)),
        );
        child.painter().rect(
            area,
            egui::CornerRadius::same(24),
            pal.card,
            Stroke::new(1.0, pal.accent),
            StrokeKind::Inside,
        );
        child.add_space(18.0);
        widgets::text(
            &mut child,
            text(lang, Key::TheServiceIsNotRunning),
            13.0,
            fam::sans_medium(),
            pal.ink,
        );
        if waiting {
            // A bar along the banner's foot: the window is waiting for a core it asked for.
            let track = Rect::from_min_max(
                Pos2::new(area.left() + 18.0, area.bottom() - 9.0),
                Pos2::new(area.right() - 18.0, area.bottom() - 6.0),
            );
            let painter = child.painter();
            painter.rect_filled(
                track,
                egui::CornerRadius::same(2),
                rgba(pal.accent.r(), pal.accent.g(), pal.accent.b(), 0.25),
            );
            let span = track.width() * 0.3;
            let phase = (ui.input(|input| input.time) as f32 * 0.7).fract();
            let bar = Rect::from_min_size(
                Pos2::new(track.left() + (track.width() - span) * phase, track.top()),
                Vec2::new(span, track.height()),
            );
            painter.rect_filled(bar, egui::CornerRadius::same(2), pal.accent);
        }
        // One button, whatever the banner is saying: «Запустить», and «Ещё раз» once a start
        // has failed. It is off while a start is in flight.
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(18.0);
            let label = text(lang, start.button());
            if widgets::notice_button(ui, pal, label, Kind::Hot, !waiting).clicked() {
                actions.push(Action::StartCore);
            }
        });
        y += 54.0;
        if let Some(reason) = start.reason() {
            let galley = ui.painter().layout(
                reason.to_owned(),
                font(13.0, fam::sans()),
                pal.ink2,
                (rect.width() - 92.0).max(40.0),
            );
            let size = galley.size();
            ui.painter()
                .galley(Pos2::new(center - size.x / 2.0, y + 6.0), galley, pal.ink2);
            y += size.y + 12.0;
        }
    }
    if let Some(outcome) = &vm.outcome {
        let galley = ui.painter().layout(
            outcome.line(lang),
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

#[cfg(test)]
mod transform_tests {
    use super::*;

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

    #[test]
    fn the_ribbon_starts_and_ends_on_the_card_edges_under_a_high_arch() {
        let first = Rect::from_min_size(Pos2::new(0.0, 100.0), Vec2::new(200.0, 120.0));
        let second = Rect::from_min_size(Pos2::new(400.0, 200.0), Vec2::new(160.0, 90.0));
        let arch = RIBBON_ARCH.max(RIBBON_ARCH_MIN);
        let ribbon = ribbon_arc(first, second, arch);
        // The ends are on the edges, facing each other.
        assert!((ribbon.a.x - first.max.x).abs() < 0.001, "{ribbon:?}");
        assert!(
            ribbon.a.y > first.min.y && ribbon.a.y < first.max.y,
            "{ribbon:?}"
        );
        assert!((ribbon.b.x - second.min.x).abs() < 0.001, "{ribbon:?}");
        assert!(
            ribbon.b.y > second.min.y && ribbon.b.y < second.max.y,
            "{ribbon:?}"
        );
        // The arch is over the midpoint between the ends, as high as asked and never flat.
        assert!((ribbon.c.x - (ribbon.a.x + ribbon.b.x) / 2.0).abs() < 0.001);
        let rise = ribbon.a.y.min(ribbon.b.y) - ribbon.c.y;
        assert!((rise - arch).abs() < 0.001, "{rise}");
        assert!(rise >= 60.0);
        // And it passes above both cards rather than through them.
        assert!(
            ribbon.c.y < first.min.y && ribbon.c.y < second.min.y,
            "{ribbon:?}"
        );
    }

    fn owner_layout() -> MapModel {
        use crate::map::{self, DocLink, Rect as MapRect};
        use crate::view_model::{layout::MapMonitor, Side};
        let arrangement = map::arrange(
            &[
                vec![
                    MapRect::new(0.0, 0.0, 1920.0, 1080.0),
                    MapRect::new(1920.0, -247.0, 1080.0, 1920.0),
                ],
                vec![MapRect::new(0.0, 0.0, 1745.0, 1091.0)],
            ],
            &[DocLink {
                a: 0,
                side: Side::Left,
                b: 1,
            }],
            0,
        );
        let nodes = arrangement
            .shapes
            .iter()
            .enumerate()
            .map(|(i, shape)| MapNode {
                node_id: i.to_string(),
                name: ["WORKSTATION", "LAPTOP"][i].to_owned(),
                os: String::new(),
                focus: String::new(),
                online: true,
                target: i == 0,
                approx: i == 1,
                at: arrangement.pos[i],
                w: shape.w,
                h: shape.h,
                mons: shape
                    .mons
                    .iter()
                    .map(|&rect| MapMonitor {
                        rect,
                        num: String::new(),
                        res: String::new(),
                        tag: "",
                    })
                    .collect(),
            })
            .collect();
        let links = map::links(&arrangement.shapes, &arrangement.pos)
            .into_iter()
            .map(|l| MapLink {
                a: l.a,
                b: l.b,
                from: l.from,
                to: l.to,
                dir: l.dir,
                span: l.span,
                state: LinkState::Active,
                whole_desktop: true,
                forward: false,
            })
            .collect();
        MapModel {
            nodes,
            links,
            screens: 3,
            dirty: false,
        }
    }

    #[test]
    fn separated_owner_desktops_have_visible_links_and_external_rails_in_both_styles() {
        let model = owner_layout();
        assert_eq!(model.links.len(), 1);
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let pal = Palette::of(crate::gui_settings::Theme::Dark);
        for style in [LinkStyle::Ascii, LinkStyle::Line] {
            for state in [LinkState::Active, LinkState::Idle] {
                let mut link = model.links[0].clone();
                link.state = state;
                let xf = Xf::new(
                    Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 708.0)),
                    150,
                );
                let shown: Vec<_> = model.nodes.iter().map(|n| xf.p(n.at.x, n.at.y)).collect();
                let (a, b) = link_ends(&model, &link, &shown, xf);
                eprintln!("owner layout {style:?} {state:?}: link {a:?} -> {b:?}");
                assert!(b.x - a.x > 30.0);
                let mut field = super::super::field::Field::new();
                let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    draw_link(
                        ui.painter(),
                        &pal,
                        &model,
                        &link,
                        &shown,
                        xf,
                        style,
                        &mut field,
                    );
                });
                let mut rails = 0;
                let mut chain = 0;
                fn inspect(shape: &Shape, a: Pos2, b: Pos2, rails: &mut usize, chain: &mut usize) {
                    match shape {
                        Shape::Vec(shapes) => {
                            for s in shapes {
                                inspect(s, a, b, rails, chain);
                            }
                        }
                        Shape::LineSegment { points, stroke } => {
                            eprintln!("line {points:?} {stroke:?}");
                            assert!(stroke.color.a() > 100);
                            if stroke.width == 3.0 {
                                assert!(points.iter().all(|p| p.x > a.x && p.x < b.x));
                                *rails += 1;
                            } else {
                                *chain += 1;
                            }
                        }
                        Shape::Text(text) => {
                            eprintln!("glyph {:?} at {:?}", text.galley.text(), text.pos);
                            let center = text.pos + text.galley.size() / 2.0;
                            if center.x > a.x && center.x < b.x {
                                *chain += 1;
                            }
                        }
                        _ => {}
                    }
                }
                for clipped in &output.shapes {
                    inspect(&clipped.shape, a, b, &mut rails, &mut chain);
                }
                assert_eq!(rails, 2, "{style:?} {state:?}");
                assert!(chain > 2, "{style:?} {state:?}");
                output.drop_without_applying_deltas();
            }
        }
    }

    #[test]
    fn repeated_draw_frames_carry_the_same_hover_outline_with_the_animated_card() {
        use crate::map::{Pt, Rect as MapRect, Shape as MapShape};
        use crate::view_model::{CoreEndpoint, EditorDisplay, EditorNode, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        vm.look.clouds = false;
        vm.look.pan_x = 0;
        vm.look.pan_y = 0;
        vm.look.zoom_percent = 100;
        let mon = MapRect::new(0.0, 0.0, 200.0, 100.0);
        vm.editor.nodes = vec![EditorNode {
            node_id: "01".repeat(32),
            display_name: "notebook".to_owned(),
            online: true,
            displays: vec![EditorDisplay {
                id: "zone-0".to_owned(),
                x: 0.0,
                y: 0.0,
                w: 1745.0,
                h: 1091.0,
                primary: true,
                native_known: false,
                rotation: 0,
            }],
        }];
        vm.editor.shapes = vec![MapShape {
            mons: vec![mon],
            w: mon.w,
            h: mon.h,
        }];
        vm.editor.placement = vec![Pt::new(300.0, 100.0)];
        vm.editor.baseline = vm.editor.placement.clone();
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let now = std::time::Instant::now();
        let mut st = UiState::new(now);
        let pal = Palette::of(vm.look.theme);
        let map = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 708.0));
        let xf = Xf::new(map, vm.look.zoom_percent);
        // Remains inside the card throughout its 40-stage-unit glide.
        let pointer = xf.p(400.0, 150.0);
        let card_id = egui::Id::new(("mon", &vm.editor.nodes[0].node_id, 0usize));
        let mut initial_outline = None;
        for frame in 0..12 {
            if frame == 4 {
                vm.editor.placement[0].x += 40.0;
            }
            let t = frame as f32 / 30.0;
            let mut painted = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(map),
                    time: Some(f64::from(t)),
                    events: vec![egui::Event::PointerMoved(pointer)],
                    ..Default::default()
                },
                |ui| {
                    let _ = draw(
                        ui,
                        &pal,
                        &vm,
                        &mut st,
                        map,
                        Env { now, animate: true },
                        t,
                        &mut Vec::new(),
                    );
                    // What the painter was handed this frame is the outline's ground truth; the
                    // animation itself cannot be asked for a value twice in one frame, and reading
                    // the response after the pass would hand back the frame before.
                    painted = ui
                        .ctx()
                        .read_response(card_id)
                        .map(|response| response.rect.translate(-map.min.to_vec2()));
                },
            );
            output.drop_without_applying_deltas();
            if frame >= 2 {
                assert_eq!(st.hover, Some((0, 0)), "hover identity must not change");
                let painted = painted.expect("the hovered monitor is a widget");
                let outline = st.field.outline_rect().expect("hover forms an outline");
                assert!(
                    outline.min.distance(painted.min) < 0.001,
                    "frame {frame}: {outline:?} vs {painted:?}"
                );
                assert_eq!(outline.size(), painted.size());
                if frame == 3 {
                    initial_outline = Some(outline);
                }
                if frame == 11 {
                    let target = vm.editor.placement[0];
                    assert!(outline.min.x > initial_outline.unwrap().min.x + 1.0);
                    assert!(
                        outline.min.x < monitor_rect(xf.p(target.x, target.y), mon, xf.s).min.x
                    );
                }
            }
        }
    }

    #[test]
    fn fading_preserves_premultiplied_color_without_darkening_it_twice() {
        let c = Color32::from_rgba_unmultiplied(238, 236, 228, 71);
        assert_eq!(fade(c, 1.0), c);
        assert!(fade(c, 0.6).r() > 35);
    }
}
