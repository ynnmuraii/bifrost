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
    actions.extend(degraded_banner(vm, ui));

    // «Куда идёт ввод» is the question this window exists to answer, so it is the first block
    // and the largest text on the screen, whether the service is up or not.
    input_block(vm, ui);
    this_computer(vm, ui);
    link_block(vm, ui);
    actions.extend(owner_block(vm, ui, connected));
    details_block(vm, ui);
    limits_block(vm, ui);
    actions.extend(pending_edits(vm, ui));
    log_block(vm, ui);

    actions
}

/// Shows only a degraded state the core reported; retry is an explicit button, never a timer.
fn degraded_banner(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    if !vm.connection.is_connected() {
        return Vec::new();
    }
    let Some(reason) = vm.agent_degraded_reason() else {
        return Vec::new();
    };
    let retry_available = vm
        .status
        .as_ref()
        .is_some_and(|status| status.agent_restart_circuit_open);
    let title = if retry_available {
        "Служба ввода остановлена"
    } else {
        "Служба ввода работает с ограничениями"
    };
    let mut actions = Vec::new();
    block(ui, title, |ui| {
        important(ui, &format!("Причина: {reason}"));
        let retry = ui.add_enabled(retry_available, egui::Button::new("Повторить"));
        if retry_available && retry.clicked() {
            actions.push(Action::RetryAgent);
        } else if !retry_available {
            retry.on_hover_text(
                "Повтор станет доступен, когда служба ввода остановит автоматические попытки",
            );
        }
    });
    actions
}

/// «Куда идёт ввод» — the question the window exists to answer, as big as a question can be.
fn input_block(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Куда идёт ввод", |ui| {
        ui.label(egui::RichText::new(vm.focus_line()).size(26.0).strong());
        if let Some(detail) = vm.focus_detail() {
            hint(ui, &detail);
        }
        if let Some(status) = &vm.status {
            if !status.transition_disabled_reason.is_empty() {
                important(ui, &status.transition_disabled_reason);
            }
        }
    });
}

/// «Служба не запущена»: the address it would answer on, and the two buttons that change that.
fn offline_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Служба", |ui| {
        match vm.disconnected_reason() {
            Some(why) => ui.label(format!(
                "Служба не запущена: {}",
                crate::view_model::short_error(why)
            )),
            None => ui.label(vm.connection_label()),
        };
        hint(ui, &format!("Адрес службы: {}", vm.endpoint.description()));
        if let Some(core) = &vm.core {
            hint(
                ui,
                &format!("Это окно запустило: {}", core.program.display()),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!connected, egui::Button::new("Запустить"))
                .on_hover_text("Запустить службу на этом компьютере в фоне")
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
                .on_hover_text("Спросить у службы состояние и раскладку")
                .clicked()
            {
                actions.push(Action::Refresh);
            }
        });
        if matches!(vm.endpoint, crate::view_model::CoreEndpoint::Failed { .. }) {
            important(
                ui,
                "Адрес службы определить не удалось — «Запустить» здесь не поможет",
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
        ui.label(format!("Система: {}", vm.backend_label()));
        if let Some(note) = &vm.config.note {
            hint(ui, note);
        }
    });
}

/// «Подробнее»: the two numbers that only a support question needs.
///
/// The short id and the owner's session number were lines on the main screen, where they read
/// as instructions to a person who has no use for them; a window whose owner is not a
/// developer keeps them one click away.
fn details_block(vm: &ViewModel, ui: &mut egui::Ui) {
    let short_id = vm.short_id_of_self();
    let (_, serial) = vm.owner_lines();
    egui::CollapsingHeader::new("Подробнее")
        .default_open(false)
        .show(ui, |ui| {
            match short_id {
                Some(id) => hint(ui, &format!("Короткий номер этого компьютера: {id}")),
                None => hint(
                    ui,
                    "Короткий номер этого компьютера: служба его не сообщила",
                ),
            }
            if serial.is_empty() {
                hint(ui, "Номер сеанса: пока нет");
            } else {
                hint(ui, &format!("Номер сеанса: {serial}"));
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
        for line in vm.inactive_link_lines() {
            important(ui, &line);
        }
        if let Some(delay) = vm.delay_line() {
            ui.label(delay);
        }
    });
}

/// «Владелец»: who coordinates, and the one button that changes it.
fn owner_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Владелец", |ui| {
        let (owner, _serial) = vm.owner_lines();
        ui.label(owner);
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
            hint(
                ui,
                "Кнопки станут доступны, когда появится связь со службой",
            );
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
