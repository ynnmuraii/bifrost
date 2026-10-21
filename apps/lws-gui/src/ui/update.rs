//! Updater controls share the settings rows' type, palette and buttons.
use super::theme::Palette;
use super::widgets::{self, Kind};
use crate::updater::{Action as Update, State, RELEASES};
use crate::view_model::{Action, ViewModel};
use eframe::egui;
use ui_text::{fill, text, Key};

fn button(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    key: Key,
    action: Update,
    actions: &mut Vec<Action>,
) {
    if widgets::button(ui, pal, text(vm.look.language, key), Kind::Ghost, true).clicked() {
        actions.push(Action::Update(action));
    }
}
pub(super) fn settings(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    let busy = matches!(
        vm.update,
        State::Checking | State::Downloading { .. } | State::Restarting
    );
    if widgets::button(ui, pal, text(lang, Key::CheckUpdates), Kind::Ghost, !busy).clicked() {
        actions.push(Action::Update(Update::Check));
    }
    let mut startup = vm.look.check_updates;
    if ui
        .checkbox(&mut startup, text(lang, Key::StartupUpdates))
        .changed()
    {
        actions.push(Action::Update(Update::Startup(startup)));
    }
    match &vm.update {
        State::Idle => {}
        State::Checking => {
            ui.horizontal(|ui| {
                ui.spinner();
                widgets::hint(ui, pal, text(lang, Key::CheckingUpdates));
            });
        }
        State::Current => {
            widgets::hint(ui, pal, text(lang, Key::UpToDate));
        }
        State::Available(release) => {
            widgets::hint(
                ui,
                pal,
                &fill(
                    lang,
                    Key::UpdateAvailable,
                    &[&release.tag_name.trim_start_matches('v')],
                ),
            );
            if let Some(notes) = &release.body {
                let short: String = notes.chars().take(280).collect();
                widgets::hint(ui, pal, &short);
            }
            ui.hyperlink_to(text(lang, Key::UpdateDetails), RELEASES);
            if vm.update_confirm {
                widgets::hint(ui, pal, text(lang, Key::UpdateConfirm));
                ui.horizontal_wrapped(|ui| {
                    button(ui, pal, vm, Key::InstallUpdate, Update::Install, actions);
                    button(ui, pal, vm, Key::Cancel, Update::Cancel, actions);
                });
            } else {
                button(ui, pal, vm, Key::InstallUpdate, Update::Confirm, actions);
            }
        }
        State::Downloading { received, total } => {
            widgets::hint(ui, pal, text(lang, Key::DownloadingUpdate));
            if let Some(total) = total.filter(|v| *v > 0) {
                ui.add(egui::ProgressBar::new(*received as f32 / total as f32).show_percentage());
            } else {
                ui.spinner();
            }
        }
        State::Restarting => {
            ui.spinner();
            widgets::hint(ui, pal, text(lang, Key::UpdateConfirm));
        }
        State::Error(error) => {
            widgets::hint(ui, pal, text(lang, Key::UpdateError));
            widgets::hint(ui, pal, error);
            ui.hyperlink_to(text(lang, Key::UpdateDetails), RELEASES);
        }
    }
}
pub(super) fn notice(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    if !vm.update_notice {
        return;
    }
    let State::Available(release) = &vm.update else {
        return;
    };
    egui::Area::new(egui::Id::new("update-notice"))
        .anchor(egui::Align2::RIGHT_TOP, [-92.0, 72.0])
        .order(egui::Order::Foreground)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(pal.bg)
                .stroke(egui::Stroke::new(1.0, pal.ink2))
                .inner_margin(16.0)
                .show(ui, |ui| {
                    ui.set_max_width(420.0);
                    widgets::hint(
                        ui,
                        pal,
                        &fill(
                            vm.look.language,
                            Key::UpdateAvailable,
                            &[&release.tag_name.trim_start_matches('v')],
                        ),
                    );
                    ui.horizontal_wrapped(|ui| {
                        button(ui, pal, vm, Key::InstallUpdate, Update::Confirm, actions);
                        button(ui, pal, vm, Key::UpdateLater, Update::Later, actions);
                        button(ui, pal, vm, Key::SkipUpdate, Update::Skip, actions);
                    });
                });
        });
}
