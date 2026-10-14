//! The six windows of the rail, in the mockup's «terminal load» frame.
//!
//! The opening is the only animation here: the dialog is revealed top-down in eight steps over
//! 0.32 s (the mockup's `steps(8)` clip-path), and the next frame is requested only until that
//! is done.

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::theme::{fam, font, Palette};
use super::widgets::{self, Icon, Kind, Run};
use super::{act, UiState};
use crate::gui_settings::{FieldMode, Look, Pace, Theme};
use crate::view_model::{Action, Modal, Side, ViewModel};

/// The reveal time of a dialog, seconds.
const OPEN_FOR: f32 = 0.32;
/// The label column of a row.
const LABEL_W: f32 = 172.0;
/// The header's height.
const HEAD_H: f32 = 84.0;

/// Draws the open modal, if there is one, over everything.
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    full: Rect,
    t: f32,
    actions: &mut Vec<Action>,
) {
    let Some(modal) = vm.modal else {
        st.modal_open = None;
        return;
    };
    let started = match st.modal_open {
        Some((open, at)) if open == modal => at,
        _ => {
            st.modal_open = Some((modal, t));
            if modal == Modal::Layout && vm.connection.is_connected() {
                actions.push(Action::ListPendingEdits);
            }
            t
        }
    };
    let progress = ((t - started) / OPEN_FOR).clamp(0.0, 1.0);
    if progress < 1.0 {
        ui.ctx().request_repaint();
    }
    let stepped = (progress * 8.0).floor() / 8.0;

    // The scrim: a click outside the dialog closes it, and so does Escape.
    let painter = ui.painter().clone();
    painter.rect_filled(full, 0.0, pal.scrim);
    let scrim = ui.interact(full, egui::Id::new("scrim"), Sense::click());
    if scrim.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        actions.push(Action::CloseModal);
    }

    let width = (full.width() - 48.0).min(680.0);
    let max_h = full.height() - 48.0;
    let height_id = egui::Id::new(("modal-height", modal));
    let known: Option<f32> = ui.ctx().data(|d| d.get_temp(height_id));
    let height = known.unwrap_or(420.0).min(max_h);
    let rect = Rect::from_center_size(full.center(), Vec2::new(width, height));
    let reveal = Rect::from_min_max(
        rect.min - Vec2::splat(40.0),
        Pos2::new(
            rect.max.x + 40.0,
            rect.min.y + height * stepped + if stepped >= 1.0 { 80.0 } else { 0.0 },
        ),
    );
    let clipped = painter.with_clip_rect(reveal.intersect(full));
    clipped.add(
        egui::epaint::Shadow {
            offset: [0, 30],
            blur: 80,
            spread: 0,
            color: if pal.theme == Theme::Dark {
                super::theme::rgba(0, 0, 0, 0.65)
            } else {
                super::theme::rgba(15, 23, 42, 0.16)
            },
        }
        .as_shape(rect, pal.rad()),
    );
    clipped.rect(
        rect,
        pal.rad(),
        pal.bg,
        Stroke::new(1.0, pal.line),
        StrokeKind::Inside,
    );

    let mut dialog = ui.new_child(
        UiBuilder::new()
            .id_salt(("modal", modal as u8))
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    let dialog = &mut dialog;
    dialog.set_clip_rect(reveal.intersect(full));
    dialog.spacing_mut().item_spacing = Vec2::ZERO;

    header(dialog, pal, modal, rect, progress, actions);
    let body_h = max_h - HEAD_H;
    let used = egui::ScrollArea::vertical()
        .id_salt(("modal-scroll", modal as u8))
        .max_height(body_h)
        .auto_shrink([false, true])
        .show(dialog, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            match modal {
                Modal::Layout => layout_body(ui, pal, vm, actions),
                Modal::Pairs => pairs_body(ui, pal, vm, actions),
                Modal::Keys => keys_body(ui, pal, vm, actions),
                Modal::Clip => clip_body(ui, pal, vm, actions),
                Modal::Diag => diag_body(ui, pal, vm, actions),
                Modal::Settings => settings_body(ui, pal, vm, actions),
            }
        });
    let measured = HEAD_H + used.content_size.y;
    if (known.unwrap_or(0.0) - measured).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(height_id, measured));
        ui.ctx().request_repaint();
    }
}

