//! Appearance controls in the shared rail modal.

use eframe::egui;

use super::modal::row;
use super::theme::{fam, Palette};
use super::widgets;
use crate::gui_settings::{FieldMode, LinkStyle, Theme, SPEED_MAX, SPEED_MIN};
use crate::view_model::{Action, ViewModel};
use ui_text::{text, Key};

pub(super) fn body(ui: &mut egui::Ui, pal: &Palette, vm: &ViewModel, actions: &mut Vec<Action>) {
    let lang = vm.look.language;
    let mut look = vm.look.clone();
    row(ui, pal, text(lang, Key::Theme), false, |ui| {
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[text(lang, Key::Light), text(lang, Key::Dark)],
            usize::from(look.theme == Theme::Dark),
        ) {
            look.theme = if i == 0 { Theme::Light } else { Theme::Dark };
        }
    });
    row(ui, pal, text(lang, Key::AppearanceLinks), false, |ui| {
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[text(lang, Key::ASCIIIsoline), text(lang, Key::Line)],
            usize::from(look.link_style == LinkStyle::Line),
        ) {
            look.link_style = if i == 0 {
                LinkStyle::Ascii
            } else {
                LinkStyle::Line
            };
        }
    });
    row(ui, pal, text(lang, Key::BackgroundParticles), false, |ui| {
        let selected = match look.field {
            FieldMode::Attract => 0,
            FieldMode::Repel => 1,
            FieldMode::Off => 2,
        };
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[
                text(lang, Key::Gather),
                text(lang, Key::Scatter),
                text(lang, Key::Still),
            ],
            selected,
        ) {
            look.field = [FieldMode::Attract, FieldMode::Repel, FieldMode::Off][i];
        }
    });
    row(ui, pal, text(lang, Key::ParticleSpeed), false, |ui| {
        // Commit once on release (or a keystroke), not once per drag frame.
        let mut speed = look.speed;
        let response = ui
            .horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 46.0).max(80.0);
                let response = ui
                    .scope(|ui| {
                        let visuals = ui.visuals_mut();
                        visuals.widgets.inactive.bg_fill = pal.line;
                        visuals.widgets.hovered.bg_fill = pal.line;
                        visuals.widgets.active.bg_fill = pal.line;
                        visuals.selection.bg_fill = pal.accent;
                        visuals.slider_trailing_fill = true;
                        visuals.handle_shape = egui::style::HandleShape::Circle;
                        ui.add(
                            egui::Slider::new(&mut speed, SPEED_MIN..=SPEED_MAX)
                                .fixed_decimals(1)
                                .show_value(false),
                        )
                    })
                    .inner;
                widgets::text(ui, &format!("{speed:.1}"), 13.0, fam::mono(), pal.ink2);
                response
            })
            .inner;
        if response.drag_stopped() || (response.changed() && !response.dragged()) {
            look.speed = speed;
        }
    });
    row(ui, pal, text(lang, Key::AppearanceClouds), false, |ui| {
        if let Some(i) = widgets::seg(
            ui,
            pal,
            &[text(lang, Key::ASCIIClouds), text(lang, Key::NoClouds)],
            usize::from(!look.clouds),
        ) {
            look.clouds = i == 0;
        }
    });
    if look != vm.look {
        actions.push(Action::SetLook(look));
    }
    let mut edge_effects = vm.status.as_ref().is_some_and(|s| s.edge_effects);
    let mut hide_remote_cursor = vm.status.as_ref().is_some_and(|s| s.hide_remote_cursor);
    let mut changed = false;
    row(ui, pal, text(lang, Key::EdgeEffects), false, |ui| {
        ui.push_id("edge-effects", |ui| {
            ui.add_enabled_ui(vm.can_set_appearance(), |ui| {
                if let Some(i) = widgets::seg(
                    ui,
                    pal,
                    &[text(lang, Key::On), text(lang, Key::Off)],
                    usize::from(!edge_effects),
                ) {
                    edge_effects = i == 0;
                    changed = true;
                }
            });
        });
    });
    row(ui, pal, text(lang, Key::AppearanceCursor), true, |ui| {
        ui.push_id("remote-cursor", |ui| {
            ui.add_enabled_ui(vm.can_set_appearance(), |ui| {
                if let Some(i) = widgets::seg(
                    ui,
                    pal,
                    &[text(lang, Key::On), text(lang, Key::Off)],
                    usize::from(!hide_remote_cursor),
                ) {
                    hide_remote_cursor = i == 0;
                    changed = true;
                }
            });
        });
        widgets::hint(
            ui,
            pal,
            text(lang, Key::HideTheCursorWhileInputIsOnAnotherComputer),
        );
    });
    if changed {
        actions.push(Action::SetAppearance {
            edge_effects,
            hide_remote_cursor,
        });
    }
}
