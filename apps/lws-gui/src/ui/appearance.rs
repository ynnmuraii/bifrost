//! Window-local appearance controls, separate from service settings.

use eframe::egui::{self, Align, Layout, Pos2, Rect, Stroke, StrokeKind, UiBuilder, Vec2};

use super::theme::{fam, Palette};
use super::widgets;
use crate::gui_settings::{FieldMode, LinkStyle, Theme, SPEED_MAX, SPEED_MIN};
use crate::view_model::{Action, ViewModel};

/// Draws the appearance popover next to its rail button, within the window bounds.
pub fn draw(
    ui: &mut egui::Ui,
    pal: &Palette,
    vm: &ViewModel,
    full: Rect,
    anchor: Rect,
    actions: &mut Vec<Action>,
) {
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
                widgets::text(ui, "Внешний вид", 18.0, fam::sans_semibold(), pal.ink);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::close(ui, pal) {
                        actions.push(Action::CloseAppearance);
                    }
                });
            });
            let mut look = vm.look;
            label(ui, pal, "Тема");
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["Светлая", "Тёмная"],
                usize::from(look.theme == Theme::Dark),
            ) {
                look.theme = if i == 0 { Theme::Light } else { Theme::Dark };
            }
            label(ui, pal, "Связи между компьютерами");
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["ASCII-изолиния", "Линия"],
                usize::from(look.link_style == LinkStyle::Line),
            ) {
                look.link_style = if i == 0 {
                    LinkStyle::Ascii
                } else {
                    LinkStyle::Line
                };
            }
            label(ui, pal, "Фон · частицы");
            let selected = match look.field {
                FieldMode::Attract => 0,
                FieldMode::Repel => 1,
                FieldMode::Off => 2,
            };
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["Собираются", "Разлетаются", "Без движения"],
                selected,
            ) {
                look.field = [FieldMode::Attract, FieldMode::Repel, FieldMode::Off][i];
            }
            label(ui, pal, "Скорость частиц");
            // A drag is one decision, not sixty: the value is committed when the drag ends (or
            // on a keystroke), so a drag does not write `gui.toml` once a frame.
            let mut speed = look.speed;
            ui.spacing_mut().slider_width = (ui.available_width() - 52.0).max(80.0);
            let response = ui.add(
                egui::Slider::new(&mut speed, SPEED_MIN..=SPEED_MAX)
                    .fixed_decimals(1)
                    .show_value(true),
            );
            if response.drag_stopped() || (response.changed() && !response.dragged()) {
                look.speed = speed;
            }
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["ASCII-облака", "Без облаков"],
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
                    widgets::switch(ui, pal, "Эффекты у краёв", &mut edge_effects).clicked();
                let cursor = widgets::switch(
                    ui,
                    pal,
                    "Прятать курсор, пока ввод на другой машине",
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