/// The header: the number, the title with its «load» line, and the close button.
fn header(
    ui: &mut egui::Ui,
    pal: &Palette,
    modal: Modal,
    rect: Rect,
    progress: f32,
    actions: &mut Vec<Action>,
) {
    let head = Rect::from_min_size(rect.min, Vec2::new(rect.width(), HEAD_H));
    let painter = ui.painter().clone();
    painter.line_segment(
        [
            Pos2::new(head.min.x, head.max.y - 0.5),
            Pos2::new(head.max.x, head.max.y - 0.5),
        ],
        pal.hairline(),
    );
    // The number box.
    let num = Rect::from_min_size(
        Pos2::new(head.min.x + 20.0, head.min.y + 16.0),
        Vec2::splat(52.0),
    );
    painter.rect(
        num,
        pal.rad_sm(),
        pal.bg2,
        pal.hairline(),
        StrokeKind::Inside,
    );
    let galley = painter.layout_no_wrap(modal.num().to_owned(), font(20.0, fam::pixel()), pal.ink);
    painter.galley(num.center() - galley.size() / 2.0, galley, pal.ink);
    // Title and the «load» line: `mdf layout [########################] ok · sub`.
    let title = painter.layout_no_wrap(
        modal.title().to_owned(),
        font(24.0, fam::sans_semibold()),
        pal.ink,
    );
    let tx = num.max.x + 16.0;
    let title_h = title.size().y;
    painter.galley(Pos2::new(tx, head.min.y + 16.0), title, pal.ink);
    let filled = ((progress * 24.0).floor() as usize).min(24);
    let bar = "#".repeat(filled);
    let pad = " ".repeat(24 - filled);
    let show_done = progress >= 1.0;
    let mut job = egui::text::LayoutJob::default();
    let mono = |color: Color32, family: egui::FontFamily| egui::text::TextFormat {
        font_id: font(11.0, family),
        color,
        ..Default::default()
    };
    job.append(
        &format!("{} [", modal.command()),
        0.0,
        mono(pal.ink2, fam::mono()),
    );
    job.append(&bar, 0.0, mono(pal.accent, fam::mono()));
    job.append(&pad, 0.0, mono(pal.ink2, fam::mono()));
    job.append("] ", 0.0, mono(pal.ink2, fam::mono()));
    if show_done {
        job.append("ok", 0.0, mono(pal.ink, fam::mono_medium()));
        job.append(
            &format!(" · {}", modal.subtitle()),
            0.0,
            mono(pal.ink2, fam::mono()),
        );
    }
    let load = painter.layout_job(job);
    painter.galley(
        Pos2::new(tx, head.min.y + 16.0 + title_h + 5.0),
        load,
        pal.ink2,
    );
    // The close button.
    let close = Rect::from_min_size(
        Pos2::new(head.max.x - 16.0 - 44.0, head.min.y + 20.0),
        Vec2::splat(44.0),
    );
    let response = ui
        .interact(close, egui::Id::new("modal-x"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let hover = response.hovered();
    painter.circle(
        close.center(),
        22.0,
        if hover { pal.accent } else { pal.bg2 },
        if hover {
            Stroke::new(1.0, pal.accent)
        } else {
            pal.hairline()
        },
    );
    widgets::paint_icon(
        &painter,
        Icon::Close,
        close.center(),
        16.0,
        if hover { pal.on_accent } else { pal.ink },
    );
    if response.clicked() {
        actions.push(Action::CloseModal);
    }
    ui.allocate_space(Vec2::new(rect.width(), HEAD_H));
}

/// One `.row`: a label column and a content column, with a hairline under it.
fn row(ui: &mut egui::Ui, pal: &Palette, label: &str, last: bool, add: impl FnOnce(&mut egui::Ui)) {
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let top = ui.cursor().min.y + 16.0;
    let content = Rect::from_min_max(
        Pos2::new(left + 20.0 + LABEL_W + 20.0, top),
        Pos2::new(right - 20.0, top + 100_000.0),
    );
    let painter = ui.painter().clone();
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &label.to_uppercase(),
        0.0,
        egui::text::TextFormat {
            font_id: font(11.0, fam::mono()),
            color: pal.ink2,
            extra_letter_spacing: 0.77,
            ..Default::default()
        },
    );
    let galley = painter.layout_job(job);
    painter.galley(Pos2::new(left + 20.0, top + 4.0), galley, pal.ink2);
    let inner = ui.scope_builder(
        UiBuilder::new()
            .max_rect(content)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 10.0);
            add(ui);
        },
    );
    let bottom = inner.response.rect.max.y.max(top + 20.0) + 16.0;
    let used = bottom - ui.cursor().min.y;
    ui.allocate_space(Vec2::new(
        right - left,
        (used - inner.response.rect.height()).max(0.0) + 0.0,
    ));
    if !last {
        painter.line_segment(
            [
                Pos2::new(left, bottom - 0.5),
                Pos2::new(right, bottom - 0.5),
            ],
            pal.hairline(),
        );
    }
}

