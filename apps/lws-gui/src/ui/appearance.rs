//! Window-local appearance controls, separate from service settings.

use eframe::egui::{self, Align, Layout, Pos2, Rect, Stroke, StrokeKind, UiBuilder, Vec2};

use super::theme::{fam, Palette};
use super::widgets::{self, Kind};
use crate::gui_settings::{FieldMode, LinkStyle, Pace, Theme};
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
    let width = 360.0_f32.min((full.width() - 24.0).max(0.0));
    let height = 548.0_f32.min((full.height() - 24.0).max(0.0));
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
    egui::ScrollArea::vertical()
        .max_height(inner.height())
        .show(&mut panel, |ui| {
            ui.horizontal(|ui| {
                widgets::text(ui, "Внешний вид", 18.0, fam::sans_semibold(), pal.ink);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::button(ui, pal, "Закрыть", Kind::Plain, true).clicked() {
                        actions.push(Action::CloseAppearance);
                    }
                });
            });
            let mut look = vm.look;
            label(ui, pal, "Тема");
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["Antigravity Light", "Overmind Dark"],
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
            if let Some(i) = widgets::seg(
                ui,
                pal,
                &["Спокойно", "Живо"],
                usize::from(look.pace == Pace::Live),
            ) {
                look.pace = if i == 0 { Pace::Calm } else { Pace::Live };
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
            // Service-owned edge controls are added here once their IPC fields land.
            ui.push_id("edge-effects", |ui| {
                label(ui, pal, "Эффекты краёв экрана");
                ui.add_space(44.0);
            });
        });
}

fn label(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.add_space(6.0);
    widgets::text(ui, text, 13.0, fam::sans_medium(), pal.ink);
}
