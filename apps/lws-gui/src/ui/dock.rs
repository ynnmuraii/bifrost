//! The bottom dock: four cells of pairing — this computer, the pairing window, dialling an
//! address, and the six-digit confirmation.

use std::time::Instant;

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};

use super::act;
use super::theme::{fam, font, Palette};
use super::widgets::{self, Kind, Run};
use crate::view_model::{Action, ViewModel};

/// The dock's usual height.
const BASE_H: f32 = 214.0;
/// What the open fingerprints add.
const DETAILS_H: f32 = 96.0;
/// How long the core keeps a pairing window open.
const WINDOW_S: u32 = 120;

/// The height of the dock for the current state.
pub fn height(vm: &ViewModel, narrow: bool) -> f32 {
    // A prompt names the other computer and wraps in a quarter of the window: it needs room.
    let prompt = if vm.pairing.prompt.is_some() {
        40.0
    } else {
        0.0
    };
    let extra = prompt
        + if vm.pairing.prompt.is_some() && vm.pairing.details {
            DETAILS_H
        } else {
            0.0
        };
    if narrow {
        2.0 * 188.0 + extra
    } else {
        BASE_H + extra
    }
}

/// Draws the dock into `rect`.
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    rect: Rect,
    narrow: bool,
    now: Instant,
    actions: &mut Vec<Action>,
) {
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, pal.card);
    painter.line_segment([rect.left_top(), rect.right_top()], pal.hairline());
    let (cols, rows) = if narrow { (2usize, 2usize) } else { (4, 1) };
    let cw = rect.width() / cols as f32;
    let ch = rect.height() / rows as f32;
    for n in 0..4usize {
        let (c, r) = (n % cols, n / cols);
        let cell = Rect::from_min_size(
            Pos2::new(rect.min.x + c as f32 * cw, rect.min.y + r as f32 * ch),
            Vec2::new(cw, ch),
        );
        if c > 0 {
            painter.line_segment([cell.left_top(), cell.left_bottom()], pal.hairline());
        }
        if r > 0 {
            painter.line_segment([cell.left_top(), cell.right_top()], pal.hairline());
        }
        let inner = Rect::from_min_max(
            Pos2::new(cell.min.x + 22.0, cell.min.y + 16.0),
            Pos2::new(cell.max.x - 22.0, cell.max.y - 18.0),
        );
        let mut child = ui.new_child(
            UiBuilder::new()
                .id_salt(("dock", n))
                .max_rect(inner)
                .layout(Layout::top_down(Align::Min)),
        );
        child.set_clip_rect(cell.intersect(ui.clip_rect()));
        child.spacing_mut().item_spacing.y = 10.0;
        match n {
            0 => this_computer(&mut child, pal, vm),
            1 => window(&mut child, pal, vm, now, actions),
            2 => dial(&mut child, pal, vm, actions),
            _ => code(&mut child, pal, vm, now, actions),
        }
    }
}

fn cell_head(ui: &mut egui::Ui, pal: &Palette, letter: &str, caption: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
        let painter = ui.painter();
        painter.rect(
            rect,
            egui::CornerRadius::same(6),
            pal.bg2,
            pal.hairline(),
            StrokeKind::Inside,
        );
        let galley = painter.layout_no_wrap(letter.to_owned(), font(10.0, fam::pixel()), pal.ink);
        painter.galley(rect.center() - galley.size() / 2.0, galley, pal.ink);
        widgets::cap(ui, pal, caption);
    });
    ui.add_space(-8.0);
}

/// A: this computer and the address the other one types.
fn this_computer(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel) {
    cell_head(ui, pal, "A", "Этот компьютер");
    widgets::text(ui, &vm.this_name(), 24.0, fam::sans_semibold(), pal.ink);
    let address = vm.config.listen_address.as_str();
    if address.is_empty() {
        widgets::text(ui, "адрес не выбран", 18.0, fam::mono(), pal.ink2);
        widgets::hint(ui, pal, "Выберите адрес в локальной сети в настройках.");
    } else {
        let port = format!(":{}", vm.config.port_input);
        let galley = widgets::mono_runs(
            ui,
            &[Run::plain(address, pal.ink), Run::plain(&port, pal.ink2)],
            18.0,
            ui.available_width(),
        );
        widgets::show_galley(ui, galley);
        widgets::hint(
            ui,
            pal,
            "Этот адрес вводят на другом компьютере. Сменить — в настройках.",
        );
    }
}

