//! «Экраны» — the layout editor (KVM-077).
//!
//! The model is the core's [`protocol::ipc::TopologyDoc`]: every member's displays with the
//! logical bounds their own agent reported. The editor draws them, lets the user say which
//! side of this computer the other one is on (and drag the other computer's rectangle), and
//! sends the whole document back as `UpdateTopology` — the coordinator applies it or refuses
//! it with `LAYOUT_CONFLICT` (ADR-013: one document, whole-document compare-and-swap).
//!
//! Sizes are never invented: a display whose `native_known` is false is drawn as the
//! approximation it is and labelled as one.

use eframe::egui;

use super::{block, hint, important, warn_color};
use crate::view_model::{Action, Side, ViewModel};

/// The canvas height; the width follows the window.
const CANVAS_HEIGHT: f32 = 260.0;

/// The smallest a rectangle is drawn, so a display with unknown bounds stays grabbable.
const MIN_RECT: f32 = 36.0;

/// Draws the layout screen.
pub fn draw(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    if !vm.editor.has_document() {
        block(ui, "Раскладка", |ui| {
            ui.label("Служба ещё не прислала раскладку");
            if ui.button("Запросить раскладку").clicked() {
                actions.push(Action::Refresh);
            }
        });
        return actions;
    }

    actions.extend(state_block(vm, ui));
    actions.extend(conflict_block(vm, ui));
    actions.extend(pending_block(vm, ui));

    block(ui, "Компьютеры и их экраны", |ui| {
        actions.extend(pickers(vm, ui));
        for node in vm
            .editor
            .nodes
            .iter()
            .filter(|node| node.displays.is_empty())
        {
            important(
                ui,
                &format!("Служба не получила список мониторов от {}", node.label()),
            );
        }
        actions.extend(canvas(vm, ui));
        hint(
            ui,
            "Серый прямоугольник можно перетаскивать: так экраны встают рядом",
        );
        hint(ui, "Пунктирная рамка — размер известен приблизительно");
    });

    actions.extend(side_block(vm, ui));
    let auto = vm.auto_link_lines();
    if !auto.is_empty() {
        block(
            ui,
            "Связи со всем рабочим столом",
            |ui| {
                for line in &auto {
                    ui.label(line);
                }
                hint(
                    ui,
                    "Связь следует за мониторами: выключенный или новый экран её не ломает",
                );
            },
        );
    }
    let inactive = vm.inactive_link_lines();
    if !inactive.is_empty() {
        block(ui, "Неактивные связи", |ui| {
            for line in &inactive {
                important(ui, line);
            }
            hint(
                ui,
                "Связь станет активной, когда её экран снова появится в системе",
            );
        });
    }
    preview(vm, ui);
    actions
}

/// «Применено: ревизия N от …» and the two reasons editing may be off.
fn state_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Раскладка", |ui| {
        ui.label(format!(
            "Применено: ревизия {}, автор — {}",
            vm.editor.revision,
            author_text(&vm.editor.author)
        ));
        if !vm.editor.editable {
            important(ui, "Правка недоступна: нет координатора (владельца)");
        }
        ui.horizontal(|ui| {
            if ui.button("Обновить").clicked() {
                actions.push(Action::Refresh);
            }
            if ui.button("Список непринятых правок").clicked() {
                actions.push(Action::ListPendingEdits);
            }
        });
    });
    actions
}

/// `LAYOUT_CONFLICT`: what to do about a document somebody else moved.
fn conflict_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(conflict) = &vm.conflict else {
        return actions;
    };
    block(ui, "Раскладку изменили", |ui| {
        important(ui, conflict);
        if ui.button("Обновить").clicked() {
            actions.push(Action::Refresh);
        }
    });
    actions
}

/// The divergent edit kept after a partition (`layout.pending.toml`).
fn pending_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    if vm.pending_edits.is_empty() {
        return actions;
    }
    block(
        ui,
        "Непринятая правка после разрыва связи",
        |ui| {
            ui.label("Эта правка не попала в общую раскладку. Её можно применить или отбросить.");
            for edit in &vm.pending_edits {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Основа: ревизия {}, автор — {}",
                        edit.base_revision,
                        author_text(&edit.base_author)
                    ));
                    if ui
                        .button("Применить")
                        .on_hover_text("Попросить координатора применить эту правку")
                        .clicked()
                    {
                        actions.push(Action::ApplyPendingEdit(edit.edit_id.clone()));
                    }
                    if ui
                        .button("Отбросить")
                        .on_hover_text("Удалить эту правку без применения")
                        .clicked()
                    {
                        actions.push(Action::DiscardPendingEdit(edit.edit_id.clone()));
                    }
                });
                hint(
                    ui,
                    &format!("Размер документа: {} байт", edit.layout_toml.len()),
                );
            }
        },
    );
    actions
}

