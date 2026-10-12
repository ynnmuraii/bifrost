//! The four screens, one module each.
//!
//! Every screen has the same shape: `draw(&ViewModel, &mut Ui) -> Vec<Action>`. Widgets read
//! the view model, the user's intent comes back as [`Action`]s, and the app decides who acts
//! on them — nothing here talks to the core or changes state.
//!
//! All of it is Russian and plain: the owner of this window is not a developer, and a code
//! from the core is shown at most in parentheses behind a sentence that says what happened.

pub mod layout;
pub mod pairing;
pub mod settings;
pub mod status;

use eframe::egui;

use crate::view_model::{Action, Screen, ViewModel};
use crate::TITLE;

/// The window's own header: the name, the screen tabs and the state of the link.
///
/// The link line lives here rather than on one screen because it is the one fact that is
/// true on all four (the build spec's «Статус» block, in the words of someone who did not
/// install anything: «Служба не запущена» / «Служба работает» / «Служба не отвечает»).
pub fn header(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.horizontal_wrapped(|ui| {
        ui.heading(TITLE);
        ui.separator();
        for screen in Screen::ALL {
            let selected = vm.screen == screen;
            if ui.selectable_label(selected, screen.title()).clicked() {
                actions.push(Action::ShowScreen(screen));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let color = if vm.silent {
                warn_color()
            } else if vm.connection.is_connected() {
                ok_color()
            } else {
                ui.visuals().weak_text_color()
            };
            ui.colored_label(color, vm.connection_label());
            if vm.screen == Screen::Status && vm.connection.is_connected() {
                let can_stop =
                    vm.settings.service.stopping.is_none() && vm.settings.service.restart.is_none();
                if ui
                    .add_enabled(can_stop, egui::Button::new("Остановить службу"))
                    .clicked()
                {
                    actions.push(Action::StopCore);
                }
            }
        });
    });
    ui.separator();
    actions
}

/// The last finished operation, with a way to make it go away.
pub fn outcome_banner(vm: &ViewModel, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(outcome) = &vm.outcome else {
        return actions;
    };
    ui.horizontal_wrapped(|ui| {
        let color = if outcome.ok { ok_color() } else { warn_color() };
        ui.colored_label(color, outcome.line());
        if ui.button("Скрыть").clicked() {
            actions.push(Action::DismissOutcome);
        }
    });
    actions
}

/// A titled block: the shape every «Блок …» of the build spec has.
pub(crate) fn block<R>(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.group(|ui| {
        ui.vertical(|ui| {
            ui.strong(title);
            body(ui)
        })
    })
    .inner
    .inner
}

/// One line of a list, with a dash so a wrapped line still reads as one item.
pub(crate) fn bullet(ui: &mut egui::Ui, text: &str) {
    ui.label(format!("• {text}"));
}

/// A line the user may need but will rarely read: the detail behind a sentence.
pub(crate) fn hint(ui: &mut egui::Ui, text: &str) {
    ui.small(text);
}

/// A line that has to be noticed.
pub(crate) fn important(ui: &mut egui::Ui, text: &str) {
    ui.colored_label(warn_color(), text);
}

/// The colour of «this is fine».
pub(crate) const fn ok_color() -> egui::Color32 {
    egui::Color32::from_rgb(0x2f, 0xa8, 0x4f)
}

/// The colour of «read this».
pub(crate) const fn warn_color() -> egui::Color32 {
    egui::Color32::from_rgb(0xd8, 0x7a, 0x1e)
}
