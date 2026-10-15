//! The five windows of the rail, in the mockup's «terminal load» frame.
//!
//! The opening is the only animation here: the dialog is revealed top-down in eight steps over
//! 0.32 s (the mockup's `steps(8)` clip-path), and the next frame is requested only until that
//! is done.
//!
//! Modal bodies scroll independently of their fixed header and footer. Every final row
//! keeps 16 px below its contents; Keys lists the implemented emergency shortcut and
//! retains the manual ownership control, without advertising unhandled config hotkeys.

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::theme::{fam, font, Palette};
use super::widgets::{self, Icon, Kind};
use super::{act, UiState};
use crate::gui_settings::Theme;
use crate::view_model::{Action, Modal, ViewModel};

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
    /// The height of a dialog's footer.
    const FOOT_H: f32 = 72.0;
    // Only the dialogs with an action of their own have a footer; one that held nothing but
    // «Закрыть» is gone, because the × in the header, Escape and a click outside the dialog
    // are all ways out of any of them.
    let foot_h = if has_footer(modal) { FOOT_H } else { 0.0 };
    let body_h = (height - HEAD_H - foot_h).max(0.0);
    let body_rect = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + HEAD_H),
        Pos2::new(rect.right(), rect.bottom() - foot_h),
    );
    let mut body = dialog.new_child(
        UiBuilder::new()
            .id_salt(("modal-body", modal as u8))
            .max_rect(body_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    body.set_clip_rect(body_rect.intersect(reveal).intersect(full));
    let used = egui::ScrollArea::vertical()
        .id_salt(("modal-scroll", modal as u8))
        .max_height(body_h)
        .auto_shrink([false, true])
        .show(&mut body, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            match modal {
                Modal::Pairs => pairs_body(ui, pal, vm, actions),
                Modal::Keys => keys_body(ui, pal, vm),
                Modal::Clip => clip_body(ui, pal, vm, actions),
                Modal::Diag => diag_body(ui, pal, vm, actions),
                Modal::Settings => settings_body(ui, pal, vm, actions),
            }
        });
    if foot_h > 0.0 {
        let footer_rect =
            Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - foot_h), rect.max);
        let mut foot = dialog.new_child(
            UiBuilder::new()
                .id_salt(("modal-footer", modal as u8))
                .max_rect(footer_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        foot.set_clip_rect(footer_rect.intersect(reveal).intersect(full));
        footer(&mut foot, pal, |ui| match modal {
            Modal::Pairs => {
                act(
                    ui,
                    pal,
                    "Открыть окно сопряжения",
                    Kind::Hot,
                    vm.connection.is_connected(),
                    actions,
                    || Action::StartPairing { window_s: 120 },
                );
            }
            Modal::Keys => {
                act(
                    ui,
                    pal,
                    "Открыть файл настроек",
                    Kind::Solid,
                    true,
                    actions,
                    || Action::OpenConfigFile,
                );
            }
            Modal::Clip | Modal::Diag | Modal::Settings => {}
        });
    }
    let measured = HEAD_H + used.content_size.y + foot_h;
    if (known.unwrap_or(0.0) - measured).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(height_id, measured));
        ui.ctx().request_repaint();
    }
}

/// Whether a dialog has a footer: only the two whose footer carries an action of their own.
fn has_footer(modal: Modal) -> bool {
    matches!(modal, Modal::Pairs | Modal::Keys)
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
    // The scope already advanced the cursor past its contents, not the row padding.
    ui.add_space((bottom - ui.cursor().min.y).max(0.0));
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

fn pairs_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let peers = vm.peers();
    for (index, peer) in peers.iter().enumerate() {
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
        ui.allocate_space(Vec2::new(right - left, 14.0 + 44.0 + 16.0));
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
        if index + 1 < peers.len() {
            painter.line_segment(
                [
                    Pos2::new(left, ui.cursor().min.y - 0.5),
                    Pos2::new(right, ui.cursor().min.y - 0.5),
                ],
                pal.hairline(),
            );
        }
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
}

fn keys_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel) {
    let emergency = vm
        .status
        .as_ref()
        .map(|s| s.emergency_hotkey.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(vm.config.emergency_hotkey.as_str());
    let missing = vm.capability_missing("EMERGENCY_HOTKEY");
    row(ui, pal, "Аварийная", true, |ui| {
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
}

fn clip_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let pending = vm.settings.clipboard_pending;
    let on = pending.unwrap_or_else(|| vm.status.as_ref().is_some_and(|s| s.clipboard_enabled));
    // A backend that cannot deliver the clipboard is told so by the core: the toggle is then
    // not offered at all, because a switch that does nothing is worse than no switch.
    let missing = vm.capability_missing("TEXT_CLIPBOARD");
    row(ui, pal, "Общий буфер", false, |ui| {
        let enabled = vm.connection.is_connected() && pending.is_none() && !missing;
        ui.add_enabled_ui(enabled, |ui| {
            if let Some(i) = widgets::seg(ui, pal, &["Включён", "Выключен"], usize::from(!on))
            {
                actions.push(Action::SetClipboard(i == 0));
            }
        });
        let text = if pending.is_some() {
            "Применяем…"
        } else if missing {
            "Эта система не может передавать содержимое буфера обмена"
        } else if on {
            "Текст можно копировать на одном компьютере и вставлять на другом"
        } else {
            "Общий буфер обмена выключен"
        };
        widgets::wrapped(ui, text, 13.0, fam::sans(), pal.ink);
        if missing {
            widgets::hint(ui, pal, "Служба сообщила: общий буфер обмена недоступен");
        }
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
    row(ui, pal, "Служба", !vm.uses_portals(), |ui| {
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
    // Portals are a Wayland desktop's, and only a Wayland desktop's: a window on Windows or
    // X11 has nothing to forget and does not show the row at all.
    if vm.uses_portals() {
        row(ui, pal, "Порталы", true, |ui| {
            act(
                ui,
                pal,
                "Забыть разрешения порталов",
                Kind::Plain,
                vm.connection.is_connected(),
                actions,
                || Action::ResetPortalGrants,
            );
            widgets::hint(ui, pal, "Служба попросит разрешения заново");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_bottom_padding_after_short_and_tall_content() {
        let ctx = egui::Context::default();
        super::super::install_fonts(&ctx);
        let pal = Palette::of(Theme::Dark);
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            for height in [20.0, 120.0] {
                let mut content_bottom = 0.0;
                row(ui, &pal, "Row", true, |ui| {
                    content_bottom = ui.allocate_space(Vec2::new(100.0, height)).1.bottom();
                });
                assert!((ui.cursor().min.y - content_bottom - 16.0).abs() < 0.5);
            }
        });
        output.drop_without_applying_deltas();
    }
}