/// Who is «этот компьютер» and who is «другой», when the window cannot tell by itself.
fn pickers(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let this = vm.self_node().map(str::to_owned);
    if this.is_none() {
        ui.horizontal_wrapped(|ui| {
            ui.label("Этот компьютер:");
            for node in &vm.editor.nodes {
                let chosen = Some(node.node_id.as_str()) == vm.self_node();
                if ui.selectable_label(chosen, node.label()).clicked() {
                    actions.push(Action::ChooseSelf(Some(node.node_id.clone())));
                }
            }
        });
        hint(
            ui,
            "Служба не сообщила, какой из компьютеров этот; выберите его сами",
        );
        return actions;
    }
    if vm.editor.nodes.len() > 2 {
        ui.horizontal_wrapped(|ui| {
            ui.label("Соединить с:");
            for node in &vm.editor.nodes {
                if Some(node.node_id.as_str()) == this.as_deref() {
                    continue;
                }
                let chosen = Some(node.node_id.as_str()) == vm.other_node();
                if ui.selectable_label(chosen, node.label()).clicked() {
                    actions.push(Action::ChooseOther(Some(node.node_id.clone())));
                }
            }
        });
    }
    actions
}

/// The rectangle field: every member's displays, with the other computer's draggable.
fn canvas(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let width = ui.available_width().max(120.0);
    let (response, painter) =
        ui.allocate_painter(egui::vec2(width, CANVAS_HEIGHT), egui::Sense::hover());
    let field = response.rect;
    let Some((min_x, min_y, max_x, max_y)) = every_bounds(vm) else {
        painter.text(
            field.center(),
            egui::Align2::CENTER_CENTER,
            "Данных об экранах нет",
            egui::FontId::proportional(15.0),
            ui.visuals().weak_text_color(),
        );
        return actions;
    };
    let span_x = (max_x - min_x).max(1.0);
    let span_y = (max_y - min_y).max(1.0);
    let scale = ((f64::from(field.width()) - 24.0) / span_x)
        .min((f64::from(field.height()) - 24.0) / span_y)
        .clamp(0.005, 8.0);
    let place = |x: f64, y: f64| -> egui::Pos2 {
        egui::pos2(
            field.min.x + 12.0 + ((x - min_x) * scale) as f32,
            field.min.y + 12.0 + ((y - min_y) * scale) as f32,
        )
    };
    let this = vm.self_node().map(str::to_owned);
    let linked = vm.linked_span();
    let link_color = ui.visuals().selection.bg_fill;
    for node in &vm.editor.nodes {
        let mine = this.as_deref() == Some(node.node_id.as_str());
        for display in &node.displays {
            let size = egui::vec2(
                ((display.w * scale) as f32).max(MIN_RECT),
                ((display.h * scale) as f32).max(MIN_RECT),
            );
            let rect = egui::Rect::from_min_size(place(display.x, display.y), size);
            let (fill, stroke) = if mine {
                (
                    ui.visuals().widgets.active.bg_fill.linear_multiply(0.35),
                    ui.visuals().widgets.active.fg_stroke,
                )
            } else {
                (
                    ui.visuals().widgets.inactive.bg_fill,
                    ui.visuals().widgets.inactive.fg_stroke,
                )
            };
            painter.rect_filled(rect, egui::CornerRadius::ZERO, fill);
            painter.rect_stroke(
                rect,
                egui::CornerRadius::ZERO,
                egui::Stroke::new(2.0, stroke.color),
                egui::StrokeKind::Middle,
            );
            if !display.native_known {
                // A rough size is drawn roughly: a dashed border, and the label says so.
                painter.rect_stroke(
                    rect.shrink(2.0),
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
                    egui::StrokeKind::Inside,
                );
            }
            // Which side of the screen is which computer: the owner reads «этот компьютер»
            // once, on the machine they are sitting at.
            let label = if mine {
                format!("Этот компьютер — {}", node.label())
            } else {
                node.label()
            };
            painter.text(
                rect.min + egui::vec2(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(13.0),
                ui.visuals().text_color(),
            );
            // The stretch of edge the link crosses, drawn on the edge it crosses: this is the
            // «Применить» document's own `start..end`, so the picture is the document.
            if let Some((my_display, my_edge, their_display, their_edge, end)) = &linked {
                let wanted =
                    (mine && my_display == &display.id) || (!mine && their_display == &display.id);
                if wanted {
                    let edge = if mine { *my_edge } else { *their_edge };
                    // On the edge itself, not through the middle of the rectangle: the line
                    // *is* the stretch of screen border input crosses at.
                    let (a, b) = match edge {
                        config::Edge::Left | config::Edge::Right => {
                            let x = if edge == config::Edge::Left {
                                rect.min.x
                            } else {
                                rect.max.x
                            };
                            (
                                egui::pos2(x, place(display.x, display.y).y),
                                egui::pos2(x, place(display.x, display.y + *end).y),
                            )
                        }
                        config::Edge::Top | config::Edge::Bottom => {
                            let y = if edge == config::Edge::Top {
                                rect.min.y
                            } else {
                                rect.max.y
                            };
                            (
                                egui::pos2(place(display.x, display.y).x, y),
                                egui::pos2(place(display.x + *end, display.y).x, y),
                            )
                        }
                    };
                    painter.line_segment([a, b], egui::Stroke::new(4.0, link_color));
                }
            }
            let size_label = if display.native_known {
                format!("{}×{}", display.w.round(), display.h.round())
            } else {
                "размер известен приблизительно".to_owned()
            };
            painter.text(
                rect.min + egui::vec2(6.0, rect.height() - 6.0),
                egui::Align2::LEFT_BOTTOM,
                size_label,
                egui::FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
            if mine {
                continue;
            }
            // The other computer's rectangle is the one the user may drag; the delta is
            // reported in the node's own logical units, which is what the document carries.
            let id = egui::Id::new(("lws-display", &node.node_id, &display.id));
            let dragged = ui.interact(rect, id, egui::Sense::drag());
            let delta = dragged.drag_delta();
            if delta != egui::Vec2::ZERO {
                actions.push(Action::MoveDisplay {
                    node_id: node.node_id.clone(),
                    display_id: display.id.clone(),
                    dx: f64::from(delta.x) / scale,
                    dy: f64::from(delta.y) / scale,
                });
            }
            if dragged.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
    }
    painter.rect_stroke(
        field.shrink(1.0),
        egui::CornerRadius::ZERO,
        egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
        egui::StrokeKind::Inside,
    );
    actions
}

/// The side radio and the button that sends the document.
fn side_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Как стоят компьютеры", |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("Другой компьютер:");
            for side in Side::ALL {
                if ui.radio(vm.editor.side == side, side.label()).clicked() {
                    actions.push(Action::SetSide(side));
                }
            }
        });
        let (this, other) = (vm.self_node(), vm.other_node());
        match (this, other) {
            (Some(_), Some(other)) => {
                let name = vm
                    .editor
                    .nodes
                    .iter()
                    .find(|node| node.node_id == other)
                    .map_or_else(|| "другому компьютеру".to_owned(), |node| node.label());
                ui.label(format!(
                    "При нажатии «Применить» раскладка свяжет края этого компьютера и {name}",
                ));
            }
            _ => {
                ui.colored_label(
                    warn_color(),
                    "Выберите, какой компьютер этот и с каким его соединить",
                );
            }
        }
        ui.horizontal(|ui| {
            let ready = vm.self_node().is_some()
                && vm.other_node().is_some()
                && vm.editor.editable
                && vm.connection.is_connected();
            if ui
                .add_enabled(ready, egui::Button::new("Применить"))
                .on_hover_text("Отправить раскладку координатору")
                .clicked()
            {
                actions.push(Action::UpdateTopology {
                    layout_toml: vm.layout_toml().into_bytes(),
                    expected_revision: vm.editor.revision,
                    expected_author: crate::view_model::node_id_bytes(&vm.editor.author),
                });
            }
            if !vm.editor.editable {
                hint(ui, "Кнопка станет доступна, когда появится координатор");
            } else if !vm.connection.is_connected() {
                hint(ui, "Нужна связь со службой");
            }
        });
    });
    actions
}

