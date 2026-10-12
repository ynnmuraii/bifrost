//! «Настройки» — the few switches the window owns, and the read-only facts the owner needs.
//!
//! What is here is only what the core can actually do: the clipboard switch, the hotkeys
//! (shown, never written — the window reads `config.toml` for exactly this), the LAN address
//! this computer answers on, a restart of the service the window started, forgetting the
//! portal permissions, revoking a paired computer and writing the diagnostics. There is no
//! autostart switch, because the core does not implement autostart (KVM-051).

use eframe::egui;

use super::{block, hint, important, warn_color};
use crate::view_model::{Action, Restart, ViewModel};

/// Draws the settings screen.
pub fn draw(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    actions.extend(first_run_block(vm, ui));
    actions.extend(clipboard_block(vm, ui));
    keys_block(vm, ui);
    actions.extend(lan_block(vm, ui));
    actions.extend(service_block(vm, ui));
    actions.extend(portal_block(vm, ui, vm.connection.is_connected()));
    actions.extend(revoke_block(vm, ui, vm.connection.is_connected()));
    actions.extend(diagnostics_block(vm, ui, vm.connection.is_connected()));
    // KVM-051: автозапуск не реализован в ядре — переключателя нет.
    actions
}

/// «Настройки созданы автоматически»: the one sentence about a file the core has just made.
///
/// It belongs on the screen that shows the file, and once per session — the view model keeps
/// it shut after «Понятно» and does not raise it again when the service is restarted.
fn first_run_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    if !vm.settings.first_run {
        return actions;
    }
    block(ui, "Первый запуск", |ui| {
        important(ui, "Настройки созданы автоматически");
        hint(
            ui,
            "Служба создала файл config.toml при первом запуске — проверьте адрес и клавиши",
        );
        if ui.button("Понятно").clicked() {
            actions.push(Action::DismissFirstRun);
        }
    });
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

/// «Адрес этого компьютера в локальной сети»: shown, and changed here.
///
/// The list is everything the machine has plus the value the file already names, so an
/// address that is not one of this computer's own is never quietly replaced by one that is.
fn lan_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Адрес в локальной сети", |ui| {
        let current = &vm.config.listen_address;
        let shown = if current.is_empty() {
            "не указан"
        } else {
            current.as_str()
        };
        ui.label(format!("Адрес этого компьютера в локальной сети: {shown}"));
        hint(
            ui,
            "Другой компьютер должен иметь доступ к этому адресу — он вводит его при сопряжении",
        );
        let options = vm.settings.lan.options(current);
        if options.is_empty() {
            hint(ui, "Этот компьютер не сообщил ни одного адреса");
            return;
        }
        let mut selected = vm.settings.lan.selected(current).unwrap_or_default();
        let shown_selection = if selected.is_empty() {
            "выберите адрес".to_owned()
        } else {
            selected.clone()
        };
        egui::ComboBox::from_label("Адрес")
            .selected_text(shown_selection)
            .width(220.0)
            .show_ui(ui, |ui| {
                for option in &options {
                    let picked = ui
                        .selectable_value(&mut selected, option.clone(), option.as_str())
                        .clicked();
                    if picked {
                        actions.push(Action::SelectLanAddress(option.clone()));
                    }
                }
            });
        if ui.button("Применить").clicked() {
            actions.push(Action::ApplyLanAddress);
        }
        match &vm.settings.lan.applied {
            Some(applied) => {
                let color = if applied.ok {
                    super::ok_color()
                } else {
                    warn_color()
                };
                ui.colored_label(color, applied.line());
            }
            None => hint(
                ui,
                "После «Применить» служба перезапустится с новым адресом",
            ),
        }
    });
    actions
}

/// Service launch, restart and the graceful stop available even for a service started elsewhere.
fn service_block(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    block(ui, "Служба", |ui| {
        let restarting = vm.settings.service.restart.is_some();
        let stopping = vm.settings.service.stopping.is_some();
        if !vm.connection.is_connected() {
            if ui
                .add_enabled(
                    !restarting && !stopping,
                    egui::Button::new("Запустить службу"),
                )
                .clicked()
            {
                actions.push(Action::StartCore);
            }
        } else {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !restarting && !stopping,
                        egui::Button::new("Перезапустить службу"),
                    )
                    .on_hover_text(
                        "Служба остановится и запустится снова — настройки начнут работать",
                    )
                    .clicked()
                {
                    actions.push(Action::RestartCore);
                }
                if ui
                    .add_enabled(
                        !restarting && !stopping,
                        egui::Button::new("Остановить службу"),
                    )
                    .on_hover_text("Остановить службу на этом компьютере")
                    .clicked()
                {
                    actions.push(Action::StopCore);
                }
            });
        }
        match &vm.settings.service.restart {
            Some(Restart::Stopping { .. }) => hint(ui, "Служба останавливается…"),
            Some(Restart::Starting) => hint(ui, "Служба запускается…"),
            None => hint(ui, "Перезапуск поможет, если служба перестала отвечать"),
        }
        if stopping {
            hint(ui, "Служба останавливается…");
        }
        if let Some(line) = &vm.settings.service.line {
            let color = if line.ok {
                super::ok_color()
            } else {
                warn_color()
            };
            ui.colored_label(color, line.line());
        }
    });
    actions
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