/// B: the pairing window and its countdown.
fn window(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    now: Instant,
    actions: &mut Vec<Action>,
) {
    cell_head(ui, pal, "B", "Окно сопряжения");
    match (&vm.pairing.window, vm.pairing.window_deadline) {
        (Some(window), Some(deadline)) => {
            widgets::text(ui, &window.address, 22.0, fam::mono_medium(), pal.ink);
            let left = deadline.saturating_duration_since(now).as_secs_f32().ceil() as u32;
            let filled = ((left.min(WINDOW_S) as f32 / WINDOW_S as f32) * 24.0).round() as usize;
            let (full, empty) = ("#".repeat(filled), ".".repeat(24 - filled.min(24)));
            let tail = format!("] {left} с");
            let galley = widgets::mono_runs(
                ui,
                &[
                    Run::plain("[", pal.ink),
                    Run::plain(&full, pal.accent),
                    Run::plain(&empty, pal.ink2),
                    Run::plain(&tail, pal.ink),
                ],
                12.0,
                f32::INFINITY,
            );
            widgets::show_galley(ui, galley);
            widgets::hint(
                ui,
                pal,
                "Скажите этот адрес владельцу другого компьютера. Окно закроется само.",
            );
        }
        _ => {
            widgets::wrapped(
                ui,
                "На одном компьютере откройте окно, на другом введите адрес.",
                14.0,
                fam::sans(),
                pal.ink,
            );
            let connected = vm.connection.is_connected();
            act(
                ui,
                pal,
                "Открыть окно сопряжения",
                Kind::Solid,
                connected,
                actions,
                || Action::StartPairing { window_s: WINDOW_S },
            );
            widgets::hint(ui, pal, "Окно закроется само через 120 секунд");
        }
    }
}

/// C: dial an address.
fn dial(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    cell_head(ui, pal, "C", "Подключиться по адресу");
    widgets::text(
        ui,
        "Адрес с экрана другого компьютера",
        12.0,
        fam::sans(),
        pal.ink2,
    );
    let mut address = vm.pairing.address.clone();
    let mut submit = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let button_w = 18.0 * 2.0
            + ui.painter()
                .layout_no_wrap(
                    "Подключиться".to_owned(),
                    font(13.0, fam::sans_medium()),
                    Color32::WHITE,
                )
                .size()
                .x;
        let input_w = (ui.available_width() - button_w - 8.0).max(80.0);
        let response = egui::Frame::NONE
            .fill(pal.bg)
            .stroke(Stroke::new(1.0, pal.line))
            .corner_radius(pal.rad_sm())
            .inner_margin(egui::Margin::symmetric(12, 0))
            .show(ui, |ui| {
                ui.set_width(input_w - 24.0);
                ui.set_height(42.0);
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut address)
                            .frame(egui::Frame::NONE)
                            .hint_text(egui::RichText::new("192.168.1.10:47901").color(pal.ink2))
                            .font(font(13.0, fam::mono()))
                            .text_color(pal.ink)
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner
            })
            .inner;
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        let can = vm.connection.is_connected() && !address.trim().is_empty() && !vm.pairing.dialing;
        let label = if vm.pairing.dialing {
            "Подключаемся…"
        } else {
            "Подключиться"
        };
        if widgets::button(ui, pal, label, Kind::Solid, can).clicked() {
            submit = true;
        }
        if submit && !can {
            submit = false;
        }
    });
    if address != vm.pairing.address {
        actions.push(Action::SetAddress(address.clone()));
    }
    if submit {
        actions.push(Action::PairWith(address.trim().to_owned()));
    }
    widgets::hint(
        ui,
        pal,
        "Адрес показывает окно сопряжения на другом компьютере",
    );
}

/// One box of the six-digit code.
fn code_box(ui: &mut egui::Ui, pal: &Palette, glyph: &str, wait: bool, lit: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(34.0, 46.0), Sense::hover());
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(8);
    if wait {
        let (fill, edge) = if lit {
            (pal.accent_soft, pal.accent)
        } else {
            (Color32::TRANSPARENT, pal.line)
        };
        painter.rect_filled(rect, radius, fill);
        painter.add(egui::Shape::dashed_line(
            &[
                rect.left_top(),
                rect.right_top(),
                rect.right_bottom(),
                rect.left_bottom(),
                rect.left_top(),
            ],
            Stroke::new(1.0, edge),
            3.0,
            2.0,
        ));
        let galley = painter.layout_no_wrap(glyph.to_owned(), font(16.0, fam::pixel()), pal.ink2);
        painter.galley(rect.center() - galley.size() / 2.0, galley, pal.ink2);
    } else {
        painter.rect(
            rect,
            radius,
            pal.bg,
            Stroke::new(1.0, pal.line),
            StrokeKind::Inside,
        );
        let galley = painter.layout_no_wrap(glyph.to_owned(), font(24.0, fam::pixel()), pal.ink);
        painter.galley(rect.center() - galley.size() / 2.0, galley, pal.ink);
    }
}

