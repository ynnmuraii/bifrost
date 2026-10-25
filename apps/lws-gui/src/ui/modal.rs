//! The rail's windows, in the mockup's «terminal load» frame.
//!
//! The opening is the only animation here: the dialog is revealed top-down in eight steps over
//! 0.32 s (the mockup's `steps(8)` clip-path), and the next frame is requested only until that
//! is done.
//!
//! Modal bodies scroll independently of their fixed header and footer. Every final row
//! keeps 16 px below its contents. Keys edits service-owned bindings through IPC only,
//! while preserving the autonomous emergency-release warning.

use std::time::Duration;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::theme::{fam, font, Palette};
use super::widgets::{self, Icon, Kind};
use super::{act, UiState};
use crate::view_model::{Action, Modal, ViewModel};
use ui_text::{ago, ago_next_change_in, clipboard_size, fill, fill_named, text, Key, Lang};

/// The reveal time of a dialog, seconds.
const OPEN_FOR: f32 = 0.32;
/// The label column of a row.
const LABEL_W: f32 = 172.0;
/// The header's height.
const HEAD_H: f32 = 84.0;

/// Draws the open modal, if there is one, over everything.
///
/// Returns when the next frame is due for the relative times the open dialog shows (`None` when
/// none of them changes on a clock), which the caller folds into its own repaint schedule.
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    full: Rect,
    t: f32,
    actions: &mut Vec<Action>,
) -> Option<Duration> {
    let lang = vm.look.language;
    let Some(modal) = (if vm.appearance_open {
        Some(Modal::Appearance)
    } else {
        vm.modal
    }) else {
        st.modal_open = None;
        st.confirm_clear_history = false;
        st.clipboard_send_menu = None;
        ui.ctx().data_mut(|d| {
            d.remove::<crate::view_model::hotkeys::Editor>(egui::Id::new("hotkey-editor"));
        });
        return None;
    };
    let started = match st.modal_open {
        Some((open, at)) if open == modal => at,
        _ => {
            st.confirm_clear_history = false;
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
    let capture_id = egui::Id::new("hotkey-editor");
    let capturing = ui.ctx().data(|d| {
        d.get_temp::<crate::view_model::hotkeys::Editor>(capture_id)
            .is_some_and(|editor| editor.capturing.is_some())
    });
    if scrim.clicked() || (!capturing && ui.input(|i| i.key_pressed(egui::Key::Escape))) {
        ui.ctx()
            .data_mut(|d| d.remove::<crate::view_model::hotkeys::Editor>(capture_id));
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
            color: pal.shadow_dialog,
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

    header(dialog, pal, lang, modal, rect, progress, actions);
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
    let mut relabel: Option<Duration> = None;
    let used = egui::ScrollArea::vertical()
        .id_salt(("modal-scroll", modal as u8))
        .max_height(body_h)
        .auto_shrink([false, true])
        .show(&mut body, |ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            match modal {
                Modal::Pairs => pairs_body(ui, pal, vm, actions),
                Modal::Keys => keys_body(ui, pal, vm, actions),
                Modal::Clip => clip_body(ui, pal, vm, st, &mut relabel, actions),
                Modal::Diag => diag_body(ui, pal, vm, actions),
                Modal::Settings => settings_body(ui, pal, vm, actions),
                Modal::Appearance => super::appearance::body(ui, pal, vm, actions),
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
                    text(lang, Key::OpenThePairingWindow2),
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
                    text(lang, Key::OpenTheSettingsFile),
                    Kind::Solid,
                    true,
                    actions,
                    || Action::OpenConfigFile,
                );
            }
            Modal::Clip | Modal::Diag | Modal::Settings | Modal::Appearance => {}
        });
    }
    let measured = HEAD_H + used.content_size.y + foot_h;
    if (known.unwrap_or(0.0) - measured).abs() > 0.5 {
        ui.ctx().data_mut(|d| d.insert_temp(height_id, measured));
        ui.ctx().request_repaint();
    }
    relabel
}

/// Whether a dialog has a footer: only the two whose footer carries an action of their own.
fn has_footer(modal: Modal) -> bool {
    matches!(modal, Modal::Pairs | Modal::Keys)
}

