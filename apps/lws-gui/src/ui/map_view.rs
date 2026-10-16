//! The monitor map, draggable computers, links and quiet placement feedback.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::field::Frame;
use super::theme::{fam, font, rgba, Palette};
use super::widgets::{self, Kind};
use super::{Env, PaddleDrag, PaddleHandle, UiState, FIELD_FRAME};
use crate::gui_settings::{LinkStyle, Look, PAN_MAX};
use crate::map::{Dir, STAGE_H, STAGE_W};
use crate::view_model::layout::{LinkState, MapLink, MapModel, MapNode};
use crate::view_model::{Action, ViewModel};
use ui_text::{text, Key};

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
    let bridge_live = vm.look.link_style == LinkStyle::Ascii
        && model
            .links
            .iter()
            .any(|link| matches!(link.state, LinkState::Active | LinkState::Idle));
    st.bridge.tick(t, simulate && bridge_live);
    let rtt = vm
        .status
        .as_ref()
        .and_then(|status| single_peer_rtt(status, vm.self_node()));
    st.bridge.set_latency(&painter, lang, rtt);
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

    // Raw edge hit testing runs before card widgets: a paddle never starts a node drag.
    let paddle_hovered = paddle_interaction(ui, vm, st, &model, &shown, xf, rect, t, actions);

    // Hover identity may stay the same while the card glides, zooms or pans.
    // Retarget before painting so the outline uses this frame's displayed geometry.
    if simulate && st.drag.is_none() {
        if let Some((n, _)) = st.hover {
            if let Some(node) = model.nodes.get(n) {
                let local = node_rect(shown[n], node, s).translate(-rect.min.to_vec2());
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
            vm.modal.is_none()
                && !vm.appearance_open
                && !paddle_hovered
                && st.paddle_drag.is_none(),
            &mut hovered,
            actions,
        );
    }
    if bridge_live {
        for link in model
            .links
            .iter()
            .filter(|link| matches!(link.state, LinkState::Active | LinkState::Idle))
        {
            let (a, b) = link_ends(&model, link, &shown, xf);
            st.bridge.paint(
                &painter,
                a,
                b,
                link.forward,
                link.dir == Dir::Horizontal,
                f32::from(vm.look.zoom_percent) / 100.0,
                (
                    (link.a_span.1 - link.a_span.0) as f32 * s,
                    (link.b_span.1 - link.b_span.0) as f32 * s,
                ),
                pal.accent,
            );
            st.bridge.paint_latency(&painter, a + (b - a) * 0.5, pal);
        }
    }
    let paddle_fading = paint_paddles(&painter, pal, st, &model, &shown, xf, t);
    if simulate && hovered != st.hover {
        match hovered {
            Some((n, _)) if simulate && st.drag.is_none() => {
                let node = &model.nodes[n];
                let local = node_rect(shown[n], node, s).translate(-rect.min.to_vec2());
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
    // A bridge has its own clock: a static or settled background must not stop its flow.
    // Both layers freeze without scheduling frames when the map/window is inactive.
    (moving || paddle_fading || (simulate && bridge_live)).then_some(FIELD_FRAME)
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
/// Screen endpoints following the same node animation as the bridge.
fn paddle_ends(model: &MapModel, link: &MapLink, from: bool, shown: &[Pos2], xf: Xf) -> [Pos2; 2] {
    let (index, point, span) = if from {
        (link.a, link.from, link.a_span)
    } else {
        (link.b, link.to, link.b_span)
    };
    let carry = shown[index] - xf.p(model.nodes[index].at.x, model.nodes[index].at.y);
    let points = if link.dir == Dir::Horizontal {
        [xf.p(point.x, span.0), xf.p(point.x, span.1)]
    } else {
        [xf.p(span.0, point.y), xf.p(span.1, point.y)]
    };
    [points[0] + carry, points[1] + carry]
}

fn paddle_hit(points: [Pos2; 2], pointer: Pos2, horizontal: bool) -> Option<PaddleHandle> {
    let (normal, tangent, start, end) = if horizontal {
        (
            (pointer.x - points[0].x).abs(),
            pointer.y,
            points[0].y,
            points[1].y,
        )
    } else {
        (
            (pointer.y - points[0].y).abs(),
            pointer.x,
            points[0].x,
            points[1].x,
        )
    };
    if normal > 8.0 || tangent < start - 8.0 || tangent > end + 8.0 {
        return None;
    }
    // Short paddles still have a middle grip and two distinct ends. The outer half
    // of the edge hit strip also moves them, without requiring subpixel precision.
    let end_zone = ((end - start) / 3.0).min(8.0);
    Some(
        if normal >= 4.0 || (tangent > start + end_zone && tangent < end - end_zone) {
            PaddleHandle::Move
        } else if (tangent - start).abs() <= (tangent - end).abs() {
            PaddleHandle::Start
        } else {
            PaddleHandle::End
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn paddle_interaction(
    ui: &egui::Ui,
    vm: &ViewModel,
    st: &mut UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
    rect: Rect,
    time: f32,
    actions: &mut Vec<Action>,
) -> bool {
    let (pointer, hover_pointer, pressed, down, released, focused) = ui.input(|input| {
        (
            input.pointer.interact_pos(),
            input.pointer.hover_pos(),
            input.pointer.button_pressed(egui::PointerButton::Primary),
            input.pointer.button_down(egui::PointerButton::Primary),
            input.pointer.button_released(egui::PointerButton::Primary),
            input.focused,
        )
    });
    let enabled = vm.modal.is_none() && !vm.appearance_open && focused;
    if !enabled {
        st.paddle_hover = None;
        if st.paddle_drag.take().is_some() {
            actions.push(Action::CancelDrag);
        }
        return false;
    }
    let hit = if st.drag.is_none() {
        hover_pointer
            .filter(|point| rect.contains(*point))
            .and_then(|point| {
                model
                    .links
                    .iter()
                    .filter(|link| link.state != LinkState::Inactive && !link.whole_desktop)
                    .find_map(|link| {
                        [true, false].into_iter().find_map(|from| {
                            paddle_hit(
                                paddle_ends(model, link, from, shown, xf),
                                point,
                                link.dir == Dir::Horizontal,
                            )
                            .map(|handle| (link, from, handle))
                        })
                    })
            })
    } else {
        None
    };
    let hovered_id = hit.map(|(link, _, _)| link.id.as_str());
    if st.paddle_hover.as_deref() != hovered_id {
        st.paddle_hover = hovered_id.map(str::to_owned);
    }
    if let Some((link, from, handle)) = hit {
        st.reveal_paddle_at(&link.id, time);
        ui.ctx().set_cursor_icon(paddle_cursor(handle, link.dir));
        if pressed {
            st.paddle_drag = Some(PaddleDrag {
                link_id: link.id.clone(),
                from,
                span: if from { link.a_span } else { link.b_span },
                grab: pointer.expect("a paddle hit has a pointer"),
                handle,
            });
            st.field.release();
        }
    }
    if let Some(drag) = &st.paddle_drag {
        if let Some(link) = model.links.iter().find(|link| link.id == drag.link_id) {
            ui.ctx()
                .set_cursor_icon(paddle_cursor(drag.handle, link.dir));
            if down || released {
                if let Some(pointer) = pointer {
                    let delta = xf.stage_delta(pointer - drag.grab);
                    let delta = if link.dir == Dir::Horizontal {
                        delta.y
                    } else {
                        delta.x
                    };
                    let bounds = link.span;
                    let span = match drag.handle {
                        PaddleHandle::Move => crate::map::move_paddle(drag.span, delta, bounds),
                        PaddleHandle::Start => {
                            crate::map::resize_paddle(drag.span, true, drag.span.0 + delta, bounds)
                        }
                        PaddleHandle::End => {
                            crate::map::resize_paddle(drag.span, false, drag.span.1 + delta, bounds)
                        }
                    };
                    actions.push(Action::MovePaddle {
                        link_id: drag.link_id.clone(),
                        from: drag.from,
                        start: span.0,
                        end: span.1,
                    });
                }
            }
        }
        if released {
            st.paddle_drag = None;
            actions.push(Action::DropPaddle);
        } else if !down {
            st.paddle_drag = None;
            actions.push(Action::CancelDrag);
        }
    }
    hit.is_some() || st.paddle_drag.is_some()
}

fn paddle_cursor(handle: PaddleHandle, dir: Dir) -> egui::CursorIcon {
    match (handle, dir) {
        (PaddleHandle::Move, _) => egui::CursorIcon::Grab,
        (_, Dir::Horizontal) => egui::CursorIcon::ResizeVertical,
        (_, Dir::Vertical) => egui::CursorIcon::ResizeHorizontal,
    }
}

fn paddle_alpha(last_reveal: Option<f32>, now: f32) -> f32 {
    last_reveal.map_or(0.0, |last| {
        (1.0 - (now - last).max(0.0) / 0.2).clamp(0.0, 1.0)
    })
}

fn paint_paddles(
    painter: &egui::Painter,
    pal: &Palette,
    st: &mut UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
    now: f32,
) -> bool {
    let mut fading = false;
    for link in &model.links {
        if link.whole_desktop || link.state == LinkState::Inactive {
            continue;
        }
        let held = st.drag.as_ref().is_some_and(|drag| {
            model.nodes[link.a].node_id == drag.node_id
                || model.nodes[link.b].node_id == drag.node_id
        }) || st
            .paddle_drag
            .as_ref()
            .is_some_and(|drag| drag.link_id == link.id)
            || st.paddle_hover.as_deref() == Some(link.id.as_str());
        if held {
            st.reveal_paddle_at(&link.id, now);
        }
        let alpha = paddle_alpha(st.paddle_reveals.get(&link.id).copied(), now);
        if alpha <= 0.0 {
            continue;
        }
        fading |= !held;
        for from in [true, false] {
            let dragged = st
                .paddle_drag
                .as_ref()
                .is_some_and(|drag| drag.link_id == link.id && drag.from == from);
            let color = if dragged { pal.accent } else { pal.line };
            painter.line_segment(
                paddle_ends(model, link, from, shown, xf),
                Stroke::new(4.0, fade(color, alpha)),
            );
        }
    }
    st.paddle_reveals.retain(|_, last| now - *last < 0.2);
    fading
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
    let (mut color, mut alpha) = (pal.ink2, 1.0f32);
    match link.state {
        LinkState::Active => color = pal.accent,
        LinkState::Offline | LinkState::Inactive => alpha = 0.45,
        LinkState::Idle => {}
    }
    let color = fade(color, alpha);
    if style == LinkStyle::Ascii && matches!(link.state, LinkState::Active | LinkState::Idle) {
        // Painted above the cards so a touching seam remains visible.
    } else if style == LinkStyle::Ascii {
        field.link(painter, a, b, color);
    } else {
        painter.add(Shape::dashed_line(
            &[a, b],
            Stroke::new(1.5, color),
            5.0,
            3.5,
        ));
    }
    // Endpoint paddles are painted separately, above cards, only during interaction.
}

fn fade(c: Color32, k: f32) -> Color32 {
    // Color32 stores premultiplied channels; do not premultiply them a second time.
    c.gamma_multiply(k)
}

fn monitor_rect(at: Pos2, r: crate::map::Rect, s: f32) -> Rect {
    Rect::from_min_size(
        at + Vec2::new(r.x as f32 * s, r.y as f32 * s),
        Vec2::new(r.w as f32 * s, r.h as f32 * s),
    )
}

fn node_rect(at: Pos2, node: &MapNode, scale: f32) -> Rect {
    Rect::from_min_size(at, Vec2::new(node.w as f32 * scale, node.h as f32 * scale))
}

fn single_peer_rtt(status: &protocol::ipc::Status, local: Option<&str>) -> Option<u32> {
    let local = local?;
    let peers = status
        .members
        .iter()
        .filter(|member| member.online && member.node_id != local)
        .count();
    (peers == 1 && status.rtt_p95_us > 0).then_some(status.rtt_p95_us)
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
    let system_hot = enabled
        && (ui
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|p| node_rect(at, node, s).contains(p))
            || st
                .drag
                .as_ref()
                .is_some_and(|drag| drag.node_id == node.node_id)
            || ui.memory(|mem| {
                (0..node.mons.len())
                    .any(|m| mem.has_focus(egui::Id::new(("mon", &node.node_id, m))))
            }));
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
                        // Free placement: proximity creates a bridge, never a forced snap.
                        snap_threshold: 0.0,
                    });
                }
            }
        }
        if enabled && response.hovered() && st.drag.is_none() {
            *hovered = Some((index, 0));
        }
        if !painter.clip_rect().intersects(rect) {
            continue;
        }

        let hot = system_hot;
        let dragging = response.dragged();
        let shadow = if dragging {
            egui::epaint::Shadow {
                offset: [0, 30],
                blur: 80,
                spread: 0,
                color: pal.shadow_drag,
            }
        } else {
            egui::epaint::Shadow {
                offset: [0, 10],
                blur: 30,
                spread: 0,
                color: pal.shadow,
            }
        };
        let radius = pal.rad_sm();
        painter.add(shadow.as_shape(rect, radius));
        let border = if node.target || hot {
            pal.accent
        } else {
            pal.line
        };
        painter.rect_filled(rect, radius, fade(pal.card, k));
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
    // A link the core keeps but cannot use: a display it names is not reported, so nothing
    // crosses there until that monitor is back. The map names the display, which the faded
    // line cannot say on its own; placing the pair again is the way to give it another link,
    // and this one returns by itself once the display is reported again.
    for line in vm.inactive_link_lines() {
        let galley = ui.painter().layout(
            line,
            font(12.0, fam::sans_medium()),
            pal.accent,
            (rect.width() - 92.0).max(40.0),
        );
        let size = galley.size();
        ui.painter()
            .galley(Pos2::new(center - size.x / 2.0, y), galley, pal.accent);
        y += size.y + 4.0;
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    #[test]
    fn minimum_paddles_keep_both_resize_ends_and_a_move_grip_at_every_zoom() {
        for scale in [0.315, 0.7, 1.0, 1.5] {
            let points = [
                Pos2::new(100.0, 100.0),
                Pos2::new(100.0, 100.0 + 12.0 * scale),
            ];
            assert_eq!(
                paddle_hit(points, points[0], true),
                Some(PaddleHandle::Start)
            );
            assert_eq!(paddle_hit(points, points[1], true), Some(PaddleHandle::End));
            let middle = points[0] + (points[1] - points[0]) * 0.5;
            assert_eq!(paddle_hit(points, middle, true), Some(PaddleHandle::Move));
            assert_eq!(
                paddle_hit(points, middle + Vec2::new(7.0, 0.0), true),
                Some(PaddleHandle::Move)
            );
            assert_eq!(paddle_hit(points, middle + Vec2::new(9.0, 0.0), true), None);
        }
    }

    #[test]
    fn paddles_reveal_both_edges_then_finish_fading_without_idle_repaints() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let mut model = owner_layout();
        model.links[0].whole_desktop = false;
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let now = std::time::Instant::now();
        let mut st = UiState::new(now);
        let vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        let pal = Palette::of(vm.look.theme);
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 708.0));
        let xf = Xf::new(rect, 100);
        let shown: Vec<_> = model
            .nodes
            .iter()
            .map(|node| xf.p(node.at.x, node.at.y))
            .collect();
        let pointer = paddle_ends(&model, &model.links[0], true, &shown, xf)[0];
        let render = |st: &mut UiState, t, hover: bool| {
            let mut fading = false;
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(rect),
                    time: Some(f64::from(t)),
                    events: vec![if hover {
                        egui::Event::PointerMoved(pointer)
                    } else {
                        egui::Event::PointerGone
                    }],
                    ..Default::default()
                },
                |ui| {
                    paddle_interaction(ui, &vm, st, &model, &shown, xf, rect, t, &mut Vec::new());
                    fading = paint_paddles(ui.painter(), &pal, st, &model, &shown, xf, t);
                },
            );
            let count = output
                .shapes
                .iter()
                .filter(|shape| {
                    matches!(&shape.shape,
                Shape::LineSegment { stroke, .. } if stroke.width == 4.0)
                })
                .count();
            output.drop_without_applying_deltas();
            (count, fading)
        };
        assert_eq!(render(&mut st, 0.0, false), (0, false), "hidden by default");
        st.drag = Some(super::super::Drag {
            node_id: model.nodes[0].node_id.clone(),
            start: model.nodes[0].at,
            grab: pointer,
        });
        assert_eq!(
            render(&mut st, 0.1, false),
            (2, false),
            "node drag reveals both edges"
        );
        st.drag = None;
        assert_eq!(render(&mut st, 0.2, false), (2, true));
        assert_eq!(
            render(&mut st, 0.35, false),
            (0, false),
            "fade ends within250ms"
        );
        assert_eq!(
            render(&mut st, 0.4, true),
            (2, false),
            "linked edge hover reveals both paddles without scheduling frames"
        );
        assert_eq!(render(&mut st, 0.65, false), (0, false));
        assert_eq!(render(&mut st, 5.0, false), (0, false), "no idle animation");
    }

    #[test]
    fn paddle_pointer_drag_sends_one_layout_update_and_never_moves_the_node() {
        use crate::map::{Pt, Rect as MapRect, Shape as MapShape};
        use crate::view_model::{CoreEndpoint, EditorDisplay, EditorNode, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        vm.on_connected("test");
        vm.editor.editable = true;
        vm.look.clouds = false;
        vm.look.field = crate::gui_settings::FieldMode::Off;
        vm.look.pan_x = 0;
        vm.look.pan_y = 0;
        vm.look.zoom_percent = 100;
        vm.editor.nodes = ["aa".repeat(32), "bb".repeat(32)]
            .into_iter()
            .map(|id| EditorNode {
                node_id: id,
                display_name: String::new(),
                online: true,
                displays: vec![EditorDisplay {
                    id: "screen".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                    primary: true,
                    native_known: true,
                    rotation: 0,
                }],
            })
            .collect();
        vm.editor.scale = crate::map::BASE_SCALE;
        let mon = MapRect::new(0.0, 0.0, 144.0, 81.0);
        vm.editor.shapes = vec![
            MapShape {
                mons: vec![mon],
                w: mon.w,
                h: mon.h
            };
            2
        ];
        vm.editor.baseline = vec![Pt::new(200.0, 80.0), Pt::new(600.0, 80.0)];
        vm.editor.placement = vec![Pt::new(200.0, 80.0), Pt::new(400.0, 80.0)];
        let positions = vm.editor.placement.clone();
        let link = vm.map_model().links.remove(0);
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let now = std::time::Instant::now();
        let mut st = UiState::new(now);
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 708.0));
        let xf = Xf::new(rect, 100);
        let start = xf.p(link.from.x - 1.0, link.a_span.0 + 1.0);
        let end = start + Vec2::new(0.0, 20.0);
        let mut drops = 0;
        for (frame, pointer, button) in [
            (0, start, None),
            (1, start, Some(true)),
            (2, end, Some(false)),
        ] {
            let mut events = vec![egui::Event::PointerMoved(pointer)];
            if let Some(pressed) = button {
                events.push(egui::Event::PointerButton {
                    pos: pointer,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            let mut actions = Vec::new();
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(rect),
                    time: Some(f64::from(frame) * 0.05),
                    events,
                    ..Default::default()
                },
                |ui| {
                    draw(
                        ui,
                        &Palette::of(vm.look.theme),
                        &vm,
                        &mut st,
                        rect,
                        Env {
                            now,
                            animate: false,
                        },
                        frame as f32 * 0.05,
                        &mut actions,
                    );
                },
            )
            .drop_without_applying_deltas();
            for action in actions {
                assert!(!matches!(
                    action,
                    Action::MoveNode { .. } | Action::DropNode(_)
                ));
                drops += usize::from(matches!(action, Action::DropPaddle));
                vm.local(&action);
            }
        }
        assert_eq!(drops, 1);
        assert_eq!(vm.editor.placement, positions);
        let sent = vm
            .editor
            .save
            .sent
            .as_ref()
            .expect("paddle drop sends layout CAS");
        assert!(sent.links[0].from.start > 0.0);
        assert_eq!(
            vm.take_outgoing()
                .iter()
                .filter(|(_, message)| matches!(
                    message,
                    protocol::ipc::gui_frame::Msg::UpdateTopology(_)
                ))
                .count(),
            1
        );
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
                contact: None,
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
        // Legacy whole-desktop documents keep their original placement and bridge.
        // Endpoint editing applies after a concrete per-display link is established.
        let (pc, laptop) = (&arrangement.shapes[0], &arrangement.shapes[1]);
        let (pc_at, laptop_at) = (arrangement.pos[0], arrangement.pos[1]);
        let top = pc_at.y.max(laptop_at.y);
        let bottom = (pc_at.y + pc.h).min(laptop_at.y + laptop.h);
        let links = vec![MapLink {
            id: "legacy".to_owned(),
            a_display: 0,
            b_display: 0,
            a_span: (top, bottom),
            b_span: (top, bottom),
            a: 1,
            b: 0,
            from: map::Pt::new(laptop_at.x + laptop.w, (top + bottom) / 2.0),
            to: map::Pt::new(pc_at.x, (top + bottom) / 2.0),
            dir: Dir::Horizontal,
            span: (top, bottom),
            state: LinkState::Active,
            whole_desktop: true,
            forward: false,
        }];
        MapModel {
            nodes,
            links,
            screens: 3,
            dirty: false,
        }
    }

    #[test]
    fn aggregate_rtt_is_only_attributed_to_exactly_one_online_peer() {
        use protocol::ipc::{MemberStatus, Status};
        let member = |id: &str, online| MemberStatus {
            node_id: id.to_owned(),
            online,
            ..Default::default()
        };
        let mut status = Status {
            rtt_p95_us: 1_500,
            members: vec![member("local", true), member("peer", true)],
            ..Default::default()
        };
        assert_eq!(single_peer_rtt(&status, Some("local")), Some(1_500));
        assert_eq!(single_peer_rtt(&status, None), None);
        status.members.push(member("offline", false));
        assert_eq!(single_peer_rtt(&status, Some("local")), Some(1_500));
        status.members.push(member("second", true));
        assert_eq!(single_peer_rtt(&status, Some("local")), None);
        status.members.pop();
        status.rtt_p95_us = 0;
        assert_eq!(single_peer_rtt(&status, Some("local")), None);
        status.members[1].online = false;
        status.rtt_p95_us = 1_500;
        assert_eq!(single_peer_rtt(&status, Some("local")), None);
    }

    #[test]
    fn owner_system_hover_encloses_every_monitor_with_one_group_rectangle() {
        let model = owner_layout();
        let node = model
            .nodes
            .iter()
            .find(|node| node.mons.len() == 2)
            .unwrap();
        let at = Pos2::new(70.0, 90.0);
        for scale in [0.7, 1.0, 1.5] {
            let group = node_rect(at, node, scale);
            for monitor in &node.mons {
                let rect = monitor_rect(at, monitor.rect, scale);
                assert!(group.contains_rect(rect));
            }
            assert_eq!(group.min, at);
        }
    }

    #[test]
    fn separated_owner_desktops_have_visible_bridges_without_permanent_rails() {
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
                    let look = Look {
                        field: crate::gui_settings::FieldMode::Off,
                        link_style: style,
                        clouds: false,
                        ..Look::default()
                    };
                    field.paint(
                        ui.painter(),
                        &Frame {
                            map: ui.ctx().content_rect(),
                            pal: &pal,
                            look,
                            simulate: false,
                            t: 0.0,
                            pointer: None,
                        },
                    );
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
                    if style == LinkStyle::Ascii {
                        super::super::bridge::Bridge::default().paint(
                            ui.painter(),
                            a,
                            b,
                            link.forward,
                            true,
                            1.5,
                            (120.0, 120.0),
                            pal.accent,
                        );
                    }
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
                assert_eq!(rails, 0, "{style:?} {state:?}");
                assert!(chain > 2, "{style:?} {state:?}");
                let glyphs = output
                    .shapes
                    .iter()
                    .filter(|s| matches!(&s.shape, Shape::Text(t) if matches!(t.galley.text(), ">" | "<" | "^" | "v")))
                    .count();
                assert_eq!(
                    glyphs > 0,
                    style == LinkStyle::Ascii,
                    "static field must not hide ASCII bridge or add glyphs to Line"
                );
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
