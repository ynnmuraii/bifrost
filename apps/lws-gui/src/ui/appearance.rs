//! Window-local appearance controls, separate from service settings.

use eframe::egui::{self, Align, Layout, Pos2, Rect, Stroke, StrokeKind, UiBuilder, Vec2};

use super::theme::{fam, Palette};
use super::widgets;
use crate::gui_settings::{FieldMode, LinkStyle, Theme, SPEED_MAX, SPEED_MIN};
use crate::view_model::{Action, ViewModel};
use ui_text::{text, Key};

/// Draws the appearance popover next to its rail button, within the window bounds.
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    full: Rect,
    anchor: Rect,
    actions: &mut Vec<Action>,
) {
    let lang = vm.look.language;
    if !vm.appearance_open {
        return;
    }
    let width = 400.0_f32.min((full.width() - 24.0).max(0.0));
    let height = 572.0_f32.min((full.height() - 24.0).max(0.0));
    let x = (anchor.right() + 12.0)
        .min(full.right() - width - 12.0)
        .max(full.left() + 12.0);
    let y = anchor
        .top()
        .min(full.bottom() - height - 12.0)
        .max(full.top() + 12.0);
    let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(width, height));
    if ui.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !rect.contains(p) && !anchor.contains(p)))
    }) {
        actions.push(Action::CloseAppearance);
    }
    ui.painter().rect(
        rect,
        pal.rad(),
        pal.bg,
        Stroke::new(1.0, pal.line),
        StrokeKind::Inside,
    );
    // Consume clicks in the panel rather than passing them to the map beneath it.
    ui.interact(
        rect,
        egui::Id::new("appearance-panel"),
        egui::Sense::click(),
    );
    let inner = rect.shrink(20.0);
    let mut panel = ui.new_child(
        UiBuilder::new()
            .id_salt("appearance")
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    panel.set_clip_rect(rect.intersect(full));
    panel.spacing_mut().item_spacing.y = 10.0;
    // The bar is an overlay that appears only when the content really scrolls, and the width
    // it may take is reserved for it, so it can never cover «Без движения».
    panel.style_mut().spacing.scroll.floating_allocated_width = 6.0;
    egui::ScrollArea::vertical()
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .max_height(inner.height())
        .show(&mut panel, |ui| {
            ui.horizontal(|ui| {
                widgets::text(
                    ui,
                    text(lang, Key::Appearance2),
                    18.0,
                    fam::sans_semibold(),
                    pal.ink,
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::close(ui, pal) {
                        actions.push(Action::CloseAppearance);
                    }
                });
            });
            let mut look = vm.look;
            label(ui, pal, text(lang, Key::Theme));
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[text(lang, Key::Light), text(lang, Key::Dark)],
                usize::from(look.theme == Theme::Dark),
            ) {
                look.theme = if i == 0 { Theme::Light } else { Theme::Dark };
            }
            label(ui, pal, text(lang, Key::LinksBetweenComputers));
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
            label(ui, pal, text(lang, Key::BackgroundParticles));
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
            label(ui, pal, text(lang, Key::ParticleSpeed));
            // A drag is one decision, not sixty: the value is committed when the drag ends (or
            // on a keystroke), so a drag does not write `gui.toml` once a frame.
            let mut speed = look.speed;
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 46.0).max(80.0);
                    let response = ui
                        .scope(|ui| {
                            // The one slider of the window, in the shared rounded style: a
                            // visible rail, the part the pointer has passed in the accent, and
                            // a round knob. Nothing else here uses these tokens.
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
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &[text(lang, Key::ASCIIClouds), text(lang, Key::NoClouds)],
                usize::from(!look.clouds),
            ) {
                look.clouds = i == 0;
            }
            if look != vm.look {
                actions.push(Action::SetLook(look));
            }
            ui.add_space(6.0);
            ui.separator();
            let mut edge_effects = vm.status.as_ref().is_some_and(|s| s.edge_effects);
            let mut hide_remote_cursor = vm.status.as_ref().is_some_and(|s| s.hide_remote_cursor);
            ui.add_enabled_ui(vm.can_set_appearance(), |ui| {
                let effects =
                    widgets::switch(ui, pal, text(lang, Key::EdgeEffects), &mut edge_effects)
                        .clicked();
                let cursor = widgets::switch(
                    ui,
                    pal,
                    text(lang, Key::HideTheCursorWhileInputIsOnAnotherComputer),
                    &mut hide_remote_cursor,
                )
                .clicked();
                if effects || cursor {
                    actions.push(Action::SetAppearance {
                        edge_effects,
                        hide_remote_cursor,
                    });
                }
            });
        });
}

fn label(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.add_space(6.0);
    widgets::text(ui, text, 13.0, fam::sans_medium(), pal.ink);
}
