//! The monitor map, draggable computers, links and quiet placement feedback.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::field::Frame;
use super::theme::{fam, font, rgba, Palette};
use super::widgets::{self, Kind};
use super::{Env, PaddleDrag, PaddleHandle, UiState, FIELD_FRAME};
use crate::gui_settings::{
    zoom_display_percent, zoom_from_display, LinkStyle, Look, PAN_MAX, ZOOM_MAX, ZOOM_MIN,
};
use crate::map::{Dir, STAGE_H, STAGE_W};
use crate::view_model::layout::{LinkState, MapLink, MapModel, MapNode};
use crate::view_model::{Action, Modal, ViewModel};
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
        .clamp(0.05, 1.0);
        let s = base * f32::from(zoom_percent.clamp(ZOOM_MIN, ZOOM_MAX)) / 100.0;
        Self {
            origin: rect.center() - Vec2::new(STAGE_W as f32, STAGE_H as f32) * (s / 2.0),
            s,
        }
    }

    fn fit(mut self, viewport: Rect, model: &MapModel) -> Self {
        let bounds = model.nodes.iter().fold(
            Rect::from_min_size(Pos2::ZERO, Vec2::new(STAGE_W as f32, STAGE_H as f32)),
            |bounds, node| {
                bounds.union(Rect::from_min_size(
                    Pos2::new(node.at.x as f32, node.at.y as f32),
                    Vec2::new(node.w as f32, node.h as f32 + 70.0),
                ))
            },
        );
        let ratio = (STAGE_W as f32 / bounds.width()).min(STAGE_H as f32 / bounds.height());
        self.s *= ratio;
        self.origin = viewport.center() - bounds.center().to_vec2() * self.s;
        self
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
        && model.links.iter().any(|link| {
            !link.disabled && matches!(link.state, LinkState::Active | LinkState::Idle)
        });
    st.bridge.tick(t, simulate && bridge_live);
    let rtt = vm
        .status
        .as_ref()
        .and_then(|status| single_peer_rtt(status, vm.self_node()));
    st.bridge.set_latency(&painter, lang, rtt);
    let mut xf = Xf::new(rect, vm.look.zoom_percent).fit(rect, &model);
    let saved_pan = Vec2::new(f32::from(vm.look.pan_x), f32::from(vm.look.pan_y));
    let pointer_over_object = ui.input(|i| i.pointer.hover_pos()).is_some_and(|p| {
        model.nodes.iter().any(|node| {
            node.mons.iter().any(|mon| {
                monitor_rect(xf.p(node.at.x, node.at.y) + saved_pan, mon.rect, xf.s).contains(p)
            })
        }) || model.links.iter().any(|link| {
            let a = xf.p(link.from.x, link.from.y) + saved_pan;
            let b = xf.p(link.to.x, link.to.y) + saved_pan;
            segment_distance(p, [a, b]) <= 8.0
        })
    });
    xf.origin += pan(ui, vm, rect, !pointer_over_object, actions);
    let transform_id = egui::Id::new("map-drag-transform");
    if st.drag.is_some() || st.paddle_drag.is_some() || st.display_resize.is_some() {
        if let Some((origin, scale)) = ui
            .ctx()
            .data_mut(|d| d.get_temp::<(Pos2, f32)>(transform_id))
        {
            xf = Xf { origin, s: scale };
        }
    } else {
        ui.ctx()
            .data_mut(|d| d.insert_temp(transform_id, (xf.origin, xf.s)));
    }
    let map_layer_hovered = ui
        .input(|i| i.pointer.hover_pos())
        .is_none_or(|p| ui.ctx().layer_id_at(p) == Some(ui.layer_id()));
    let s = xf.s;

    // Where each computer is drawn now: animated towards the model unless it is being dragged.
    let shown: Vec<Pos2> = model
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let dragging = st.drag.as_ref().is_some_and(|d| d.node_id == node.node_id)
                || st
                    .display_resize
                    .as_ref()
                    .is_some_and(|d| d.node_id == node.node_id);
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

    // Handles and bridge menus arbitrate before raw paddles and card drags.
    let resize_hovered = display_resize_interaction(ui, vm, st, &model, &shown, xf, rect, actions);
    let bridge_hovered = bridge_interaction(ui, vm, st, &model, &shown, xf, rect, actions);
    let paddle_hovered = !resize_hovered
        && !bridge_hovered
        && paddle_interaction(ui, vm, st, &model, &shown, xf, rect, t, actions);

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
            look: &vm.look,
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
    if bridge_live {
        for link in model.links.iter().filter(|link| {
            !link.disabled && matches!(link.state, LinkState::Active | LinkState::Idle)
        }) {
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
            st.bridge.paint_latency(
                &painter,
                a + (b - a) * 0.5,
                pal,
                model
                    .nodes
                    .iter()
                    .zip(&shown)
                    .map(|(node, &at)| node_rect(at, node, s).union(caption_rect(at, node, s))),
            );
        }
    }
    // Node captions must cover both the field and the bridge glyphs.
    let mut hovered: Option<(usize, usize)> = None;
    for (i, node) in model.nodes.iter().enumerate() {
        let bounds = node_rect(shown[i], node, s);
        let mut caption_clip = Rect::from_min_max(
            bounds.left_bottom(),
            Pos2::new(bounds.right(), rect.bottom()),
        );
        for (j, other) in model.nodes.iter().enumerate() {
            if i == j {
                continue;
            }
            for mon in &other.mons {
                let obstacle = monitor_rect(shown[j], mon.rect, s);
                if obstacle.right() > bounds.left()
                    && obstacle.left() < bounds.right()
                    && obstacle.bottom() > bounds.bottom()
                {
                    caption_clip.max.y = caption_clip
                        .max
                        .y
                        .min((obstacle.top() - 4.0).max(bounds.bottom()));
                }
            }
        }
        node_ui(
            ui,
            &painter,
            pal,
            st,
            node,
            i,
            shown[i],
            xf,
            caption_clip,
            vm.modal.is_none()
                && !vm.appearance_open
                && map_layer_hovered
                && !paddle_hovered
                && !resize_hovered
                && !bridge_hovered
                && st.display_resize.is_none()
                && st.paddle_drag.is_none(),
            &mut hovered,
            actions,
        );
    }
    let phantom_visible = draw_phantoms(
        ui,
        &painter,
        pal,
        vm,
        &model,
        &shown,
        xf,
        &mut st.phantom_art,
        actions,
    );
    let phantom_live = env.animate && vm.modal.is_none() && !vm.appearance_open && phantom_visible;
    st.phantom_art.tick(t, phantom_live);
    let paddle_fading = paint_paddles(&painter, pal, st, &model, &shown, xf, t);
    paint_bridge_endpoints(&painter, pal, st, &model, &shown, xf);
    paint_resize_handles(ui, &painter, pal, st, &model, &shown, xf);
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
    properties(ui, pal, vm, st, &model, rect, actions);
    // A bridge has its own clock: a static or settled background must not stop its flow.
    // Animated layers freeze without scheduling frames when the map/window is inactive.
    (moving || paddle_fading || (simulate && bridge_live) || phantom_live).then_some(FIELD_FRAME)
}