/// Six boxes in two groups of three.
fn code_row(ui: &mut egui::Ui, pal: &Palette, digits: [&str; 6], wait: bool, lit: Option<usize>) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        for (i, glyph) in digits.iter().enumerate() {
            if i == 3 {
                ui.add_space(8.0);
            }
            code_box(ui, pal, glyph, wait, lit == Some(i));
        }
    });
}

/// D: the code to compare, the wait for it, or the pair that was made.
fn code(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, now: Instant, actions: &mut Vec<Action>) {
    cell_head(ui, pal, "D", "Код сопряжения");
    if let Some(prompt) = &vm.pairing.prompt {
        let name = if prompt.peer_name.is_empty() {
            "Компьютер"
        } else {
            prompt.peer_name.as_str()
        };
        let galley = widgets::sans_runs(
            ui,
            &[
                Run::strong(name, pal.ink),
                Run::plain(" подключается — коды совпадают?", pal.ink),
            ],
            14.0,
            ui.available_width(),
        );
        widgets::show_galley(ui, galley);
        let digits: Vec<String> = prompt.sas_code.chars().map(String::from).collect();
        let at = |i: usize| digits.get(i).map_or("·", String::as_str);
        code_row(
            ui,
            pal,
            [at(0), at(1), at(2), at(3), at(4), at(5)],
            false,
            None,
        );
        widgets::duo(ui, |ui| {
            let attempt = vm.pairing.attempt_id();
            let can = attempt.is_some();
            if widgets::button(ui, pal, "Да, совпадают", Kind::Hot, can).clicked() {
                if let Some(attempt_id) = attempt.clone() {
                    actions.push(Action::Answer {
                        attempt_id,
                        approve: true,
                    });
                }
            }
            if widgets::button(ui, pal, "Нет", Kind::Plain, can).clicked() {
                if let Some(attempt_id) = attempt {
                    actions.push(Action::Answer {
                        attempt_id,
                        approve: false,
                    });
                }
            }
            let label = if vm.pairing.details {
                "Свернуть"
            } else {
                "Подробнее"
            };
            if widgets::button(ui, pal, label, Kind::Link, true).clicked() {
                actions.push(Action::ToggleDetails(!vm.pairing.details));
            }
        });
        if vm.pairing.details {
            let mono = |ui: &mut egui::Ui, text: String| {
                let galley = ui.painter().layout(
                    text,
                    font(11.0, fam::mono()),
                    pal.ink2,
                    ui.available_width(),
                );
                widgets::show_galley(ui, galley);
            };
            ui.spacing_mut().item_spacing.y = 4.0;
            mono(ui, format!("{name} · SHA-256 {}", prompt.peer_fingerprint));
            mono(ui, format!("ЭТОТ · SHA-256 {}", prompt.local_fingerprint));
            mono(
                ui,
                "Отпечатки совпадают, а коды нет — не соглашайтесь.".to_owned(),
            );
        }
    } else if let Some(name) = &vm.pairing.paired {
        widgets::text(ui, "Пара создана", 24.0, fam::sans_semibold(), pal.ink);
        widgets::wrapped(
            ui,
            &format!("{name} сопряжён. Доверие сохранено — повторять не нужно."),
            14.0,
            fam::sans(),
            pal.ink,
        );
        act(ui, pal, "Готово", Kind::Plain, true, actions, || {
            Action::DonePairing
        });
    } else {
        let window_open = vm.pairing.window.is_some();
        let text = if vm.pairing.dialing {
            "Подключаемся к другому компьютеру…"
        } else if window_open {
            "Ждём, пока другой компьютер подключится…"
        } else {
            "Код появится после подключения"
        };
        widgets::wrapped(ui, text, 14.0, fam::sans(), pal.ink);
        let waiting = window_open || vm.pairing.dialing;
        // The «waiting» highlight steps once a second, with the countdown's own repaint.
        let lit = waiting.then(|| {
            let secs = vm
                .pairing
                .window_deadline
                .map_or(0, |d| d.saturating_duration_since(now).as_secs());
            (secs % 6) as usize
        });
        code_row(ui, pal, ["·"; 6], true, lit);
        widgets::hint(ui, pal, "Шесть цифр появятся на обоих экранах одновременно");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_model::{CoreEndpoint, LocalConfig};

    #[test]
    fn the_dock_grows_only_for_the_fingerprints() {
        let mut vm = ViewModel::new(
            CoreEndpoint::Ready {
                description: String::new(),
            },
            LocalConfig::default(),
        );
        assert_eq!(height(&vm, false), BASE_H);
        assert_eq!(height(&vm, true), 376.0);
        vm.pairing.details = true;
        assert_eq!(height(&vm, false), BASE_H, "no prompt, nothing to show");
        vm.pairing.prompt = Some(protocol::ipc::PairingPrompt::default());
        assert_eq!(height(&vm, false), BASE_H + 40.0 + DETAILS_H);
    }
}
