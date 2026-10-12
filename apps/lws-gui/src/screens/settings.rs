//! «Настройки» — the few switches the window owns, and the read-only facts the owner needs.
//!
//! What is here is only what the core can actually do: the clipboard switch, the hotkeys
//! (shown, never written — the window reads `config.toml` for exactly this), forgetting the
//! portal permissions, revoking a paired computer and writing the diagnostics. There is no
//! autostart switch, because the core does not implement autostart (KVM-051).

use eframe::egui;

use super::{block, hint, important, warn_color};
use crate::view_model::{Action, ViewModel};

/// Draws the settings screen.
pub fn draw(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    actions.extend(clipboard_block(vm, ui));
    keys_block(vm, ui);
    actions.extend(portal_block(vm, ui, vm.connection.is_connected()));
    actions.extend(revoke_block(vm, ui, vm.connection.is_connected()));
    actions.extend(diagnostics_block(vm, ui, vm.connection.is_connected()));
    // KVM-051: автозапуск не реализован в ядре — переключателя нет.
    actions
}

/// «Общий буфер обмена».
fn clipboard_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Общий буфер обмена", |ui| {
        let Some(status) = &vm.status else {
            ui.label("Нет данных от службы");
            return;
        };
        let mut enabled = status.clipboard_enabled;
        let response = ui.add_enabled(
            vm.connection.is_connected(),
            egui::Checkbox::new(&mut enabled, "Общий буфер обмена"),
        );
        if response.changed() {
            actions.push(Action::SetClipboard(enabled));
        }
        if status.clipboard_enabled {
            ui.label("Текст можно копировать на одном компьютере и вставлять на другом");
        } else {
            ui.label("Общий буфер обмена выключен");
        }
        if let Some(note) = &vm.settings.clipboard_note {
            important(ui, note);
        }
    });
    actions
}

/// The hotkeys: read from `config.toml`, and the emergency key from the core itself.
fn keys_block(vm: &ViewModel, ui: &mut egui::Ui) {
    block(ui, "Клавиши", |ui| {
        let emergency = vm
            .status
            .as_ref()
            .map(|status| status.emergency_hotkey.clone())
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| "не сообщена службой".to_owned());
        ui.label(format!("Аварийная клавиша: {emergency}"));
        ui.label(format!("Клавиша переключения: {}", vm.config.switch_hotkey));
        ui.label(format!(
            "Клавиша взятия управления: {}",
            vm.config.claim_hotkey
        ));
        hint(
            ui,
            &format!("Значения прочитаны из {}", vm.config.config_file.display()),
        );
        if let Some(note) = &vm.config.note {
            important(ui, note);
        }
    });
}

/// «Забыть разрешения порталов».
fn portal_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Разрешения порталов", |ui| {
        ui.label("Понадобится, если вы отказались в диалоге разрешения или сменили рабочий стол");
        if let Some(status) = &vm.status {
            if !status.portal_state.is_empty() {
                hint(ui, &format!("Состояние порталов: {}", status.portal_state));
            }
        }
        if ui
            .add_enabled(connected, egui::Button::new("Забыть разрешения порталов"))
            .on_hover_text("Служба попросит разрешения заново")
            .clicked()
        {
            actions.push(Action::ResetPortalGrants);
        }
    });
    actions
}

/// «Удалить компьютер из пары», with the confirmation the button needs.
fn revoke_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Пары", |ui| {
        let members = vm
            .status
            .as_ref()
            .map(|status| status.members.clone())
            .unwrap_or_default();
        if members.is_empty() {
            ui.label("Пока никого: откройте окно сопряжения");
            return;
        }
        ui.label("Компьютер для удаления:");
        for member in &members {
            let name = if member.display_name.is_empty() {
                format!("компьютер {}", crate::view_model::short_id(&member.node_id))
            } else {
                member.display_name.clone()
            };
            let chosen = vm.settings.revoke.as_deref() == Some(member.node_id.as_str());
            if ui.selectable_label(chosen, name).clicked() {
                actions.push(Action::SelectRevoke(Some(member.node_id.clone())));
            }
        }
        let Some(chosen) = vm.settings.revoke.clone() else {
            hint(ui, "Выберите компьютер в списке");
            return;
        };
        let name = members
            .iter()
            .find(|member| member.node_id == chosen)
            .map(|member| member.display_name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("компьютер {}", crate::view_model::short_id(&chosen)));
        if !vm.settings.revoke_confirmed {
            if ui.button("Удалить компьютер из пары").clicked() {
                actions.push(Action::ConfirmRevoke(true));
            }
            return;
        }
        important(
            ui,
            &format!("Удалить «{name}» из пары? Сопрягать придётся заново"),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(connected, egui::Button::new("Да, удалить"))
                .clicked()
            {
                actions.push(Action::RevokePeer(hex_bytes(&chosen)));
            }
            if ui.button("Отмена").clicked() {
                actions.push(Action::ConfirmRevoke(false));
            }
        });
    });
    actions
}

/// «Сохранить диагностику».
fn diagnostics_block(vm: &ViewModel, ui: &mut egui::Ui, connected: bool) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Диагностика", |ui| {
        if ui
            .add_enabled(connected, egui::Button::new("Сохранить диагностику"))
            .on_hover_text("Служба запишет отчёт без ключей и содержимого буфера обмена")
            .clicked()
        {
            // An empty path asks the core to choose one: it knows its own directories.
            actions.push(Action::ExportDiagnostics(String::new()));
        }
        match &vm.settings.export {
            Some(outcome) => {
                if outcome.ok {
                    ui.label("Файл сохранён в каталог состояния");
                }
                let color = if outcome.ok {
                    super::ok_color()
                } else {
                    warn_color()
                };
                ui.colored_label(color, outcome.line());
            }
            None => hint(ui, "Отчёт попадёт в каталог состояния службы"),
        }
        ui.label(format!(
            "Каталог состояния: {}",
            vm.config.state_dir.display()
        ));
    });
    actions
}

/// `RevokePeer.peer_node_id`: 32 identity bytes, or none when the id is not one.
fn hex_bytes(node_id: &str) -> Vec<u8> {
    crate::view_model::node_id_bytes(node_id)
}