fn pan(
    ui: &egui::Ui,
    vm: &ViewModel,
    rect: Rect,
    allow_start: bool,
    actions: &mut Vec<Action>,
) -> Vec2 {
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
    if pressed
        && enabled
        && allow_start
        && pointer.is_none_or(|p| ui.ctx().layer_id_at(p) == Some(ui.layer_id()))
    {
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
                ..vm.look.clone()
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
    let current = vm.look.zoom_percent.clamp(ZOOM_MIN, ZOOM_MAX);
    let display = zoom_display_percent(current);
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
        enabled && current > ZOOM_MIN,
    )
    .clicked()
    {
        wanted = zoom_from_display(display - 5.0);
    }
    widgets::text(
        &mut controls,
        &format!("{display:.0}%"),
        12.0,
        fam::mono(),
        pal.ink2,
    );
    if widgets::button(
        &mut controls,
        pal,
        "+",
        Kind::Ghost,
        enabled && current < ZOOM_MAX,
    )
    .clicked()
    {
        wanted = zoom_from_display(display + 5.0);
    }
    if enabled {
        let (wheel, pinch) = ui.input(|input| {
            input
                .pointer
                .hover_pos()
                .filter(|point| rect.contains(*point))
                .map_or((0.0, 1.0), |_| {
                    input
                        .raw
                        .events
                        .iter()
                        .fold((0.0, 1.0), |(wheel, pinch), event| match event {
                            egui::Event::MouseWheel { delta, .. } => (wheel + delta.y, pinch),
                            egui::Event::Zoom(factor) => (wheel, pinch * factor),
                            _ => (wheel, pinch),
                        })
                })
        });
        if (pinch - 1.0).abs() > f32::EPSILON {
            wanted = (f32::from(current) * pinch)
                .round()
                .clamp(f32::from(ZOOM_MIN), f32::from(ZOOM_MAX)) as u16;
        } else if wheel > 0.0 {
            wanted = zoom_from_display(display + 5.0);
        } else if wheel < 0.0 {
            wanted = zoom_from_display(display - 5.0);
        }
    }
    if wanted != current {
        actions.push(Action::SetLook(Look {
            zoom_percent: wanted,
            ..vm.look.clone()
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
            .filter(|point| ui.ctx().layer_id_at(*point) == Some(ui.layer_id()))
            .and_then(|point| {
                model
                    .links
                    .iter()
                    .filter(|link| {
                        !link.disabled && link.state != LinkState::Inactive && !link.whole_desktop
                    })
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
        if link.disabled || link.whole_desktop || link.state == LinkState::Inactive {
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
    if link.disabled {
        painter.add(Shape::dashed_line(
            &[a, b],
            Stroke::new(1.5, color),
            5.0,
            3.5,
        ));
    } else if style == LinkStyle::Ascii && matches!(link.state, LinkState::Active | LinkState::Idle)
    {
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

fn segment_distance(point: Pos2, ends: [Pos2; 2]) -> f32 {
    let delta = ends[1] - ends[0];
    let t = if delta.length_sq() > 0.0 {
        ((point - ends[0]).dot(delta) / delta.length_sq()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    point.distance(ends[0] + delta * t)
}

fn bridge_description(model: &MapModel, link: &MapLink, lang: ui_text::Lang) -> String {
    let a = &model.nodes[link.a];
    let b = &model.nodes[link.b];
    let endpoints = ui_text::fill(
        lang,
        Key::BridgeEndpoints,
        &[
            &a.name,
            &a.mons[link.a_display].num,
            &b.name,
            &b.mons[link.b_display].num,
        ],
    )
    .replace(' ', "\u{a0}")
    .replace("\u{a0}↔\u{a0}", "\n↔\u{a0}");
    let lengths = ui_text::fill(
        lang,
        Key::BridgeLengths,
        &[
            &format!("{:.0}", link.a_logical_length),
            &format!("{:.0}", link.b_logical_length),
        ],
    );
    let reason = if link.short_candidate {
        text(lang, Key::BridgeShortCandidate)
    } else if link.disabled {
        text(lang, Key::BridgeDisabled)
    } else {
        ""
    };
    format!("{endpoints}\n{lengths}\n{reason}")
}

/// Keep transient controls below the map heading and zoom toolbar.
fn popup_bounds(clip: Rect) -> Rect {
    Rect::from_min_max(
        clip.min + Vec2::new(0.0, 130.0_f32.min(clip.height() / 3.0)),
        clip.max,
    )
}

#[allow(clippy::too_many_arguments)]
fn bridge_interaction(
    ui: &egui::Ui,
    vm: &ViewModel,
    st: &mut UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
    clip: Rect,
    actions: &mut Vec<Action>,
) -> bool {
    if vm.modal.is_some()
        || vm.appearance_open
        || st.drag.is_some()
        || st.display_resize.is_some()
        || st.paddle_drag.is_some()
    {
        return false;
    }
    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| clip.contains(*p) && ui.ctx().layer_id_at(*p) == Some(ui.layer_id()));
    let mut blocked = false;
    for (index, link) in model.links.iter().enumerate() {
        let hit = pointer.is_some_and(|p| {
            let (a, b) = link_ends(model, link, shown, xf);
            segment_distance(p, [a, b]) <= 5.0
                || [true, false].into_iter().any(|from| {
                    segment_distance(p, paddle_ends(model, link, from, shown, xf)) <= 5.0
                })
        });
        let secondary = ui.input(|i| {
            i.pointer.button_down(egui::PointerButton::Secondary)
                || i.pointer.button_released(egui::PointerButton::Secondary)
        });
        let endpoint = pointer.is_some_and(|p| {
            [true, false].into_iter().any(|from| {
                paddle_hit(
                    paddle_ends(model, link, from, shown, xf),
                    p,
                    link.dir == Dir::Horizontal,
                )
                .is_some()
            })
        });
        let capture = hit && (secondary || link.disabled || !endpoint);
        let hit_rect = if capture {
            Rect::from_center_size(pointer.expect("hit has pointer"), Vec2::splat(10.0))
                .intersect(clip)
        } else {
            Rect::NOTHING
        };
        let response = ui.interact(
            hit_rect,
            egui::Id::new(("bridge-menu", &link.id, index)),
            Sense::click(),
        );
        if capture {
            response
                .clone()
                .on_hover_text(bridge_description(model, link, vm.look.language));
        }
        if response.clicked() || response.secondary_clicked() {
            st.selected_bridge = Some(link.bridge_id.clone());
            st.selected_display = None;
        }
        let popup = egui::Popup::context_menu(&response);
        let size = popup.get_expected_size().unwrap_or(Vec2::new(260.0, 220.0));
        let bounds = popup_bounds(clip);
        let (a, b) = link_ends(model, link, shown, xf);
        let anchor = popup.get_anchor_rect().map_or(a.lerp(b, 0.5), |r| r.min);
        let position = Pos2::new(
            anchor
                .x
                .clamp(bounds.left(), (bounds.right() - size.x).max(bounds.left())),
            anchor
                .y
                .clamp(bounds.top(), (bounds.bottom() - size.y).max(bounds.top())),
        );
        popup
            .at_position(position)
            .align(egui::RectAlign::BOTTOM_START)
            .align_alternatives(&[])
            .show(|ui| {
                ui.set_max_width((clip.width() - 24.0).clamp(80.0, 240.0));
                egui::ScrollArea::vertical()
                    .max_height((bounds.height() - 24.0).max(24.0))
                    .show(ui, |ui| {
                        ui.label(bridge_description(model, link, vm.look.language));
                        ui.label(text(vm.look.language, Key::BridgeAllSegments));
                        if ui
                            .button(text(
                                vm.look.language,
                                if link.disabled {
                                    Key::EnableBridge
                                } else {
                                    Key::DisableBridge
                                },
                            ))
                            .clicked()
                        {
                            actions.push(Action::SetBridgeEnabled {
                                link_id: link.id.clone(),
                                enabled: link.disabled,
                            });
                            ui.close();
                        }
                    });
            });
        blocked |= capture;
    }
    blocked
}

fn paint_bridge_endpoints(
    painter: &egui::Painter,
    pal: &Palette,
    st: &UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
) {
    for link in &model.links {
        let selected = st.selected_bridge.as_deref() == Some(link.bridge_id.as_str());
        if !link.disabled && !selected {
            continue;
        }
        let color = if selected { pal.accent } else { pal.ink2 };
        for from in [true, false] {
            let ends = paddle_ends(model, link, from, shown, xf);
            if link.disabled {
                painter.add(Shape::dashed_line(&ends, Stroke::new(2.5, color), 4.0, 3.0));
            } else {
                painter.line_segment(ends, Stroke::new(2.5, color));
            }
        }
    }
}

fn resize_handle(rect: Rect, clip: Rect) -> Option<Rect> {
    monitor_hit_rect(
        Rect::from_min_max(rect.max - Vec2::splat(16.0), rect.max),
        clip,
    )
}

#[allow(clippy::too_many_arguments)]
fn display_resize_interaction(
    ui: &egui::Ui,
    vm: &ViewModel,
    st: &mut UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
    clip: Rect,
    actions: &mut Vec<Action>,
) -> bool {
    let (pointer, pressed, down, released, focused, escape, shift) = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.button_pressed(egui::PointerButton::Primary),
            i.pointer.button_down(egui::PointerButton::Primary),
            i.pointer.button_released(egui::PointerButton::Primary),
            i.focused,
            i.key_pressed(egui::Key::Escape),
            i.modifiers.shift,
        )
    });
    if !focused || vm.modal.is_some() || vm.appearance_open || escape {
        if st.display_resize.take().is_some() {
            actions.push(Action::CancelDisplayResize);
        }
        return false;
    }
    let mut hit = false;
    if st.drag.is_none() && st.paddle_drag.is_none() && st.display_resize.is_none() {
        if pointer.is_some_and(|p| ui.ctx().layer_id_at(p) != Some(ui.layer_id())) {
            return false;
        }
        for (n, node) in model.nodes.iter().enumerate() {
            for mon in &node.mons {
                let rect = monitor_rect(shown[n], mon.rect, xf.s);
                let handle_hit = resize_handle(rect, clip)
                    .is_some_and(|r| pointer.is_some_and(|p| r.contains(p)));
                let move_hit = shift
                    && monitor_hit_rect(rect, clip)
                        .is_some_and(|r| pointer.is_some_and(|p| r.contains(p)));
                if move_hit {
                    ui.interact(
                        rect.intersect(clip),
                        egui::Id::new(("display-move-help", &node.node_id, &mon.display_id)),
                        Sense::hover(),
                    )
                    .on_hover_text(text(vm.look.language, Key::ShiftDragDisplay));
                }
                if handle_hit || move_hit {
                    hit = true;
                    if pressed {
                        st.selected_display = Some((node.node_id.clone(), mon.display_id.clone()));
                        st.selected_bridge = None;
                        ui.ctx().data_mut(|d| {
                            d.remove::<String>(egui::Id::new((
                                "display-diagonal",
                                &node.node_id,
                                &mon.display_id,
                            )))
                        });
                        st.display_resize = Some(super::DisplayResize {
                            node_id: node.node_id.clone(),
                            display_id: mon.display_id.clone(),
                            grab: pointer.expect("resize hit has pointer"),
                            width: mon.rect.w,
                            aspect: mon.aspect,
                            move_start: (move_hit && !handle_hit).then_some(crate::map::Pt::new(
                                node.at.x + mon.rect.x,
                                node.at.y + mon.rect.y,
                            )),
                        });
                        st.field.release();
                    }
                }
            }
        }
    }
    if let Some(drag) = &st.display_resize {
        ui.ctx().set_cursor_icon(if drag.move_start.is_some() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::ResizeNwSe
        });
        if let Some(pointer) = pointer.filter(|_| down || released) {
            let delta = xf.stage_delta(pointer - drag.grab);
            if let Some(start) = drag.move_start {
                actions.push(Action::MoveDisplay {
                    node_id: drag.node_id.clone(),
                    display_id: drag.display_id.clone(),
                    x: start.x + delta.x,
                    y: start.y + delta.y,
                });
            } else {
                let width = drag.width
                    + (delta.x + delta.y / drag.aspect) / (1.0 + 1.0 / drag.aspect.powi(2));
                let inches = width.hypot(width / drag.aspect) / 7.2;
                if width.is_finite() && (3.0..=120.0).contains(&inches) {
                    actions.push(Action::ResizeDisplay {
                        node_id: drag.node_id.clone(),
                        display_id: drag.display_id.clone(),
                        width,
                    });
                }
            }
        }
        if released {
            st.display_resize = None;
            actions.push(Action::DropDisplayResize);
        } else if !down {
            st.display_resize = None;
            actions.push(Action::CancelDisplayResize);
        }
        return true;
    }
    if hit {
        ui.ctx().set_cursor_icon(if shift {
            egui::CursorIcon::Grab
        } else {
            egui::CursorIcon::ResizeNwSe
        });
    }
    hit
}

fn paint_resize_handles(
    ui: &egui::Ui,
    painter: &egui::Painter,
    pal: &Palette,
    st: &UiState,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
) {
    let pointer = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| painter.clip_rect().contains(*p));
    for (n, node) in model.nodes.iter().enumerate() {
        for mon in &node.mons {
            let rect = monitor_rect(shown[n], mon.rect, xf.s);
            let selected = st
                .selected_display
                .as_ref()
                .is_some_and(|(node_id, display_id)| {
                    node_id == &node.node_id && display_id == &mon.display_id
                });
            if selected || pointer.is_some_and(|p| rect.contains(p)) {
                for offset in [4.0, 8.0, 12.0] {
                    painter.line_segment(
                        [
                            rect.max - Vec2::new(offset, 2.0),
                            rect.max - Vec2::new(2.0, offset),
                        ],
                        Stroke::new(1.5, pal.accent),
                    );
                }
            }
        }
    }
}

fn parse_diagonal(value: &str) -> Option<f64> {
    value
        .trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && (3.0..=120.0).contains(value))
}

fn properties(
    ui: &egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    model: &MapModel,
    clip: Rect,
    actions: &mut Vec<Action>,
) {
    if vm.modal.is_some() || vm.appearance_open || st.display_resize.is_some() {
        return;
    }
    let lang = vm.look.language;
    let clip = popup_bounds(clip);
    let mut open = true;
    let width = (clip.width() - 24.0).clamp(80.0, 280.0);
    if let Some((node_id, display_id)) = st.selected_display.clone() {
        let Some(mon) = model
            .nodes
            .iter()
            .find(|n| n.node_id == node_id)
            .and_then(|n| n.mons.iter().find(|m| m.display_id == display_id))
        else {
            st.selected_display = None;
            return;
        };
        egui::Window::new(text(lang, Key::DisplaySize))
            .id(egui::Id::new("display-properties"))
            .open(&mut open)
            .constrain_to(clip)
            .default_pos(clip.left_top() + Vec2::splat(12.0))
            .default_width(width)
            .max_width(width)
            .resizable(false)
            .collapsible(false)
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical()
                    .max_height((clip.height() - 80.0).max(40.0))
                    .show(ui, |ui| {
                        ui.set_max_width(width);
                        let source = match mon.size_source {
                            "auto" => Key::SizeSourceAuto,
                            "manual" => Key::SizeSourceManual,
                            _ => Key::SizeSourceUnknown,
                        };
                        ui.label(text(lang, Key::SizeSource));
                        ui.label(text(lang, source));
                        ui.label(text(lang, Key::DiagonalInches));
                        let id = egui::Id::new(("display-diagonal", &node_id, &display_id));
                        let mut value = ui
                            .ctx()
                            .data_mut(|d| d.get_temp::<String>(id))
                            .unwrap_or_else(|| {
                                mon.diagonal_inches
                                    .map(|v| format!("{v:.2}"))
                                    .unwrap_or_default()
                            });
                        let edit = ui.add(
                            egui::TextEdit::singleline(&mut value)
                                .desired_width(width - 24.0)
                                .hint_text(text(lang, Key::SizeSourceUnknown)),
                        );
                        let parsed = parse_diagonal(&value);
                        if !value.is_empty() && parsed.is_none() {
                            ui.colored_label(pal.accent_ink, text(lang, Key::InvalidDiagonal));
                        }
                        let apply = ui
                            .add_enabled(
                                parsed.is_some(),
                                egui::Button::new(text(lang, Key::ApplyDisplaySize)),
                            )
                            .clicked()
                            || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                        if let Some(inches) = parsed.filter(|_| apply) {
                            actions.push(Action::SetDisplayDiagonal {
                                node_id: node_id.clone(),
                                display_id: display_id.clone(),
                                inches,
                            });
                        }
                        ui.ctx().data_mut(|d| d.insert_temp(id, value));
                        if ui.button(text(lang, Key::ResetDisplaySize)).clicked() {
                            actions.push(Action::ResetDisplaySize {
                                node_id: node_id.clone(),
                                display_id: display_id.clone(),
                            });
                            ui.ctx().data_mut(|d| d.remove::<String>(id));
                        }
                        ui.separator();
                        ui.label(text(lang, Key::DisplaySizeMapOnly));
                        ui.label(text(lang, Key::ShiftDragDisplay));
                        for link in model.links.iter().filter(|link| {
                            (model.nodes[link.a].node_id == node_id
                                && model.nodes[link.a].mons[link.a_display].display_id
                                    == display_id)
                                || (model.nodes[link.b].node_id == node_id
                                    && model.nodes[link.b].mons[link.b_display].display_id
                                        == display_id)
                        }) {
                            ui.separator();
                            ui.label(bridge_description(model, link, lang));
                            if ui
                                .button(text(
                                    lang,
                                    if link.disabled {
                                        Key::EnableBridge
                                    } else {
                                        Key::DisableBridge
                                    },
                                ))
                                .clicked()
                            {
                                actions.push(Action::SetBridgeEnabled {
                                    link_id: link.id.clone(),
                                    enabled: link.disabled,
                                });
                            }
                        }
                    });
            });
        if !open {
            st.selected_display = None;
        }
    } else if let Some(id) = st.selected_bridge.clone() {
        let Some(link) = model.links.iter().find(|link| link.bridge_id == id) else {
            st.selected_bridge = None;
            return;
        };
        egui::Window::new(text(lang, Key::BridgeProperties))
            .id(egui::Id::new("bridge-properties"))
            .open(&mut open)
            .constrain_to(clip)
            .default_pos(clip.left_top() + Vec2::splat(12.0))
            .default_width(width)
            .max_width(width)
            .resizable(false)
            .collapsible(false)
            .show(ui.ctx(), |ui| {
                egui::ScrollArea::vertical()
                    .max_height((clip.height() - 80.0).max(40.0))
                    .show(ui, |ui| {
                        ui.label(bridge_description(model, link, lang));
                        ui.label(text(lang, Key::BridgeAllSegments));
                        if ui
                            .button(text(
                                lang,
                                if link.disabled {
                                    Key::EnableBridge
                                } else {
                                    Key::DisableBridge
                                },
                            ))
                            .clicked()
                        {
                            actions.push(Action::SetBridgeEnabled {
                                link_id: link.id.clone(),
                                enabled: link.disabled,
                            });
                        }
                    });
            });
        if !open {
            st.selected_bridge = None;
        }
    }
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

fn caption_rect(at: Pos2, node: &MapNode, scale: f32) -> Rect {
    Rect::from_min_size(
        Pos2::new(at.x, at.y + node.h as f32 * scale),
        Vec2::new(
            node.w as f32 * scale,
            (88.0
                + if node.game_guard_badge.is_empty() {
                    0.0
                } else {
                    64.0
                })
                * scale.clamp(0.8, 1.0),
        ),
    )
}

fn phantom_caption_rect(card: Rect, scale: f32) -> Rect {
    let fs = scale.clamp(0.8, 1.0);
    Rect::from_min_size(
        card.left_bottom(),
        Vec2::new(card.width().max(192.0 * fs), 88.0 * fs),
    )
}

/// Discovery is a view overlay, never a node in the editable layout or its fit bounds.
/// Keep real monitors at their chosen scale: a distant beacon must not shrink the workspace.
fn phantom_rects(
    model: &MapModel,
    shown: &[Pos2],
    scale: f32,
    display_scale: f64,
    viewport: Rect,
    count: usize,
) -> impl Iterator<Item = Rect> {
    let occupied = model
        .nodes
        .iter()
        .zip(shown)
        .map(|(node, &at)| node_rect(at, node, scale).union(caption_rect(at, node, scale)))
        .reduce(|a, b| a.union(b))
        .unwrap_or_else(|| Rect::from_min_size(Pos2::new(72.0, 100.0), Vec2::ZERO));
    let size = Vec2::new(1920.0, 1080.0) * (display_scale as f32 * scale);
    let caption = phantom_caption_rect(Rect::from_min_size(Pos2::ZERO, size), scale);
    let gap = 24.0;
    // Prefer a row below; an expanded pairing dock may leave room only on the right.
    let start = if occupied.height() <= occupied.width()
        && occupied.bottom() + gap + size.y + caption.height() <= viewport.bottom()
    {
        occupied.left_bottom() + Vec2::new(0.0, gap)
    } else {
        occupied.right_top() + Vec2::new(gap, 0.0)
    };
    (0..count).map(move |i| {
        Rect::from_min_size(
            start + Vec2::new(i as f32 * (caption.width() + gap), 0.0),
            size,
        )
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_phantoms(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    pal: &Palette,
    vm: &ViewModel,
    model: &MapModel,
    shown: &[Pos2],
    xf: Xf,
    art: &mut super::phantom::PhantomArt,
    actions: &mut Vec<Action>,
) -> bool {
    let mut peers: Vec<_> = vm.discovered_peers().collect();
    peers.sort_unstable_by(|a, b| a.node_id.cmp(&b.node_id));
    let display_scale = if vm.editor.scale > 0.0 {
        vm.editor.scale
    } else {
        crate::map::BASE_SCALE
    };
    let bounds = phantom_rects(
        model,
        shown,
        xf.s,
        display_scale,
        painter.clip_rect(),
        peers.len(),
    );
    let mut visible = false;
    for (peer, rect) in peers.into_iter().zip(bounds) {
        visible |= monitor_hit_rect(rect, painter.clip_rect()).is_some();
        phantom_ui(ui, painter, pal, vm, peer, rect, xf.s, art, actions);
    }
    visible
}

#[allow(clippy::too_many_arguments)]
fn phantom_ui(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    pal: &Palette,
    vm: &ViewModel,
    peer: &protocol::ipc::DiscoveredPeer,
    rect: Rect,
    scale: f32,
    art: &mut super::phantom::PhantomArt,
    actions: &mut Vec<Action>,
) {
    let caption = phantom_caption_rect(rect, scale);
    let Some(hit) = monitor_hit_rect(rect.union(caption), painter.clip_rect()) else {
        return;
    };
    let enabled = vm.modal.is_none() && !vm.appearance_open;
    let response = ui.interact(
        hit,
        egui::Id::new(("phantom", &peer.node_id)),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hovered = enabled && response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    painter.rect_filled(rect, 8.0, pal.bg2);
    art.paint(
        painter,
        rect,
        pal,
        &peer.node_id,
        peer.pairing_open,
        hovered,
    );
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    painter.extend(Shape::dashed_line(
        &corners,
        Stroke::new(1.0, if hovered { pal.ink2 } else { pal.line }),
        5.0,
        5.0,
    ));
    let lang = vm.look.language;
    let name = if peer.display_name.is_empty() {
        text(lang, Key::WithoutAName)
    } else {
        &peer.display_name
    };
    let system = if peer.system_name.is_empty() {
        text(lang, Key::UnknownSystem)
    } else {
        &peer.system_name
    };
    let rows = [
        (name.to_owned(), 13.0, pal.ink2),
        (format!("{} · {system}", peer.address), 11.0, pal.ink2),
        (text(lang, Key::PhantomNotPaired).to_owned(), 11.0, pal.ink2),
        (
            text(
                lang,
                if peer.pairing_open {
                    Key::PairingWindowOpen
                } else {
                    Key::PhantomDisplaysUnknown
                },
            )
            .to_owned(),
            10.5,
            if peer.pairing_open {
                pal.accent_ink
            } else {
                pal.ink2
            },
        ),
    ];
    let fs = scale.clamp(0.8, 1.0);
    let clipped = painter.with_clip_rect(caption.intersect(painter.clip_rect()));
    let mut y = caption.top() + 10.0 * fs;
    for (label, size, color) in rows {
        let mut job = egui::text::LayoutJob::simple(
            label,
            font(size * fs, fam::sans()),
            color,
            caption.width(),
        );
        job.wrap.max_rows = 1;
        let galley = clipped.layout_job(job);
        let height = galley.size().y;
        clipped.galley(Pos2::new(caption.left(), y), galley, color);
        y += height + fs;
    }
    egui::Popup::menu(&response).show(|ui| {
        if widgets::small_button(
            ui,
            pal,
            text(lang, Key::Connect),
            vm.connection.is_connected()
                && !vm.pairing.dialing
                && !vm
                    .outgoing_pair_request
                    .as_ref()
                    .is_some_and(|request| request.state == "waiting"),
        )
        .clicked()
        {
            actions.push(Action::connect_discovered(peer));
            ui.close();
        }
        if widgets::small_button(ui, pal, text(lang, Key::HideDiscovered), true).clicked() {
            actions.push(Action::HideDiscovered(peer.node_id.clone()));
            ui.close();
        }
    });
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

fn monitor_hit_rect(monitor: Rect, map: Rect) -> Option<Rect> {
    let visible = monitor.intersect(map);
    (visible.width() > 0.0 && visible.height() > 0.0).then_some(visible)
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
    caption_clip: Rect,
    enabled: bool,
    hovered: &mut Option<(usize, usize)>,
    actions: &mut Vec<Action>,
) {
    let s = xf.s;
    let fs = s.clamp(0.8, 1.0);
    let k = if node.online { 1.0 } else { 0.62 };
    // Use the fitted on-screen scale: small windows need the same decluttering as zooming out.
    let identifiers = ((s - 0.4) / 0.1).clamp(0.0, 1.0);
    let details = ((s - 0.75) / 0.15).clamp(0.0, 1.0);
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
        let Some(hit_rect) = monitor_hit_rect(rect, painter.clip_rect()) else {
            continue;
        };
        let id = egui::Id::new(("mon", &node.node_id, m));
        let response = ui.interact(
            hit_rect,
            id,
            if enabled {
                Sense::click_and_drag()
            } else {
                Sense::hover()
            },
        );
        response.clone().on_hover_text(node.app_version.clone());
        if enabled
            && (response.clicked()
                || response.secondary_clicked()
                || (response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
        {
            st.selected_display = Some((node.node_id.clone(), mon.display_id.clone()));
            st.selected_bridge = None;
        }
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
            *hovered = Some((index, 0));
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
        let clipped = painter.with_clip_rect(rect.shrink(2.0));
        let mut numbers = clipped.clone();
        numbers.multiply_opacity(((s - 0.3) / 0.1).clamp(0.0, 1.0));
        let num = numbers.layout_no_wrap(mon.num.clone(), font(10.5 * fs, fam::mono()), pal.ink);
        numbers.galley(rect.min + Vec2::new(9.0 * fs, 7.0 * fs), num, pal.ink);
        let mut clipped = clipped;
        clipped.multiply_opacity(details);
        if details > 0.0 && !mon.tag.is_empty() {
            let (ink, edge, fill) = if node.target {
                (pal.accent_ink, pal.accent, pal.accent_soft)
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
        if details > 0.0 {
            let res =
                clipped.layout_no_wrap(mon.res.clone(), font(10.5 * fs, fam::mono()), pal.ink);
            clipped.galley(
                Pos2::new(rect.min.x + 9.0 * fs, rect.max.y - 7.0 * fs - res.size().y),
                res,
                pal.ink,
            );
        }
    }

    // The caption: name, system line, who has the keyboard.
    if identifiers <= 0.0 {
        return;
    }
    // A long hostname/subtitle must not extend into a neighbouring computer's caption.
    let caption_rect = caption_rect(at, node, s);
    let mut painter = painter.with_clip_rect(
        caption_rect
            .intersect(caption_clip)
            .intersect(painter.clip_rect()),
    );
    painter.multiply_opacity(identifiers);
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
    if details <= 0.0 {
        return;
    }
    painter.multiply_opacity(details);
    for badge_text in [&node.game_guard_badge, &node.update_badge]
        .into_iter()
        .filter(|text| !text.is_empty())
    {
        let padding = Vec2::new(6.0 * fs, 2.5 * fs);
        let badge = painter.layout(
            badge_text.clone(),
            font(10.5 * fs, fam::sans()),
            fade(pal.accent_ink, k),
            (painter.clip_rect().width() - padding.x * 2.0).max(1.0),
        );
        let size = badge.size() + padding * 2.0;
        let rect = Rect::from_min_size(Pos2::new(at.x, y), size);
        // A tight vertical stack must not leave half a pill on the next card.
        if painter.clip_rect().contains_rect(rect) {
            painter.rect(
                rect,
                (size.y / 2.0) as u8,
                pal.card,
                Stroke::new(1.0, pal.line),
                StrokeKind::Inside,
            );
            painter.galley(rect.min + padding, badge, pal.accent_ink);
        }
        y += size.y + 1.0;
    }
    for (text, color, family) in [
        (node.os.as_str(), pal.ink2, fam::mono()),
        (
            node.focus.as_str(),
            if node.target {
                pal.accent_ink
            } else {
                pal.ink2
            },
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
    if let Some(outcome) = vm
        .outcome
        .as_ref()
        .filter(|_| vm.modal != Some(Modal::Clip))
    {
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
            pal.accent_ink,
            (rect.width() - 92.0).max(40.0),
        );
        let size = galley.size();
        ui.painter()
            .galley(Pos2::new(center - size.x / 2.0, y), galley, pal.accent_ink);
        y += size.y + 4.0;
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    #[test]
    fn resize_preview_releases_once_and_focus_loss_cancels() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        let model = owner_layout();
        let ctx = egui::Context::default();
        let clip = Rect::from_min_size(Pos2::ZERO, Vec2::splat(1000.0));
        let xf = Xf {
            origin: Pos2::ZERO,
            s: 1.0,
        };
        let shown: Vec<_> = model.nodes.iter().map(|n| xf.p(n.at.x, n.at.y)).collect();
        let mut st = UiState::new(std::time::Instant::now());
        let drag = super::super::DisplayResize {
            node_id: model.nodes[0].node_id.clone(),
            display_id: model.nodes[0].mons[0].display_id.clone(),
            grab: Pos2::new(100.0, 100.0),
            width: 144.0,
            aspect: 16.0 / 9.0,
            move_start: None,
        };
        st.display_resize = Some(drag.clone());
        let mut actions = Vec::new();
        for pressed in [true, false] {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(clip),
                    focused: true,
                    events: vec![egui::Event::PointerButton {
                        pos: Pos2::new(120.0, 110.0),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    }],
                    ..Default::default()
                },
                |ui| {
                    display_resize_interaction(
                        ui,
                        &vm,
                        &mut st,
                        &model,
                        &shown,
                        xf,
                        clip,
                        &mut actions,
                    );
                },
            )
            .drop_without_applying_deltas();
        }
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::ResizeDisplay { width, .. } if *width > 144.0)));
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::DropDisplayResize))
                .count(),
            1
        );
        assert!(st.drag.is_none() && st.paddle_drag.is_none() && st.display_resize.is_none());
        st.display_resize = Some(drag);
        actions.clear();
        ctx.run_ui(
            egui::RawInput {
                focused: false,
                ..Default::default()
            },
            |ui| {
                display_resize_interaction(
                    ui,
                    &vm,
                    &mut st,
                    &model,
                    &shown,
                    xf,
                    clip,
                    &mut actions,
                );
            },
        )
        .drop_without_applying_deltas();
        assert!(matches!(actions.as_slice(), [Action::CancelDisplayResize]));
    }

    #[test]
    fn display_move_preview_uses_absolute_stage_coordinates_and_shared_drop() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        let model = owner_layout();
        let ctx = egui::Context::default();
        let clip = Rect::from_min_size(Pos2::ZERO, Vec2::splat(1000.0));
        let xf = Xf {
            origin: Pos2::ZERO,
            s: 0.5,
        };
        let shown: Vec<_> = model.nodes.iter().map(|n| xf.p(n.at.x, n.at.y)).collect();
        let mut st = UiState::new(std::time::Instant::now());
        st.display_resize = Some(super::super::DisplayResize {
            node_id: model.nodes[0].node_id.clone(),
            display_id: model.nodes[0].mons[0].display_id.clone(),
            grab: Pos2::new(100.0, 100.0),
            width: 144.0,
            aspect: 16.0 / 9.0,
            move_start: Some(crate::map::Pt::new(-30.0, 40.0)),
        });
        let mut actions = Vec::new();
        for pressed in [true, false] {
            ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(clip),
                    focused: true,
                    events: vec![egui::Event::PointerButton {
                        pos: Pos2::new(120.0, 110.0),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::default(),
                    }],
                    ..Default::default()
                },
                |ui| {
                    display_resize_interaction(
                        ui,
                        &vm,
                        &mut st,
                        &model,
                        &shown,
                        xf,
                        clip,
                        &mut actions,
                    );
                },
            )
            .drop_without_applying_deltas();
        }
        assert!(actions
            .iter()
            .any(|a| matches!(a, Action::MoveDisplay { x, y, .. } if *x == 10.0 && *y == 60.0)));
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::DropDisplayResize))
                .count(),
            1
        );
        assert!(!actions
            .iter()
            .any(|a| matches!(a, Action::ResizeDisplay { .. } | Action::MoveNode { .. })));
        assert!(st.drag.is_none() && st.paddle_drag.is_none() && st.display_resize.is_none());
    }

    #[test]
    fn bridge_copy_rounds_pixels_and_keeps_each_endpoint_together() {
        let mut model = owner_layout();
        model.links[0].a_logical_length = 1071.0;
        model.links[0].b_logical_length = 20.0;
        for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
            let description = bridge_description(&model, &model.links[0], lang);
            assert!(description.contains("1071 / 20"));
            let endpoints: Vec<_> = description.lines().take(2).collect();
            assert!(endpoints[1].starts_with("↔\u{a0}"));
            assert!(endpoints.iter().all(|line| !line.contains(' ')));
        }
    }

    #[test]
    fn transient_popup_bounds_leave_the_map_heading_uncovered() {
        let clip = Rect::from_min_max(Pos2::new(0.0, 60.0), Pos2::new(836.0, 609.0));
        let bounds = popup_bounds(clip);
        assert_eq!(bounds.top(), 190.0);
        assert!(clip.contains_rect(bounds));
    }

    #[test]
    fn disabled_bridge_is_dashed_in_both_styles_with_all_fragment_endpoints() {
        let ctx = egui::Context::default();
        let mut model = owner_layout();
        model.links[0].disabled = true;
        model.links[0].bridge_id = "pair".into();
        let mut second = model.links[0].clone();
        second.id = "fragment-two".into();
        second.a_span.0 += 10.0;
        model.links.push(second);
        let shown: Vec<_> = model
            .nodes
            .iter()
            .map(|n| Pos2::new(n.at.x as f32, n.at.y as f32))
            .collect();
        let pal = Palette::of(crate::gui_settings::Theme::Dark);
        let mut st = UiState::new(std::time::Instant::now());
        st.selected_bridge = Some("pair".into());
        let render = |style| {
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let xf = Xf {
                    origin: Pos2::ZERO,
                    s: 1.0,
                };
                draw_link(
                    ui.painter(),
                    &pal,
                    &model,
                    &model.links[0],
                    &shown,
                    xf,
                    style,
                    &mut super::super::field::Field::new(),
                );
                paint_bridge_endpoints(ui.painter(), &pal, &st, &model, &shown, xf);
            });
            let shapes = format!("{:?}", output.shapes);
            output.drop_without_applying_deltas();
            shapes
        };
        assert_eq!(render(LinkStyle::Ascii), render(LinkStyle::Line));
    }

    #[test]
    fn diagonal_accepts_local_decimal_separator_without_clamping() {
        for value in ["3", "120", " 27,5 ", "27.5"] {
            assert!(parse_diagonal(value).is_some(), "{value}");
        }
        for value in ["", "NaN", "inf", "-24", "2.99", "120.01", "1,2.3"] {
            assert_eq!(parse_diagonal(value), None, "{value}");
        }
    }

    #[test]
    fn bridge_hit_target_covers_touching_and_narrow_segments() {
        let a = Pos2::new(100.0, 100.0);
        assert_eq!(segment_distance(a + Vec2::new(4.0, 0.0), [a, a]), 4.0);
        assert_eq!(
            segment_distance(a + Vec2::new(5.0, 1.0), [a, a + Vec2::Y * 2.0]),
            5.0
        );
        let clip = Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0));
        let handle = resize_handle(
            Rect::from_min_size(Pos2::new(90.0, 90.0), Vec2::splat(20.0)),
            clip,
        )
        .unwrap();
        assert!(clip.contains_rect(handle));
    }

    #[test]
    fn oversized_and_negative_map_bounds_fit_without_changing_model() {
        let mut model = owner_layout();
        model.nodes[0].at = crate::map::Pt::new(-500.0, -100.0);
        model.nodes[0].w = 3000.0;
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 640.0));
        let xf = Xf::new(viewport, 100).fit(viewport, &model);
        let node = &model.nodes[0];
        assert!(viewport.contains(xf.p(node.at.x, node.at.y)));
        assert!(viewport.contains(xf.p(node.at.x + node.w, node.at.y + node.h)));
    }

    #[test]
    fn real_name_caption_clipping_is_independent_of_phantom_placement() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let mut node = owner_layout().nodes.remove(0);
        node.name = "WORKSTATION".into();
        node.w = 144.0;
        node.h = 81.0;
        let at = Pos2::new(530.0, 180.0);
        let scale = 0.45;
        let card = node_rect(at, &node, scale);
        let caption = caption_rect(at, &node, scale);
        let mut st = UiState::new(std::time::Instant::now());
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            node_ui(
                ui,
                &painter,
                &Palette::of(crate::gui_settings::Theme::Dark),
                &mut st,
                &node,
                0,
                at,
                Xf {
                    origin: Pos2::ZERO,
                    s: scale,
                },
                caption,
                false,
                &mut None,
                &mut Vec::new(),
            );
        });
        let clipped_name = output.shapes.iter().find_map(|shape| {
            if let Shape::Text(label) = &shape.shape {
                (label.galley.text() == node.name).then(|| {
                    (
                        shape.clip_rect,
                        Rect::from_min_size(label.pos, label.galley.size()),
                    )
                })
            } else {
                None
            }
        });
        output.drop_without_applying_deltas();
        let (clip, glyphs) =
            clipped_name.expect("Real name is drawn without any discovery overlay");
        assert_eq!(clip.right(), card.right());
        assert!(
            glyphs.right() > clip.right(),
            "Existing real caption clips at its monitor width"
        );
    }

    #[test]
    fn phantom_repaint_requires_a_visible_card_and_active_window() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        vm.on_connected("test");
        vm.look.field = crate::gui_settings::FieldMode::Off;
        vm.status = Some(protocol::ipc::Status {
            discovery_enabled: true,
            discovered_peers: vec![protocol::ipc::DiscoveredPeer {
                node_id: "cc".repeat(32),
                display_name: "HTPC".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        for (visible, active) in [(true, true), (true, false), (false, true), (false, false)] {
            let ctx = egui::Context::default();
            super::super::install_fonts(&ctx);
            let now = std::time::Instant::now();
            let mut state = UiState::new(now);
            vm.look.hidden_discovered = Default::default();
            if !visible {
                vm.look.hidden_discovered.insert(&"cc".repeat(32));
            }
            let mut repaint = None;
            let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                repaint = draw(
                    ui,
                    &Palette::of(vm.look.theme),
                    &vm,
                    &mut state,
                    Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 720.0)),
                    Env {
                        now,
                        animate: active,
                    },
                    0.0,
                    &mut Vec::new(),
                );
            });
            output.drop_without_applying_deltas();
            assert_eq!(repaint, (visible && active).then_some(FIELD_FRAME));
        }
    }

    #[test]
    fn phantom_placeholder_matches_a_real_1080p_display_at_every_map_scale() {
        for display_scale in [crate::map::BASE_SCALE, crate::map::BASE_SCALE / 3.0] {
            for zoom in [0.45, 0.8, 1.0, 1.5, 3.0] {
                let phantom = phantom_rects(
                    &MapModel::default(),
                    &[],
                    zoom,
                    display_scale,
                    Rect::EVERYTHING,
                    1,
                )
                .next()
                .unwrap();
                let real = monitor_rect(
                    Pos2::ZERO,
                    crate::map::Rect::new(0.0, 0.0, 1920.0 * display_scale, 1080.0 * display_scale),
                    zoom,
                );
                assert!((phantom.width() - real.width()).abs() < 0.001);
                assert!((phantom.height() - real.height()).abs() < 0.001);
            }
        }
    }

    #[test]
    fn expanded_pairing_dock_parks_phantoms_in_visible_space_to_the_right() {
        let mut model = owner_layout();
        for node in &mut model.nodes {
            node.w = 140.0;
            node.h = 80.0;
        }
        let shown = [Pos2::new(530.0, 180.0), Pos2::new(627.0, 180.0)];
        let viewport = Rect::from_min_max(Pos2::new(0.0, 60.0), Pos2::new(1216.0, 330.0));
        let phantoms: Vec<_> =
            phantom_rects(&model, &shown, 0.45, crate::map::BASE_SCALE, viewport, 2).collect();
        for phantom in phantoms {
            let phantom = phantom.union(phantom_caption_rect(phantom, 0.45));
            assert!(viewport.contains_rect(phantom));
            for (node, &at) in model.nodes.iter().zip(&shown) {
                assert!(!phantom.intersects(node_rect(at, node, 0.45)));
                assert!(!phantom.intersects(caption_rect(at, node, 0.45)));
            }
        }
    }

    #[test]
    fn phantom_connect_click_uses_the_shared_pairing_request_command() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        vm.on_connected("test");
        vm.look.language = ui_text::Lang::En;
        let peer = protocol::ipc::DiscoveredPeer {
            node_id: "cc".repeat(32),
            display_name: "HTPC".into(),
            address: "192.168.1.123:24800".into(),
            pairing_open: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let rect = Rect::from_min_size(Pos2::new(30.0, 30.0), Vec2::new(240.0, 135.0));
        let pal = Palette::of(vm.look.theme);
        let popup = egui::Id::new(("phantom", &peer.node_id)).with("popup");
        egui::Popup::open_id(&ctx, popup);
        let mut actions = Vec::new();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            phantom_ui(
                ui,
                &painter,
                &pal,
                &vm,
                &peer,
                rect,
                1.0,
                &mut super::super::phantom::PhantomArt::default(),
                &mut actions,
            );
        });
        output.drop_without_applying_deltas();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            phantom_ui(
                ui,
                &painter,
                &pal,
                &vm,
                &peer,
                rect,
                1.0,
                &mut super::super::phantom::PhantomArt::default(),
                &mut actions,
            );
        });
        let connect = output.shapes.iter().find_map(|shape| {
            if let Shape::Text(label) = &shape.shape {
                (label.galley.text() == "Connect")
                    .then(|| Rect::from_min_size(label.pos, label.galley.size()).center())
            } else {
                None
            }
        });
        output.drop_without_applying_deltas();
        let connect = connect.expect("The phantom action popup offers Connect");
        for pressed in [true, false] {
            let output = ctx.run_ui(
                egui::RawInput {
                    events: vec![
                        egui::Event::PointerMoved(connect),
                        egui::Event::PointerButton {
                            pos: connect,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    ..Default::default()
                },
                |ui| {
                    let painter = ui.painter().clone();
                    phantom_ui(
                        ui,
                        &painter,
                        &pal,
                        &vm,
                        &peer,
                        rect,
                        1.0,
                        &mut super::super::phantom::PhantomArt::default(),
                        &mut actions,
                    );
                },
            );
            output.drop_without_applying_deltas();
        }
        assert_eq!(actions, vec![Action::connect_discovered(&peer)]);
    }

    #[test]
    fn phantom_order_is_node_id_order_not_beacon_arrival_order() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        vm.on_connected("test");
        vm.status = Some(protocol::ipc::Status {
            discovery_enabled: true,
            discovered_peers: ["bb", "aa"]
                .map(|id| protocol::ipc::DiscoveredPeer {
                    node_id: id.repeat(32),
                    display_name: id.to_owned(),
                    ..Default::default()
                })
                .to_vec(),
            ..Default::default()
        });
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let model = vm.map_model();
        let before = model.clone();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            draw_phantoms(
                ui,
                &painter,
                &Palette::of(vm.look.theme),
                &vm,
                &model,
                &[],
                Xf {
                    origin: Pos2::ZERO,
                    s: 1.0,
                },
                &mut super::super::phantom::PhantomArt::default(),
                &mut Vec::new(),
            );
        });
        let names: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let Shape::Text(label) = &shape.shape {
                    ["aa", "bb"]
                        .contains(&label.galley.text())
                        .then(|| (label.galley.text().to_owned(), label.pos))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0, "aa");
        assert_eq!(names[1].0, "bb");
        assert!(names[0].1.x < names[1].1.x);
        assert_eq!(model, before);
        assert!(model.nodes.is_empty() && model.links.is_empty());
        output.drop_without_applying_deltas();
    }

    #[test]
    fn phantom_placement_avoids_real_monitors_and_captions_in_every_orientation() {
        for vertical in [false, true] {
            for count in [2, 3] {
                let mut model = owner_layout();
                while model.nodes.len() < count {
                    model.nodes.push(model.nodes[0].clone());
                }
                for (i, node) in model.nodes.iter_mut().enumerate() {
                    node.at = if vertical {
                        crate::map::Pt::new(100.0, i as f64 * 300.0)
                    } else {
                        crate::map::Pt::new(i as f64 * 400.0, 100.0)
                    };
                }
                for scale in [0.45, 0.8, 1.0, 1.5, 3.0] {
                    let shown: Vec<_> = model
                        .nodes
                        .iter()
                        .map(|n| Pos2::new(n.at.x as f32 * scale, n.at.y as f32 * scale))
                        .collect();
                    let before = model.clone();
                    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 720.0));
                    let phantoms: Vec<_> =
                        phantom_rects(&model, &shown, scale, crate::map::BASE_SCALE, viewport, 4)
                            .collect();
                    assert_eq!(
                        phantoms,
                        phantom_rects(&model, &shown, scale, crate::map::BASE_SCALE, viewport, 4)
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(before, model, "Discovery cannot mutate real nodes or links");
                    for (i, phantom) in phantoms.iter().enumerate() {
                        let phantom = phantom.union(phantom_caption_rect(*phantom, scale));
                        for (node, &at) in model.nodes.iter().zip(&shown) {
                            assert!(!phantom.intersects(node_rect(at, node, scale)));
                            assert!(!phantom.intersects(caption_rect(at, node, scale)));
                        }
                        for other in &phantoms[..i] {
                            assert!(!phantom
                                .intersects(other.union(phantom_caption_rect(*other, scale))));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn phantom_caption_rows_never_overlap_in_either_language_or_theme() {
        use crate::view_model::{CoreEndpoint, LocalConfig};
        for theme in [
            crate::gui_settings::Theme::Light,
            crate::gui_settings::Theme::Dark,
        ] {
            for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
                for pairing_open in [false, true] {
                    let mut vm = ViewModel::new(
                        CoreEndpoint::Ready {
                            description: String::new(),
                        },
                        LocalConfig::default(),
                    );
                    vm.look.language = lang;
                    let peer = protocol::ipc::DiscoveredPeer {
                        node_id: "cc".repeat(32),
                        display_name: "A deliberately very long discovered computer name".into(),
                        address: "192.168.1.123:24800".into(),
                        system_name: "A deliberately very long operating system caption".into(),
                        pairing_open,
                        ..Default::default()
                    };
                    let ctx = egui::Context::default();
                    super::super::install_fonts(&ctx);
                    let rect = Rect::from_min_size(Pos2::new(20.0, 20.0), Vec2::new(64.8, 36.45));
                    let caption = phantom_caption_rect(rect, 0.45);
                    let pal = Palette::of(theme);
                    let output = ctx.run_ui(egui::RawInput::default(), |ui| {
                        let painter = ui.painter().clone();
                        phantom_ui(
                            ui,
                            &painter,
                            &pal,
                            &vm,
                            &peer,
                            rect,
                            0.45,
                            &mut super::super::phantom::PhantomArt::default(),
                            &mut Vec::new(),
                        );
                    });
                    let texts: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| {
                            if let Shape::Text(text) = &shape.shape {
                                (text.pos.y >= caption.top())
                                    .then(|| Rect::from_min_size(text.pos, text.galley.size()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    assert_eq!(texts.len(), 4);
                    for (i, text) in texts.iter().enumerate() {
                        assert!(caption.contains_rect(*text));
                        assert!(!rect.intersects(*text));
                        for other in &texts[..i] {
                            assert!(!text.intersects(*other));
                        }
                    }
                    output.drop_without_applying_deltas();
                }
            }
        }
    }

    #[test]
    fn monitor_hit_rect_excludes_clipped_and_empty_monitor_areas() {
        let map = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(300.0, 300.0));
        let monitor = Rect::from_min_max(Pos2::new(50.0, 150.0), Pos2::new(200.0, 250.0));
        let visible = monitor_hit_rect(monitor, map).expect("partially visible monitor");
        assert_eq!(
            visible,
            Rect::from_min_max(Pos2::new(100.0, 150.0), Pos2::new(200.0, 250.0))
        );
        assert!(!visible.contains(Pos2::new(75.0, 200.0)));
        assert!(visible.contains(Pos2::new(150.0, 200.0)));
        assert_eq!(monitor_hit_rect(visible, map), Some(visible));
        for right in [99.0, 100.0] {
            let outside = Rect::from_min_max(Pos2::new(50.0, 150.0), Pos2::new(right, 250.0));
            assert_eq!(monitor_hit_rect(outside, map), None);
        }
        let below = Rect::from_min_max(Pos2::new(150.0, 300.0), Pos2::new(250.0, 350.0));
        assert_eq!(monitor_hit_rect(below, map), None);
    }

    #[test]
    fn monitor_pointer_interaction_stays_inside_the_map_clip() {
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 500.0));
        let map = Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(400.0, 400.0));
        for (at, pointer, interactive) in [
            (Pos2::new(50.0, 150.0), Pos2::new(75.0, 200.0), false),
            (Pos2::new(50.0, 150.0), Pos2::new(150.0, 200.0), true),
            (Pos2::new(-150.0, 150.0), Pos2::new(25.0, 200.0), false),
        ] {
            let ctx = egui::Context::default();
            super::super::install_fonts(&ctx);
            let mut st = UiState::new(std::time::Instant::now());
            let mut node = owner_layout().nodes.remove(0);
            node.mons.truncate(1);
            node.mons[0].rect = crate::map::Rect::new(0.0, 0.0, 200.0, 100.0);
            let pal = Palette::of(Look::default().theme);
            let xf = Xf {
                origin: Pos2::ZERO,
                s: 1.0,
            };
            let mut moved = false;
            for (frame, position, button) in [
                (0, pointer, None),
                (1, pointer, None),
                (2, pointer, Some(true)),
                (3, pointer + Vec2::new(20.0, 0.0), None),
                (4, pointer + Vec2::new(20.0, 0.0), Some(false)),
            ] {
                let mut events = vec![egui::Event::PointerMoved(position)];
                if let Some(pressed) = button {
                    events.push(egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                let mut hovered = None;
                let mut actions = Vec::new();
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        time: Some(f64::from(frame) * 0.05),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let painter = ui.painter().with_clip_rect(map);
                        node_ui(
                            ui,
                            &painter,
                            &pal,
                            &mut st,
                            &node,
                            0,
                            at,
                            xf,
                            map,
                            true,
                            &mut hovered,
                            &mut actions,
                        );
                    },
                )
                .drop_without_applying_deltas();
                if frame == 1 {
                    assert_eq!(hovered.is_some(), interactive, "pointer at {pointer:?}");
                }
                moved |= actions
                    .iter()
                    .any(|action| matches!(action, Action::MoveNode { .. }));
            }
            assert_eq!(moved, interactive, "drag starting at {pointer:?}");
            assert_eq!(st.drag.is_some(), interactive);
        }
    }

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
                system_name: String::new(),
                online: true,
                displays: vec![EditorDisplay {
                    id: "screen".into(),
                    x: 0.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                    primary: true,
                    native_known: true,
                    native_w: 1920.0,
                    native_h: 1080.0,
                    scale: 1.0,
                    rotation: 0,
                    physical_size: None,
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
            for zoom in [70, 100, 125, 150, 300] {
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
                app_version: String::new(),
                update_badge: String::new(),
                game_guard_badge: String::new(),
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
                        display_id: "fixture".to_owned(),
                        diagonal_inches: None,
                        size_source: "unknown",
                        aspect: rect.w / rect.h,
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
            bridge_id: "legacy".to_owned(),
            disabled: false,
            short_candidate: false,
            a_logical_length: bottom - top,
            b_logical_length: bottom - top,
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
    fn rendered_rtt_pill_never_intersects_horizontal_or_vertical_captions() {
        for theme in [
            crate::gui_settings::Theme::Light,
            crate::gui_settings::Theme::Dark,
        ] {
            for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
                for vertical in [false, true] {
                    let ctx = egui::Context::default();
                    super::super::install_fonts(&ctx);
                    let pal = Palette::of(theme);
                    let mut st = UiState::new(std::time::Instant::now());
                    let mut model = owner_layout();
                    for node in &mut model.nodes {
                        node.w = 200.0;
                        node.h = 100.0;
                        node.os = "this computer · Windows 11".into();
                        node.update_badge = "Version 0.1.7 is available".into();
                        node.mons.truncate(1);
                        node.mons[0].rect = crate::map::Rect::new(0.0, 0.0, 200.0, 100.0);
                    }
                    let shown = if vertical {
                        [Pos2::new(300.0, 80.0), Pos2::new(300.0, 252.0)]
                    } else {
                        [Pos2::new(100.0, 100.0), Pos2::new(400.0, 100.0)]
                    };
                    let center = if vertical {
                        Pos2::new(400.0, 216.0)
                    } else {
                        Pos2::new(350.0, 150.0)
                    };
                    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 850.0));
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(screen),
                            ..Default::default()
                        },
                        |ui| {
                            let painter = ui.painter().clone();
                            for (index, node) in model.nodes.iter().enumerate() {
                                node_ui(
                                    ui,
                                    &painter,
                                    &pal,
                                    &mut st,
                                    node,
                                    index,
                                    shown[index],
                                    Xf {
                                        origin: Pos2::ZERO,
                                        s: 1.0,
                                    },
                                    screen,
                                    false,
                                    &mut None,
                                    &mut Vec::new(),
                                );
                            }
                            st.bridge.set_latency(&painter, lang, Some(12_000));
                            st.bridge.paint_latency(
                                &painter,
                                center,
                                &pal,
                                model.nodes.iter().zip(&shown).map(|(node, &at)| {
                                    node_rect(at, node, 1.0).union(caption_rect(at, node, 1.0))
                                }),
                            );
                        },
                    );
                    output.textures_delta.clear();
                    let label = output
                        .shapes
                        .iter()
                        .position(|shape| {
                            matches!(&shape.shape, Shape::Text(text)
                                if text.galley.text().starts_with('≈'))
                        })
                        .expect("the roomy map has a free spot for the RTT");
                    let (pill_index, pill) = output.shapes[..label]
                        .iter()
                        .enumerate()
                        .rev()
                        .find_map(|(index, shape)| match &shape.shape {
                            Shape::Rect(rect) => Some((index, rect.rect)),
                            _ => None,
                        })
                        .expect("RTT backing precedes its label");
                    for (index, shape) in output.shapes.iter().enumerate() {
                        if index == label || index == pill_index {
                            continue;
                        }
                        let occupied = match &shape.shape {
                            Shape::Text(text) => Rect::from_min_size(text.pos, text.galley.size()),
                            Shape::Rect(rect) => rect.rect,
                            _ => continue,
                        }
                        .intersect(shape.clip_rect);
                        assert!(
                            !pill.intersects(occupied),
                            "RTT {pill:?} overlaps caption/card {occupied:?}, vertical={vertical}"
                        );
                    }
                    assert!(screen.contains_rect(pill));
                }
            }
        }
    }

    #[test]
    fn update_badge_has_an_opaque_pill_inside_the_available_caption_gap() {
        for theme in [
            crate::gui_settings::Theme::Light,
            crate::gui_settings::Theme::Dark,
        ] {
            for gap in [72.0, 28.0] {
                let ctx = egui::Context::default();
                super::super::install_fonts(&ctx);
                let pal = Palette::of(theme);
                let mut st = UiState::new(std::time::Instant::now());
                let mut node = owner_layout().nodes.remove(1);
                node.update_badge = "Version 0.1.7 is available".into();
                let at = Pos2::new(100.0, 100.0);
                let caption = Rect::from_min_size(
                    at + Vec2::new(0.0, node.h as f32),
                    Vec2::new(node.w as f32, gap),
                );
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let painter = ui.painter().clone();
                    node_ui(
                        ui,
                        &painter,
                        &pal,
                        &mut st,
                        &node,
                        0,
                        at,
                        Xf {
                            origin: Pos2::ZERO,
                            s: 1.0,
                        },
                        caption,
                        false,
                        &mut None,
                        &mut Vec::new(),
                    );
                });
                output.textures_delta.clear();
                let badge =
                    output
                        .shapes
                        .iter()
                        .enumerate()
                        .find_map(|(i, shape)| match &shape.shape {
                            Shape::Text(text) if text.galley.text() == node.update_badge => {
                                Some((i, Rect::from_min_size(text.pos, text.galley.size())))
                            }
                            _ => None,
                        });
                if gap < 40.0 {
                    assert!(badge.is_none(), "Never draw a clipped half-pill");
                    continue;
                }
                let (index, glyphs) = badge.expect("badge fits below the name");
                assert!(
                    output.shapes[..index].iter().any(|shape| {
                        matches!(&shape.shape, Shape::Rect(surface)
                        if surface.fill == pal.card
                            && surface.stroke == Stroke::new(1.0, pal.line)
                            && surface.rect.contains_rect(glyphs)
                            && caption.contains_rect(surface.rect)
                            && shape.clip_rect.contains_rect(surface.rect))
                    }),
                    "Badge needs its own opaque surface and hairline before the text"
                );
            }
        }
    }

    #[test]
    fn game_guard_pill_remains_visible_under_a_portrait_monitor() {
        for theme in [
            crate::gui_settings::Theme::Light,
            crate::gui_settings::Theme::Dark,
        ] {
            for lang in [ui_text::Lang::Ru, ui_text::Lang::En] {
                for reason in [
                    "fullscreen",
                    "cursor_confined",
                    "presentation",
                    "app_list",
                    "always",
                ] {
                    let ctx = egui::Context::default();
                    super::super::install_fonts(&ctx);
                    let pal = Palette::of(theme);
                    let mut st = UiState::new(std::time::Instant::now());
                    let mut node = owner_layout().nodes.remove(1);
                    node.w = 86.0;
                    node.h = 180.0;
                    node.name = "LOCAL".into();
                    node.update_badge.clear();
                    node.game_guard_badge = ui_text::game_guard_state(lang, reason);
                    let at = Pos2::new(60.0, 60.0);
                    let caption = caption_rect(at, &node, 1.0);
                    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                        let painter = ui.painter().clone();
                        node_ui(
                            ui,
                            &painter,
                            &pal,
                            &mut st,
                            &node,
                            0,
                            at,
                            Xf {
                                origin: Pos2::ZERO,
                                s: 1.0,
                            },
                            caption,
                            false,
                            &mut None,
                            &mut Vec::new(),
                        );
                    });
                    output.textures_delta.clear();
                    let glyphs = output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            Shape::Text(text) if text.galley.text() == node.game_guard_badge => {
                                Some(Rect::from_min_size(text.pos, text.galley.size()))
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("missing portrait guard pill: {lang:?}, {reason}, caption {caption:?}"));
                    assert!(caption.contains_rect(glyphs));
                }
            }
        }
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
                            look: &look,
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
            system_name: String::new(),
            online: true,
            displays: vec![EditorDisplay {
                id: "zone-0".to_owned(),
                x: 0.0,
                y: 0.0,
                w: 1745.0,
                h: 1091.0,
                primary: true,
                native_known: false,
                native_w: 0.0,
                native_h: 0.0,
                scale: 1.0,
                rotation: 0,
                physical_size: None,
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