/// What exactly would be sent, for the human who wants to see it.
fn preview(vm: &ViewModel, ui: &mut egui::Ui) {
    egui::CollapsingHeader::new("Что будет отправлено")
        .default_open(false)
        .show(ui, |ui| {
            let document = vm.layout_toml();
            if document.is_empty() {
                ui.colored_label(warn_color(), "Документ собрать не удалось");
                return;
            }
            ui.monospace(document);
        });
}

/// The bounding box of every display of every member.
fn every_bounds(vm: &ViewModel) -> Option<(f64, f64, f64, f64)> {
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    for node in &vm.editor.nodes {
        for display in &node.displays {
            bounds = Some(match bounds {
                None => (
                    display.x,
                    display.y,
                    display.x + display.w.max(1.0),
                    display.y + display.h.max(1.0),
                ),
                Some((min_x, min_y, max_x, max_y)) => (
                    min_x.min(display.x),
                    min_y.min(display.y),
                    max_x.max(display.x + display.w.max(1.0)),
                    max_y.max(display.y + display.h.max(1.0)),
                ),
            });
        }
    }
    bounds
}

/// A node id as a sentence fragment.
fn author_text(author: &str) -> String {
    if author.is_empty() {
        "неизвестно".to_owned()
    } else {
        format!("компьютер {}", crate::view_model::short_id(author))
    }
}