/// `.dlg-f`: right-aligned buttons under a hairline.
fn footer(ui: &mut egui::Ui, pal: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let top = ui.cursor().min.y;
    ui.painter().line_segment(
        [Pos2::new(left, top + 0.5), Pos2::new(right, top + 0.5)],
        pal.hairline(),
    );
    ui.add_space(14.0);
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        ui.allocate_ui_with_layout(
            Vec2::new(right - left - 40.0, 44.0),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                add(ui);
            },
        );
    });
    ui.add_space(14.0);
}

fn layout_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let editor = &vm.editor;
    row(ui, pal, "Применено", false, |ui| {
        widgets::text(ui, &vm.revision_text(), 14.0, fam::mono(), pal.ink);
        widgets::hint(
            ui,
            pal,
            "Правки принимает координатор — компьютер, который владеет вводом",
        );
        if !editor.editable {
            widgets::hint(
                ui,
                pal,
                "Сейчас координатор недоступен: применить правку нельзя.",
            );
        }
    });
    if let Some(conflict) = &vm.conflict {
        row(ui, pal, "Конфликт", false, |ui| {
            widgets::wrapped(ui, conflict, 14.0, fam::sans(), pal.ink);
            act(
                ui,
                pal,
                "Обновить",
                Kind::Plain,
                true,
                actions,
                || Action::Refresh,
            );
        });
    }
    for edit in &vm.pending_edits {
        row(ui, pal, "Непринятая правка", false, |ui| {
            let author = if edit.author_node_id.is_empty() {
                "неизвестного компьютера".to_owned()
            } else {
                crate::view_model::short_id(&edit.author_node_id)
            };
            widgets::wrapped(
                ui,
                &format!(
                    "Правка от {author} на основе ревизии {}. Её сделали, пока связи не было.",
                    edit.base_revision
                ),
                14.0,
                fam::sans(),
                pal.ink,
            );
            widgets::duo(ui, |ui| {
                act(
                    ui,
                    pal,
                    "Применить правку",
                    Kind::Solid,
                    true,
                    actions,
                    || Action::ApplyPendingEdit(edit.edit_id.clone()),
                );
                act(
                    ui,
                    pal,
                    "Отбросить",
                    Kind::Plain,
                    true,
                    actions,
                    || Action::DiscardPendingEdit(edit.edit_id.clone()),
                );
            });
        });
    }
    let names: Vec<String> = editor.nodes.iter().map(|n| n.label()).collect();
    let this = vm.self_node().map(str::to_owned);
    let other = vm.other_node().map(str::to_owned);
    let name_of = |id: &Option<String>| {
        id.as_deref()
            .and_then(|id| editor.nodes.iter().find(|n| n.node_id == id))
            .map_or_else(|| "—".to_owned(), |n| n.label())
    };
    row(ui, pal, "Этот компьютер", false, |ui| {
        if let Some(i) = widgets::select(ui, pal, "lay-this", &name_of(&this), &names) {
            actions.push(Action::ChooseSelf(Some(editor.nodes[i].node_id.clone())));
        }
    });
    row(ui, pal, "Соединить с", false, |ui| {
        if let Some(i) = widgets::select(ui, pal, "lay-peer", &name_of(&other), &names) {
            actions.push(Action::ChooseOther(Some(editor.nodes[i].node_id.clone())));
        }
    });
    row(ui, pal, "Сторона", true, |ui| {
        let labels: Vec<&str> = Side::ALL.iter().map(|s| s.label()).collect();
        let on = Side::ALL
            .iter()
            .position(|s| *s == editor.side)
            .unwrap_or(1);
        if let Some(i) = widgets::seg(ui, pal, &labels, on) {
            actions.push(Action::SetSide(Side::ALL[i]));
        }
        widgets::hint(
            ui,
            pal,
            "Проще перетащить компьютер на карте — края свяжутся сами",
        );
    });
    footer(ui, pal, |ui| {
        let apply = vm.layout_action();
        let can = apply.is_some() && editor.editable && vm.connection.is_connected();
        if widgets::button(ui, pal, "Применить", Kind::Hot, can).clicked() {
            if let Some(action) = apply {
                actions.push(action);
                actions.push(Action::CloseModal);
            }
        }
        act(
            ui,
            pal,
            "Расположить автоматически",
            Kind::Plain,
            vm.connection.is_connected(),
            actions,
            || Action::LayoutAuto,
        );
    });
}

