//! «Сопряжение» — the screen that makes AC-16 («настройка пары без CLI ≤3 мин») possible.
//!
//! Two computers are paired by a six-digit code both owners can see (ADR-010) and the
//! addresses they tell each other; the window shows the address as the largest thing on the
//! screen, because that is the part a human has to read out loud to someone else.

use eframe::egui;

use super::{block, hint, warn_color};
use crate::view_model::{Action, ViewModel};

/// How many seconds «Открыть окно сопряжения» asks for (the protocol caps it at 120).
const PAIRING_WINDOW_S: u32 = 120;

/// Draws the pairing screen.
pub fn draw(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let connected = vm.connection.is_connected();

    block(ui, "Начать сопряжение", |ui| {
        ui.label(
            "На одном компьютере нажмите «Открыть окно сопряжения», на другом введите \
             показанный адрес в поле «Подключиться по адресу».",
        );
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new("Открыть окно сопряжения"))
                .on_hover_text("Другой компьютер введёт адрес, который появится ниже")
                .clicked()
            {
                actions.push(Action::StartPairing {
                    window_s: PAIRING_WINDOW_S,
                });
            }
            hint(ui, "Окно закроется само через 120 секунд");
        });
    });

    actions.extend(window_block(vm, ui));
    actions.extend(dial_block(vm, ui, connected));
    actions.extend(prompt_block(vm, ui, connected));
    actions
}

/// The address the other computer has to be told, and what is left of the window.
fn window_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(window) = &vm.pairing.window else {
        return actions;
    };
    block(ui, "Окно сопряжения открыто", |ui| {
        let address = if window.address.is_empty() {
            "адрес ещё не готов".to_owned()
        } else {
            window.address.clone()
        };
        ui.label(egui::RichText::new(address).monospace().size(30.0).strong());
        ui.label("Скажите этот адрес владельцу другого компьютера");
        ui.label(format!("Осталось {} с", window.seconds_remaining));
        hint(ui, "Он нажмёт «Подключиться по адресу» на своём компьютере");
        if ui.button("Закрыть окно сопряжения").clicked() {
            actions.push(Action::ClosePairingWindow);
        }
    });
    actions
}

/// «Подключиться по адресу».
fn dial_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Подключиться по адресу", |ui| {
        // The buffer is the view model's; a change is reported once, after the widget ran.
        let mut address = vm.pairing.address.clone();
        ui.horizontal(|ui| {
            let entered = ui.add(
                egui::TextEdit::singleline(&mut address)
                    .hint_text("192.168.1.10:47901")
                    .desired_width(240.0),
            );
            if entered.changed() {
                actions.push(Action::SetAddress(address.clone()));
            }
            if ui
                .add_enabled(connected, egui::Button::new("Подключиться по адресу"))
                .clicked()
            {
                actions.push(Action::PairWith(address.clone()));
            }
        });
        hint(ui, "Адрес показывает окно сопряжения на другом компьютере");
        if !connected {
            hint(
                ui,
                "Нужна связь со службой: без неё компьютер не может сопрягаться",
            );
        }
    });
    actions
}

/// The six-digit confirmation of ADR-010.
fn prompt_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(prompt) = &vm.pairing.prompt else {
        return actions;
    };
    let attempt = vm.pairing.attempt_id();
    block(ui, "Проверьте код", |ui| {
        let name = if prompt.peer_name.is_empty() {
            "другой компьютер".to_owned()
        } else {
            prompt.peer_name.clone()
        };
        ui.label(format!("Подключается {name}"));
        let code = vm.pairing.code().unwrap_or_default();
        ui.label(egui::RichText::new(code).monospace().size(40.0).strong());
        ui.label("Скажите партнёру: коды совпадают?");
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new("Да, совпадают"))
                .clicked()
            {
                if let Some(attempt_id) = attempt.clone() {
                    actions.push(Action::Answer {
                        attempt_id,
                        approve: true,
                    });
                }
            }
            if ui
                .add_enabled(connected, egui::Button::new("Нет, не совпадают"))
                .clicked()
            {
                if let Some(attempt_id) = attempt.clone() {
                    actions.push(Action::Answer {
                        attempt_id,
                        approve: false,
                    });
                }
            }
        });
        // ADR-010: the full fingerprints are available, behind «Подробнее».
        let open = vm.pairing.details;
        if ui
            .button(if open {
                "Свернуть"
            } else {
                "Подробнее"
            })
            .clicked()
        {
            actions.push(Action::ToggleDetails(!open));
        }
        if open {
            ui.monospace(format!("Имя партнёра: {name}"));
            ui.monospace(format!(
                "Отпечаток партнёра: {}",
                or_unknown(&prompt.peer_fingerprint)
            ));
            ui.monospace(format!(
                "Отпечаток этого компьютера: {}",
                or_unknown(&prompt.local_fingerprint)
            ));
            hint(ui, "Если отпечатки совпадают, а коды нет — не соглашайтесь");
        }
        if attempt.is_none() {
            ui.colored_label(
                warn_color(),
                "Служба не прислала код попытки — ответить нельзя",
            );
        }
    });
    actions
}

/// A fingerprint the core did not send is not invented, it is said to be missing.
fn or_unknown(value: &str) -> String {
    if value.is_empty() {
        "не сообщён".to_owned()
    } else {
        value.to_owned()
    }
}
