//! «Статус» — the default screen: what this computer is, where input goes, who owns it, and
//! what this machine cannot do (the NFRs of `docs/03-architecture.md`).

use eframe::egui;

use super::{block, bullet, hint, important, ok_color};
use crate::view_model::{Action, ViewModel};

/// How many seconds of the pairing window the button asks for (the protocol caps it at 120).
const PAIRING_WINDOW_S: u32 = 120;

/// Draws the status screen.
pub fn draw(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let connected = vm.connection.is_connected();

    if !connected {
        actions.extend(offline_block(vm, ui, connected));
    }

    this_computer(vm, ui);
    link_block(vm, ui);
    input_block(vm, ui);
    actions.extend(owner_block(vm, ui, connected));
    limits_block(vm, ui);
    actions.extend(pending_edits(vm, ui));
    log_block(vm, ui);

    actions
}

/// «Ядро не запущено»: the endpoint, the two buttons that can change that.
fn offline_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Ядро", |ui| {
        match vm.disconnected_reason() {
            Some(why) => ui.label(format!(
                "Ядро не запущено: {}",
                crate::view_model::short_error(why)
            )),
            None => ui.label(vm.connection_label()),
        };
        hint(ui, &format!("Адрес ядра: {}", vm.endpoint.description()));
        if let Some(core) = &vm.core {
            hint(
                ui,
                &format!("Это окно запустило: {}", core.program.display()),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!connected, egui::Button::new("Запустить"))
                .on_hover_text("Запустить ядро на этом компьютере в фоне")
                .clicked()
            {
                actions.push(Action::StartCore);
            }
            if ui
                .add_enabled(!connected, egui::Button::new("Подключиться снова"))
                .clicked()
            {
                actions.push(Action::ConnectAgain);
            }
            if ui
                .button("Обновить")
                .on_hover_text("Спросить у ядра состояние и раскладку")
                .clicked()
            {
                actions.push(Action::Refresh);
            }
        });
        if matches!(vm.endpoint, crate::view_model::CoreEndpoint::Failed { .. }) {
            important(
                ui,
                "Адрес ядра определить не удалось — «Запустить» здесь не поможет",
            );
        }
    });
    actions
}

/// «Этот компьютер».
fn this_computer(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Этот компьютер", |ui| {
        let name = if vm.config.display_name.is_empty() {
            "Имя не задано".to_owned()
        } else {
            vm.config.display_name.clone()
        };
        ui.label(name);
        ui.label(format!("Приём ввода: {}", vm.backend_label()));
        match vm.short_id_of_self() {
            Some(id) => hint(ui, &format!("Короткий номер: {id}")),
            None => hint(ui, "Короткий номер: ядро его не сообщило"),
        }
        if let Some(note) = &vm.config.note {
            hint(ui, note);
        }
    });
}

/// «Связь»: whether the pair is up, and who is in it.
fn link_block(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Связь", |ui| {
        ui.label(vm.link_line());
        for line in vm.member_lines() {
            bullet(ui, &line);
        }
        if let Some(delay) = vm.delay_line() {
            ui.label(delay);
        }
    });
}

/// «Куда идёт ввод» — the question the window exists to answer.
fn input_block(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Куда идёт ввод", |ui| {
        ui.heading(vm.focus_line());
    });
}

/// «Владелец»: who coordinates, and the one button that changes it.
fn owner_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Владелец", |ui| {
        let (owner, serial) = vm.owner_lines();
        ui.label(owner);
        if !serial.is_empty() {
            hint(ui, &serial);
        }
        let is_owner = vm.status.as_ref().is_some_and(|status| status.is_owner);
        ui.horizontal(|ui| {
            let label = if is_owner {
                "Отдать управление"
            } else {
                "Взять управление"
            };
            if ui
                .add_enabled(connected, egui::Button::new(label))
                .on_hover_text(if is_owner {
                    "Этот компьютер перестанет управлять вводом"
                } else {
                    "Ввод начнёт идти с этого компьютера"
                })
                .clicked()
            {
                actions.push(if is_owner {
                    Action::Abandon
                } else {
                    Action::Claim
                });
            }
            if ui
                .add_enabled(connected, egui::Button::new("Открыть окно сопряжения"))
                .clicked()
            {
                actions.push(Action::StartPairing {
                    window_s: PAIRING_WINDOW_S,
                });
            }
        });
        if !connected {
            hint(ui, "Кнопки станут доступны, когда появится связь с ядром");
        }
    });
    actions
}

/// «Ограничения и деградации» (KVM-056): only what `Status` reported, never a guess.
fn limits_block(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Ограничения и деградации", |ui| {
        let lines = vm.limits();
        if lines.is_empty() {
            ui.colored_label(
                ok_color(),
                "Ограничений нет — этот компьютер умеет всё нужное",
            );
            return;
        }
        for line in lines {
            bullet(ui, &line);
        }
    });
}

/// The pending-edit line of §5.1: a divergent edit is easy to forget about.
fn pending_edits(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let pending = vm
        .status
        .as_ref()
        .is_some_and(|status| status.layout_pending_edits > 0);
    if pending {
        ui.horizontal_wrapped(|ui| {
            important(ui, "Есть непринятая правка раскладки");
            if ui.button("Перейти к раскладке").clicked() {
                actions.push(Action::ShowScreen(crate::view_model::Screen::Layout));
            }
        });
    }
    actions
}

/// The last 20 lines: the window's memory of what the core said.
fn log_block(vm: &ViewModel, ui: &mut egui::Ui) {
    egui::CollapsingHeader::new("Журнал окна")
        .default_open(false)
        .show(ui, |ui| {
            if vm.log.is_empty() {
                ui.label("Пока пусто");
                return;
            }
            for line in &vm.log {
                ui.monospace(line);
            }
        });
}