fn pairs_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let peers = vm.peers();
    for peer in &peers {
        let left = ui.max_rect().left();
        let right = ui.max_rect().right();
        let top = ui.cursor().min.y + 14.0;
        let icon = Rect::from_min_size(Pos2::new(left + 20.0, top), Vec2::splat(44.0));
        let painter = ui.painter().clone();
        if peer.online {
            painter.rect(
                icon,
                pal.rad_sm(),
                pal.bg2,
                Stroke::new(1.0, pal.line),
                StrokeKind::Inside,
            );
        } else {
            painter.add(egui::Shape::dashed_line(
                &[
                    icon.left_top(),
                    icon.right_top(),
                    icon.right_bottom(),
                    icon.left_bottom(),
                    icon.left_top(),
                ],
                Stroke::new(1.0, pal.line),
                3.0,
                2.0,
            ));
        }
        let ic = painter.layout_no_wrap(
            peer.initials.clone(),
            font(13.0, fam::pixel()),
            if peer.online { pal.ink } else { pal.ink2 },
        );
        painter.galley(icon.center() - ic.size() / 2.0, ic, pal.ink);
        let name =
            painter.layout_no_wrap(peer.name.clone(), font(16.0, fam::sans_semibold()), pal.ink);
        let nh = name.size().y;
        painter.galley(Pos2::new(icon.max.x + 16.0, top + 3.0), name, pal.ink);
        let meta = painter.layout_no_wrap(peer.meta.clone(), font(11.0, fam::mono()), pal.ink2);
        painter.galley(Pos2::new(icon.max.x + 16.0, top + 3.0 + nh), meta, pal.ink2);
        // State and the delete button, right-aligned.
        let asking = vm.settings.revoke.as_deref() == Some(peer.node_id.as_str());
        let label = if peer.online {
            "в сети"
        } else {
            "не в сети"
        };
        let state = painter.layout_no_wrap(label.to_owned(), font(12.0, fam::mono()), pal.ink2);
        let button_w = painter
            .layout_no_wrap(
                "Удалить".to_owned(),
                font(13.0, fam::sans_medium()),
                Color32::WHITE,
            )
            .size()
            .x
            + 36.0;
        let bx = right - 20.0 - button_w;
        let sx = bx - 8.0 - state.size().x;
        widgets::paint_dot(
            &painter,
            pal,
            Pos2::new(sx - 12.0, top + 22.0),
            if peer.online {
                widgets::Dot::Ok
            } else {
                widgets::Dot::Off
            },
        );
        painter.galley(
            Pos2::new(sx, top + 22.0 - state.size().y / 2.0),
            state,
            pal.ink2,
        );
        let button = Rect::from_min_size(Pos2::new(bx, top), Vec2::new(button_w, 44.0));
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("peer", &peer.node_id))
                .max_rect(button),
        );
        if widgets::button(&mut child, pal, "Удалить", Kind::Plain, !asking).clicked() {
            actions.push(Action::SelectRevoke(Some(peer.node_id.clone())));
        }
        ui.allocate_space(Vec2::new(right - left, 14.0 + 44.0 + 14.0));
        if asking {
            let id = peer.node_id.clone();
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.allocate_ui(Vec2::new(right - left - 40.0, 10.0), |ui| {
                    widgets::warn(ui, pal, |ui| {
                        widgets::wrapped(
                            ui,
                            &format!(
                                "Удалить «{}» из пары? Сопрягать придётся заново.",
                                peer.name
                            ),
                            14.0,
                            fam::sans(),
                            pal.ink,
                        );
                        widgets::duo(ui, |ui| {
                            act(
                                ui,
                                pal,
                                "Да, удалить",
                                Kind::Hot,
                                true,
                                actions,
                                || Action::RevokePeer(crate::view_model::node_id_bytes(&id)),
                            );
                            act(ui, pal, "Отмена", Kind::Plain, true, actions, || {
                                Action::SelectRevoke(None)
                            });
                        });
                    });
                });
            });
            ui.add_space(16.0);
        }
        painter.line_segment(
            [
                Pos2::new(left, ui.cursor().min.y - 0.5),
                Pos2::new(right, ui.cursor().min.y - 0.5),
            ],
            pal.hairline(),
        );
    }
    if peers.is_empty() {
        row(ui, pal, "Пусто", true, |ui| {
            widgets::text(
                ui,
                "Пока никого: откройте окно сопряжения.",
                14.0,
                fam::sans(),
                pal.ink,
            );
        });
    }
    footer(ui, pal, |ui| {
        act(
            ui,
            pal,
            "Открыть окно сопряжения",
            Kind::Hot,
            vm.connection.is_connected(),
            actions,
            || Action::StartPairing { window_s: 120 },
        );
    });
    // The dialog closes once the window is open: the dock shows the address.
    if vm.pairing.window.is_some()
        && actions
            .iter()
            .any(|a| matches!(a, Action::StartPairing { .. }))
    {
        actions.push(Action::CloseModal);
    }
}