/// The header: the number, the title with its «load» line, and the close button.
fn header(
    ui: &mut egui::Ui,
    pal: &Palette,
    lang: Lang,
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
    widgets::centre_galley(&painter, num, galley, pal.ink);
    // Title and the «load» line: `bifrost layout [########################] ok · sub`.
    let title = painter.layout_no_wrap(
        modal.title(lang).to_owned(),
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
    job.append(&bar, 0.0, mono(pal.accent_ink, fam::mono()));
    job.append(&pad, 0.0, mono(pal.ink2, fam::mono()));
    job.append("] ", 0.0, mono(pal.ink2, fam::mono()));
    if show_done {
        job.append("ok", 0.0, mono(pal.ink, fam::mono_medium()));
        job.append(
            &format!(" · {}", modal.subtitle(lang)),
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
pub(super) fn row(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    last: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    row_with_padding(ui, pal, label, last, 16.0, 16.0, add);
}

/// Compact settings rows keep release actions visible alongside clipboard and game mode.
/// The final row retains its normal bottom padding.
fn settings_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    last: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    row_with_padding(
        ui,
        pal,
        label,
        last,
        0.0,
        if last { 16.0 } else { 8.0 },
        add,
    );
}

fn row_with_padding(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    last: bool,
    top_padding: f32,
    bottom_padding: f32,
    add: impl FnOnce(&mut egui::Ui),
) {
    let left = ui.max_rect().left();
    let right = ui.max_rect().right();
    let top = ui.cursor().min.y + top_padding;
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
    let bottom = inner.response.rect.max.y.max(top + 20.0) + bottom_padding;
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
    let lang = vm.look.language;
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
            text(lang, Key::Online)
        } else {
            text(lang, Key::Offline)
        };
        let state = painter.layout_no_wrap(label.to_owned(), font(12.0, fam::mono()), pal.ink2);
        let button_w = painter
            .layout_no_wrap(
                text(lang, Key::Remove).to_owned(),
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
        if widgets::button(
            &mut child,
            pal,
            text(lang, Key::Remove),
            Kind::Plain,
            !asking,
        )
        .clicked()
        {
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
                            &fill(
                                lang,
                                Key::RemoveFromThePairYouWillHaveToPairAgain,
                                &[&peer.name],
                            ),
                            14.0,
                            fam::sans(),
                            pal.ink,
                        );
                        widgets::duo(ui, |ui| {
                            act(
                                ui,
                                pal,
                                text(lang, Key::YesRemove),
                                Kind::Hot,
                                true,
                                actions,
                                || Action::RevokePeer(crate::view_model::node_id_bytes(&id)),
                            );
                            act(
                                ui,
                                pal,
                                text(lang, Key::Cancel),
                                Kind::Plain,
                                true,
                                actions,
                                || Action::SelectRevoke(None),
                            );
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
        row(ui, pal, text(lang, Key::Empty), true, |ui| {
            widgets::text(
                ui,
                text(lang, Key::NobodyYetOpenThePairingWindow2),
                14.0,
                fam::sans(),
                pal.ink,
            );
        });
    }
}

fn keys_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    use crate::view_model::hotkeys::{self, Editor, ROWS};
    let lang = vm.look.language;
    let id = egui::Id::new("hotkey-editor");
    let mut editor = ui
        .ctx()
        .data(|d| d.get_temp::<Editor>(id))
        .unwrap_or_default();
    if !vm.connection.is_connected() {
        editor.capturing = None;
    }
    let bindings = vm
        .status
        .as_ref()
        .map(|s| s.hotkeys.as_slice())
        .unwrap_or_default();
    if let Some((action, chord)) = editor.capture(lang, &ui.input(|i| i.events.clone()), bindings) {
        actions.push(Action::SetHotkey { action, chord });
    }
    for (action, label, default) in ROWS {
        let binding = bindings.iter().find(|binding| binding.action == action);
        row(ui, pal, text(lang, label), action == 1, |ui| {
            widgets::chord(
                ui,
                pal,
                binding
                    .map(|binding| binding.chord.as_str())
                    .unwrap_or(default),
            );
            ui.horizontal_wrapped(|ui| {
                let connected = vm.connection.is_connected() && binding.is_some();
                ui.add_enabled_ui(connected, |ui| {
                    if widgets::button(ui, pal, text(lang, Key::Change), Kind::Solid, true)
                        .clicked()
                    {
                        editor.capturing = Some(action);
                        editor.error = None;
                    }
                    if widgets::button(ui, pal, text(lang, Key::Reset), Kind::Solid, true).clicked()
                    {
                        match hotkeys::validate(lang, action, default, bindings) {
                            Ok(()) => {
                                editor.capturing = None;
                                editor.error = None;
                                actions.push(Action::SetHotkey {
                                    action,
                                    chord: default.to_owned(),
                                });
                            }
                            Err(error) => editor.error = Some(error),
                        }
                    }
                });
            });
            if editor.capturing == Some(action) {
                widgets::wrapped(
                    ui,
                    text(lang, Key::PressAShortcutEscCancels),
                    12.0,
                    fam::sans(),
                    pal.ink,
                );
            }
            if let Some(binding) = binding {
                if !binding.available {
                    widgets::wrapped(ui, text(lang, Key::Unavailable), 12.0, fam::sans(), pal.ink);
                }
                if !binding.detail.is_empty() {
                    widgets::wrapped(ui, &binding.detail, 12.0, fam::sans(), pal.ink);
                }
            } else {
                widgets::wrapped(
                    ui,
                    text(lang, Key::WaitingForTheServiceStatus),
                    12.0,
                    fam::sans(),
                    pal.ink,
                );
            }
            if action == 1 {
                widgets::warn(ui, pal, |ui| {
                    let warning = if binding.is_some_and(|binding| !binding.available)
                        || vm.capability_missing("EMERGENCY_HOTKEY")
                    {
                        text(lang, Key::TheEmergencyKeyIsUnavailableOnThisSystemTheKeyboardCanOnlyBeReleasedWithTheMouse)
                    } else {
                        text(lang, Key::AlwaysGivesTheMouseAndKeyboardBackToThisComputerEvenIfTheServiceIsStuck)
                    };
                    widgets::wrapped(ui, warning, 12.0, fam::sans(), pal.ink);
                });
            }
        });
    }
    // The local pre-check names a sentence from the catalogue and then the exact reason, the way
    // an agent's own detail line is shown; the core's answer has no catalogue sentence of its own.
    if let Some(error) = editor.error.as_ref() {
        widgets::warn(ui, pal, |ui| {
            widgets::wrapped(
                ui,
                text(lang, Key::InvalidShortcut),
                12.0,
                fam::sans(),
                pal.ink,
            );
            widgets::wrapped(ui, error, 12.0, fam::sans(), pal.ink);
        });
    }
    if let Some(error) = vm.hotkeys_error.as_ref() {
        widgets::warn(ui, pal, |ui| {
            widgets::wrapped(ui, error, 12.0, fam::sans(), pal.ink);
        });
    }
    ui.ctx().data_mut(|d| d.insert_temp(id, editor));
}

fn clip_body(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    st: &mut UiState,
    // The soonest a relative label of a row changes; `draw` asks for its next frame then.
    relabel: &mut Option<Duration>,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    let pending = vm.settings.clipboard_pending;
    let on = pending.unwrap_or_else(|| vm.status.as_ref().is_some_and(|s| s.clipboard_enabled));
    // A backend that cannot deliver the clipboard is told so by the core: the toggle is then
    // not offered at all, because a switch that does nothing is worse than no switch.
    let missing = vm.capability_missing("TEXT_CLIPBOARD");
    row(ui, pal, text(lang, Key::SharedClipboard), false, |ui| {
        let enabled = vm.connection.is_connected() && pending.is_none() && !missing;
        ui.add_enabled_ui(enabled, |ui| {
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[text(lang, Key::On), text(lang, Key::Off)],
                usize::from(!on),
            ) {
                actions.push(Action::SetClipboard(i == 0));
            }
        });
        let line = if pending.is_some() {
            text(lang, Key::Applying)
        } else if missing {
            text(lang, Key::ThisSystemCannotCarryTheClipboard)
        } else if on {
            text(lang, Key::TextCanBeCopiedOnOneComputerAndPastedOnTheOther)
        } else {
            text(lang, Key::TheSharedClipboardIsOff)
        };
        widgets::wrapped(ui, line, 13.0, fam::sans(), pal.ink);
        if missing {
            widgets::hint(
                ui,
                pal,
                text(
                    lang,
                    Key::TheServiceReportedThatTheSharedClipboardIsUnavailable,
                ),
            );
        }
    });
    row(ui, pal, text(lang, Key::WhatIsCarried), false, |ui| {
        widgets::wrapped(
            ui,
            text(lang, Key::TextOnlyNoFilesNoImages),
            14.0,
            fam::sans(),
            pal.ink,
        );
        widgets::hint(
            ui,
            pal,
            text(lang, Key::TheClipboardNeverReachesTheDiagnosticsReports),
        );
    });
    row(ui, pal, text(lang, Key::History), true, |ui| {
        let connected = vm.connection.is_connected();
        // The list is a stack of two-line entries: keep them close to each other.
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 2.0);
        if vm.clipboard_history.is_empty() {
            st.confirm_clear_history = false;
        }
        if st.confirm_clear_history {
            widgets::wrapped(
                ui,
                text(lang, Key::ConfirmClearHistory),
                13.0,
                fam::sans(),
                pal.ink,
            );
            ui.horizontal(|ui| {
                if widgets::small_button(ui, pal, text(lang, Key::Cancel), true).clicked() {
                    st.confirm_clear_history = false;
                }
                if widgets::small_button(ui, pal, text(lang, Key::ClearHistory), connected)
                    .clicked()
                {
                    actions.push(Action::ClearClipboardHistory);
                    st.confirm_clear_history = false;
                }
            });
        } else if widgets::small_button(
            ui,
            pal,
            text(lang, Key::ClearHistory),
            connected && !vm.clipboard_history.is_empty(),
        )
        .clicked()
        {
            st.confirm_clear_history = true;
        }
        widgets::hint(
            ui,
            pal,
            text(lang, Key::TheLast30TextsInMemoryOnlySecretsAreNotKept),
        );
        if vm.clipboard_history.is_empty() {
            widgets::wrapped(
                ui,
                text(lang, Key::TheHistoryIsEmptyForNow),
                13.0,
                fam::sans(),
                pal.ink,
            );
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        for entry in &vm.clipboard_history {
            ui.push_id(entry.id, |ui| {
                // Two lines per entry: the metadata and the preview, close together.
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 1.0);
                ui.separator();
                let direction =
                    match protocol::ipc::ClipboardHistoryDirection::try_from(entry.direction) {
                        Ok(protocol::ipc::ClipboardHistoryDirection::Sent) => text(lang, Key::Sent),
                        Ok(protocol::ipc::ClipboardHistoryDirection::Copied) => {
                            text(lang, Key::Copied)
                        }
                        Ok(protocol::ipc::ClipboardHistoryDirection::Received) => {
                            text(lang, Key::Received)
                        }
                        _ => text(lang, Key::Transfer),
                    };
                let age_seconds = (now_ms.saturating_sub(u128::from(entry.time_unix_ms)) / 1000)
                    .try_into()
                    .unwrap_or(u64::MAX);
                // The next frame this row needs is the one that changes its «10 минут назад»;
                // one a minute, an hour or a day, and never a frame between them.
                let wait = Duration::from_secs(ago_next_change_in(age_seconds));
                *relabel = Some(match *relabel {
                    Some(w) => w.min(wait),
                    None => wait,
                });
                let metadata = fill_named(
                    lang,
                    Key::HistoryEntryMetadata,
                    &[
                        ("direction", &direction),
                        ("peer_name", &entry.peer_name),
                        ("age", &ago(lang, age_seconds)),
                        ("size", &clipboard_size(lang, entry.size_bytes)),
                    ],
                );
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(metadata)
                                .font(font(11.0, fam::sans()))
                                .color(pal.ink2),
                        )
                        .truncate(),
                    );
                    // «Копировать» keeps to the right edge, the metadata stays left.
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::small_button(ui, pal, text(lang, Key::Copy), connected)
                            .clicked()
                        {
                            st.clipboard_send_menu = None;
                            actions.push(Action::CopyClipboardHistory(entry.id));
                        }
                        let has_targets = vm.clipboard_targets().next().is_some();
                        if widgets::small_button(
                            ui,
                            pal,
                            text(lang, Key::SendClipboardTo),
                            connected && has_targets,
                        )
                        .clicked()
                        {
                            st.clipboard_send_menu =
                                (st.clipboard_send_menu != Some(entry.id)).then_some(entry.id);
                        }
                    });
                });
                if st.clipboard_send_menu == Some(entry.id) {
                    if connected && vm.clipboard_targets().next().is_some() {
                        ui.horizontal_wrapped(|ui| {
                            widgets::hint(ui, pal, text(lang, Key::SendClipboardTo));
                            for member in vm.clipboard_targets() {
                                let fallback;
                                let label = if member.display_name.is_empty() {
                                    fallback = crate::view_model::short_id(&member.node_id);
                                    fallback.as_str()
                                } else {
                                    member.display_name.as_str()
                                };
                                if widgets::small_button(ui, pal, label, true).clicked() {
                                    actions.push(Action::SendClipboardHistory {
                                        id: entry.id,
                                        peer_node_id: member.node_id.clone(),
                                    });
                                    st.clipboard_send_menu = None;
                                }
                            }
                            if widgets::small_button(ui, pal, text(lang, Key::Cancel), true)
                                .clicked()
                            {
                                st.clipboard_send_menu = None;
                            }
                        });
                    } else {
                        st.clipboard_send_menu = None;
                    }
                }
                // Flatten only the displayed preview; Copy still uses the stored entry id.
                let preview: String = entry
                    .preview
                    .chars()
                    .map(|c| {
                        if matches!(c, '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
                            ' '
                        } else {
                            c
                        }
                    })
                    .collect();
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(preview)
                            .font(font(13.0, fam::sans()))
                            .color(pal.ink),
                    )
                    .truncate(),
                );
            });
        }
    });
}