fn keys_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let emergency = vm
        .status
        .as_ref()
        .map(|s| s.emergency_hotkey.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(vm.config.emergency_hotkey.as_str());
    let missing = vm.status.as_ref().is_some_and(|s| {
        s.capabilities_unavailable
            .iter()
            .any(|c| c == "EMERGENCY_HOTKEY")
    });
    row(ui, pal, "Аварийная", false, |ui| {
        widgets::chord(ui, pal, emergency);
        if missing {
            widgets::warn(ui, pal, |ui| {
                widgets::wrapped(
                    ui,
                    "На этой системе аварийная клавиша недоступна: прервать захват клавиатуры можно только мышью.",
                    12.0,
                    fam::sans(),
                    pal.ink,
                );
            });
        } else {
            widgets::warn(ui, pal, |ui| {
                widgets::wrapped(
                    ui,
                    "Всегда возвращает мышь и клавиатуру этому компьютеру — даже если служба зависла.",
                    12.0,
                    fam::sans(),
                    pal.ink,
                );
            });
        }
    });
    row(ui, pal, "Переключение", false, |ui| {
        widgets::chord(ui, pal, &vm.config.switch_hotkey);
        widgets::hint(ui, pal, "Отдать ввод следующему компьютеру по раскладке");
    });
    row(ui, pal, "Взять управление", false, |ui| {
        widgets::chord(ui, pal, &vm.config.claim_hotkey);
        widgets::hint(ui, pal, "Сделать этот компьютер координатором");
        let owner = vm.status.as_ref().is_some_and(|s| s.is_owner);
        act(
            ui,
            pal,
            if owner {
                "Этот компьютер уже координатор"
            } else {
                "Взять управление"
            },
            Kind::Plain,
            vm.connection.is_connected() && !owner,
            actions,
            || Action::Claim,
        );
    });
    row(ui, pal, "Источник", true, |ui| {
        let file = vm.config.config_file.display().to_string();
        let text = if file.is_empty() {
            "config.toml · секция [input]".to_owned()
        } else {
            format!("{file} · секции [input], [workspace]")
        };
        let galley = ui.painter().layout(
            text,
            font(12.0, fam::mono()),
            pal.ink2,
            ui.available_width(),
        );
        widgets::show_galley(ui, galley);
    });
}

fn clip_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let on = vm.status.as_ref().is_some_and(|s| s.clipboard_enabled);
    row(ui, pal, "Общий буфер", false, |ui| {
        let connected = vm.connection.is_connected();
        if let Some(i) = widgets::seg(ui, pal, &["Включён", "Выключен"], usize::from(!on))
        {
            if connected {
                actions.push(Action::SetClipboard(i == 0));
            }
        }
        let text = if let Some(note) = &vm.settings.clipboard_note {
            note.as_str()
        } else if on {
            "Текст можно копировать на одном компьютере и вставлять на другом"
        } else {
            "Общий буфер обмена выключен"
        };
        widgets::wrapped(ui, text, 13.0, fam::sans(), pal.ink);
    });
    row(ui, pal, "Что передаётся", true, |ui| {
        widgets::wrapped(
            ui,
            "Только текст. Файлы и изображения — нет.",
            14.0,
            fam::sans(),
            pal.ink,
        );
        widgets::hint(
            ui,
            pal,
            "Содержимое буфера не попадает в отчёты диагностики",
        );
    });
}

fn diag_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    row(ui, pal, "Отчёт", false, |ui| {
        act(
            ui,
            pal,
            "Сохранить диагностику",
            Kind::Solid,
            vm.connection.is_connected(),
            actions,
            || Action::ExportDiagnostics(String::new()),
        );
        let text = match &vm.settings.export {
            Some(outcome) => outcome.line(),
            None => "Отчёт ещё не сохранялся".to_owned(),
        };
        widgets::wrapped(ui, &text, 13.0, fam::sans(), pal.ink);
        widgets::hint(
            ui,
            pal,
            "Служба запишет отчёт без ключей и содержимого буфера обмена",
        );
    });
    row(ui, pal, "Каталог", false, |ui| {
        let dir = vm.config.state_dir.display().to_string();
        let text = if dir.is_empty() {
            "каталог состояния службы не определён".to_owned()
        } else {
            dir
        };
        let galley = ui.painter().layout(
            text,
            font(12.0, fam::mono()),
            pal.ink2,
            ui.available_width(),
        );
        widgets::show_galley(ui, galley);
    });
    if let Some(reason) = vm.agent_degraded_reason() {
        row(ui, pal, "Служба ввода", false, |ui| {
            widgets::wrapped(ui, reason, 14.0, fam::sans(), pal.ink);
            act(
                ui,
                pal,
                "Повторить запуск",
                Kind::Plain,
                vm.connection.is_connected(),
                actions,
                || Action::RetryAgent,
            );
        });
    }
    let lines = vm.limits();
    let delay = vm.delay_line();
    row(ui, pal, "Состояние", true, |ui| {
        widgets::text(
            ui,
            &format!("Связь: {}", vm.link_line()),
            13.0,
            fam::sans(),
            pal.ink,
        );
        if let Some(delay) = delay {
            widgets::text(ui, &delay, 13.0, fam::sans(), pal.ink);
        }
        for line in &lines {
            widgets::hint(ui, pal, line);
        }
    });
}