fn diag_body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let lang = vm.look.language;
    row(ui, pal, text(lang, Key::Report), false, |ui| {
        act(
            ui,
            pal,
            text(lang, Key::SaveTheDiagnostics2),
            Kind::Solid,
            vm.connection.is_connected(),
            actions,
            || Action::ExportDiagnostics(String::new()),
        );
        let line = match &vm.settings.export {
            Some(outcome) => outcome.line(lang),
            None => text(lang, Key::NoReportHasBeenSavedYet).to_owned(),
        };
        widgets::wrapped(ui, &line, 13.0, fam::sans(), pal.ink);
        widgets::hint(
            ui,
            pal,
            text(
                lang,
                Key::TheServiceWritesTheReportWithoutKeysAndWithoutTheClipboard,
            ),
        );
    });
    row(ui, pal, text(lang, Key::Directory), false, |ui| {
        let dir = vm.config.state_dir.display().to_string();
        let line = if dir.is_empty() {
            text(lang, Key::TheServiceSStateDirectoryIsUnknown).to_owned()
        } else {
            dir
        };
        let galley = ui.painter().layout(
            line,
            font(12.0, fam::mono()),
            pal.ink2,
            ui.available_width(),
        );
        widgets::show_galley(ui, galley);
    });
    if let Some(reason) = vm.agent_degraded_reason() {
        row(ui, pal, text(lang, Key::InputService), false, |ui| {
            widgets::wrapped(ui, reason, 14.0, fam::sans(), pal.ink);
            act(
                ui,
                pal,
                text(lang, Key::StartAgain),
                Kind::Plain,
                vm.connection.is_connected(),
                actions,
                || Action::RetryAgent,
            );
        });
    }
    let lines = vm.limits();
    let delay = vm.delay_line();
    row(ui, pal, text(lang, Key::State2), true, |ui| {
        widgets::text(
            ui,
            &fill(lang, Key::Link, &[&vm.link_line()]),
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
    let lang = vm.look.language;
    settings_row(ui, pal, text(lang, Key::GameMode), false, |ui| {
        let Some(status) = &vm.status else {
            widgets::hint(ui, pal, text(lang, Key::Service));
            return;
        };
        let mut mode = status.game_guard_mode.clone();
        let selected = match mode.as_str() {
            "always" => 1,
            "off" => 2,
            _ => 0,
        };
        widgets::duo(ui, |ui| {
            if let Some(index) = widgets::seg(
                ui,
                pal,
                &[
                    text(lang, Key::GameGuardAuto),
                    text(lang, Key::GameGuardAlways),
                    text(lang, Key::GameGuardOff),
                ],
                selected,
            ) {
                mode = ["auto", "always", "off"][index].to_owned();
                actions.push(Action::SetGameGuard(protocol::ipc::SetGameGuard {
                    mode: mode.clone(),
                    block_apps: status.game_guard_block_apps.clone(),
                    allow_apps: status.game_guard_allow_apps.clone(),
                }));
            }
            if widgets::button(
                ui,
                pal,
                text(lang, Key::GameGuardApps),
                Kind::Link,
                vm.connection.is_connected(),
            )
            .on_hover_text(text(lang, Key::GameGuardHelp))
            .clicked()
            {
                actions.push(Action::ToggleGameGuardApps);
            }
        });
        if status.backend == "platform-wayland" {
            widgets::hint(ui, pal, text(lang, Key::GameGuardWayland));
        }
        if !vm.settings.game_guard_apps_open {
            return;
        }
        let id = ui.id().with("game_guard_lists");
        let mut lists = ui
            .data_mut(|data| data.get_temp::<(String, String)>(id))
            .unwrap_or_else(|| {
                (
                    status.game_guard_block_apps.join(", "),
                    status.game_guard_allow_apps.join(", "),
                )
            });
        ui.label(text(lang, Key::GameGuardBlockApps));
        ui.add(
            egui::TextEdit::singleline(&mut lists.0)
                .char_limit(8256)
                .desired_width(f32::INFINITY),
        );
        ui.label(text(lang, Key::GameGuardAllowApps));
        ui.add(
            egui::TextEdit::singleline(&mut lists.1)
                .char_limit(8256)
                .desired_width(f32::INFINITY),
        );
        let app = status.game_guard_last_app.as_str();
        if widgets::button(
            ui,
            pal,
            text(lang, Key::GameGuardAddCurrent),
            Kind::Plain,
            !app.is_empty(),
        )
        .on_hover_text(app)
        .clicked()
        {
            if !lists.0.trim().is_empty() {
                lists.0.push_str(", ");
            }
            lists.0.push_str(app);
        }
        let parse = |value: &str| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let wanted = protocol::ipc::SetGameGuard {
            mode,
            block_apps: parse(&lists.0),
            allow_apps: parse(&lists.1),
        };
        let valid = config::game_guard::GameGuardConfig {
            mode: config::game_guard::Mode::Auto,
            block_apps: wanted.block_apps.clone(),
            allow_apps: wanted.allow_apps.clone(),
        }
        .validate()
        .is_ok();
        if widgets::button(ui, pal, text(lang, Key::GameGuardSave), Kind::Solid, valid).clicked() {
            actions.push(Action::SetGameGuard(wanted));
        }
        ui.data_mut(|data| data.insert_temp(id, lists));
        widgets::hint(ui, pal, text(lang, Key::GameGuardHelp));
    });
    // The window's own language. The tray reads the same `gui.toml`, so it follows the choice;
    // the change is instant, because every string is drawn from the catalogue each frame.
    settings_row(ui, pal, text(lang, Key::Language), false, |ui| {
        let mut look = vm.look;
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[Lang::Ru.code().to_uppercase().as_str(), "EN"],
            usize::from(look.language == Lang::En),
        ) {
            look.language = if i == 0 { Lang::Ru } else { Lang::En };
            actions.push(Action::SetLook(look));
        }
    });
    settings_row(ui, pal, text(lang, Key::Autostart), false, |ui| {
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[text(lang, Key::On), text(lang, Key::Off)],
            usize::from(!vm.settings.autostart),
        ) {
            actions.push(Action::SetAutostart(i == 0));
        }
        if let Some(error) = &vm.settings.autostart_error {
            widgets::wrapped(ui, error, 13.0, fam::sans(), pal.ink);
        }
    });
    settings_row(ui, pal, text(lang, Key::ClipboardRouting), false, |ui| {
        let status = vm.status.as_ref();
        let broadcast = status.is_some_and(|status| status.clipboard_policy == "broadcast");
        let pending = vm
            .inflight
            .iter()
            .any(|request| request.request == crate::view_model::Request::SetClipboardPolicy);
        let toggle = ui.add_enabled_ui(vm.connection.is_connected() && !pending, |ui| {
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[
                    text(lang, Key::ClipboardFollow),
                    text(lang, Key::ClipboardBroadcast),
                ],
                usize::from(broadcast),
            ) {
                let settle_ms = status.map_or(700, |status| {
                    if status.clipboard_settle_ms == 0 {
                        700
                    } else {
                        status.clipboard_settle_ms.clamp(200, 3000)
                    }
                });
                actions.push(Action::SetClipboardPolicy {
                    policy: if i == 0 { "follow" } else { "broadcast" }.to_owned(),
                    settle_ms,
                });
            }
        });
        toggle.response.on_hover_text(text(
            lang,
            if broadcast {
                Key::ClipboardBroadcastHint
            } else {
                Key::ClipboardFollowHint
            },
        ));
        if pending {
            widgets::hint(ui, pal, text(lang, Key::Applying));
        }
    });
    settings_row(ui, pal, text(lang, Key::LanDiscovery), false, |ui| {
        let pending = vm.settings.discovery_pending;
        let toggle = ui.add_enabled_ui(vm.connection.is_connected() && pending.is_none(), |ui| {
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[text(lang, Key::On), text(lang, Key::Off)],
                usize::from(!vm.discovery_enabled()),
            ) {
                actions.push(Action::SetDiscovery(i == 0));
            }
        });
        // Keep this a single-control row so release actions still fit at desktop heights.
        toggle
            .response
            .on_hover_text(text(lang, Key::DiscoveryExplanation));
        if pending.is_some() {
            widgets::hint(ui, pal, text(lang, Key::Applying));
        }
    });
    let current = vm.config.listen_address.clone();
    settings_row(ui, pal, text(lang, Key::LANAddress), false, |ui| {
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
                text(lang, Key::Apply),
                Kind::Plain,
                !selected.is_empty(),
                actions,
                || Action::ApplyLanAddress,
            );
        });
        if let Some(applied) = &vm.settings.lan.applied {
            widgets::wrapped(ui, &applied.line(lang), 13.0, fam::sans(), pal.ink);
        }
        widgets::hint(
            ui,
            pal,
            text(lang, Key::AfterApplyTheServiceRestartsOnTheNewAddress),
        );
    });
    settings_row(ui, pal, text(lang, Key::Service), false, |ui| {
        let busy = vm.settings.service.restart.is_some() || vm.settings.service.stopping.is_some();
        widgets::duo(ui, |ui| {
            act(
                ui,
                pal,
                text(lang, Key::Restart),
                Kind::Plain,
                !busy,
                actions,
                || Action::RestartCore,
            );
            act(
                ui,
                pal,
                text(lang, Key::Stop),
                Kind::Plain,
                vm.connection.is_connected() && !busy,
                actions,
                || Action::StopCore,
            );
        });
        if let Some(line) = &vm.settings.service.line {
            widgets::wrapped(ui, &line.line(lang), 13.0, fam::sans(), pal.ink);
        }
        if vm.settings.first_run {
            widgets::hint(
                ui,
                pal,
                text(lang, Key::TheSettingsWereCreatedAutomaticallyConfigToml2),
            );
            act(
                ui,
                pal,
                text(lang, Key::Understood),
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
        settings_row(ui, pal, text(lang, Key::Portals), false, |ui| {
            act(
                ui,
                pal,
                text(lang, Key::ForgetThePortalGrants2),
                Kind::Plain,
                vm.connection.is_connected(),
                actions,
                || Action::ResetPortalGrants,
            );
            widgets::hint(ui, pal, text(lang, Key::TheServiceWillAskForTheGrantsAgain));
        });
    }
    settings_row(ui, pal, text(lang, Key::StartupUpdates), false, |ui| {
        // The other on/off segment uses the same labels; keep its animation and hit IDs separate.
        ui.push_id("startup-updates", |ui| {
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[text(lang, Key::On), text(lang, Key::Off)],
                usize::from(!vm.look.check_updates),
            ) {
                actions.push(Action::Update(crate::updater::Action::Startup(i == 0)));
            }
        });
    });
    settings_row(ui, pal, text(lang, Key::Version), true, |ui| {
        super::update::settings(ui, pal, vm, actions);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui_settings::Theme;

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