fn settings_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let current = vm.config.listen_address.clone();
    row(ui, pal, "Адрес в LAN", false, |ui| {
        let options = vm.settings.lan.options(&current);
        let selected = vm.settings.lan.selected(&current).unwrap_or_default();
        widgets::duo(ui, |ui| {
            let shown = if selected.is_empty() {
                "—"
            } else {
                selected.as_str()
            };
            if let Some(i) = widgets::select(ui, pal, "set-addr", shown, &options) {
                actions.push(Action::SelectLanAddress(options[i].clone()));
            }
            act(
                ui,
                pal,
                "Применить",
                Kind::Plain,
                !selected.is_empty(),
                actions,
                || Action::ApplyLanAddress,
            );
        });
        if let Some(applied) = &vm.settings.lan.applied {
            widgets::wrapped(ui, &applied.line(), 13.0, fam::sans(), pal.ink);
        }
        widgets::hint(
            ui,
            pal,
            "После «Применить» служба перезапустится с новым адресом",
        );
    });
    row(ui, pal, "Служба", false, |ui| {
        let (dot, text) = if vm.silent {
            (widgets::Dot::Off, "Не отвечает")
        } else if vm.connection.is_connected() {
            (widgets::Dot::Ok, "Работает")
        } else {
            (widgets::Dot::Off, "Не запущена")
        };
        widgets::pill_state(ui, pal, dot, text);
        let busy = vm.settings.service.restart.is_some() || vm.settings.service.stopping.is_some();
        widgets::duo(ui, |ui| {
            act(
                ui,
                pal,
                "Перезапустить",
                Kind::Plain,
                !busy,
                actions,
                || Action::RestartCore,
            );
            act(
                ui,
                pal,
                "Остановить",
                Kind::Plain,
                vm.connection.is_connected() && !busy,
                actions,
                || Action::StopCore,
            );
        });
        if let Some(line) = &vm.settings.service.line {
            widgets::wrapped(ui, &line.line(), 13.0, fam::sans(), pal.ink);
        }
        if vm.settings.first_run {
            widgets::hint(ui, pal, "Настройки созданы автоматически (config.toml).");
            act(
                ui,
                pal,
                "Понятно",
                Kind::Plain,
                true,
                actions,
                || Action::DismissFirstRun,
            );
        }
    });
    row(ui, pal, "Порталы", false, |ui| {
        let wayland = vm
            .status
            .as_ref()
            .is_some_and(|s| s.backend == "platform-wayland");
        act(
            ui,
            pal,
            "Забыть разрешения порталов",
            Kind::Plain,
            wayland && vm.connection.is_connected(),
            actions,
            || Action::ResetPortalGrants,
        );
        widgets::hint(
            ui,
            pal,
            if wayland {
                "Служба попросит разрешения заново"
            } else {
                "Только Linux / Wayland: на этой системе порталов нет"
            },
        );
    });
    let look = vm.look;
    row(ui, pal, "Тема", false, |ui| {
        let on = usize::from(look.theme == Theme::Dark);
        if let Some(i) = widgets::seg(ui, pal, &["Antigravity Light", "Overmind Dark"], on) {
            actions.push(Action::SetLook(Look {
                theme: if i == 0 { Theme::Light } else { Theme::Dark },
                ..look
            }));
        }
    });
    row(ui, pal, "Фон", true, |ui| {
        let field = match look.field {
            FieldMode::Repel => 0,
            FieldMode::Attract => 1,
            FieldMode::Off => 2,
        };
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &["Разлетаются", "Собираются", "Без движения"],
            field,
        ) {
            let field = [FieldMode::Repel, FieldMode::Attract, FieldMode::Off][i];
            actions.push(Action::SetLook(Look { field, ..look }));
        }
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &["Спокойно", "Живо"],
            usize::from(look.pace == Pace::Live),
        ) {
            actions.push(Action::SetLook(Look {
                pace: if i == 0 { Pace::Calm } else { Pace::Live },
                ..look
            }));
        }
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &["ASCII-облака", "Без облаков"],
            usize::from(!look.clouds),
        ) {
            actions.push(Action::SetLook(Look {
                clouds: i == 0,
                ..look
            }));
        }
        let _ = Run::plain("", pal.ink);
    });
}
