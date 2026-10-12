//! The `eframe::App`: drain the client, fold what it said, draw a screen, send what it asked.
//!
//! There is deliberately no decision in this file and none in [`crate::screens`]: an
//! [`Action`](crate::view_model::Action) produced by a widget is either a `GuiFrame` (handed
//! to [`GuiClient`](crate::view_model::GuiClient)) or a local change handed to the view model,
//! and the view model is where both are interpreted.
//!
//! Which half an action goes to is [`Bridge`], not this file: the window owns the two halves
//! wired together and only draws them, so the same wiring runs in a test with no display.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::screens;
use crate::view_model::{self, Bridge};

/// How often the window redraws even when nothing happens.
///
/// Pushed frames and `try_recv` need a frame to be noticed, and the pairing countdown and a
/// drag need one to be smooth; 250 ms costs nothing and keeps the window honest while it also
/// waits for the client's own redraw requests.
const REPAINT_EVERY: Duration = Duration::from_millis(250);

/// The window: the state that draws, and the client that talks to the core.
pub struct GuiApp {
    bridge: Bridge,
}

impl GuiApp {
    /// Wires the window up from the command line and opens the client thread.
    ///
    /// Every failure before the first frame — the directories, the endpoint, a broken
    /// `config.toml` — is carried into the view model and shown on the status screen, because
    /// a window that refuses to open cannot tell its user anything.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (view, client) = view_model::boot(view_model::cli_args_from_env());
        configure(&cc.egui_ctx);
        GuiApp {
            bridge: Bridge::new(view, client),
        }
    }

    /// Draws every widget and sends what they asked for.
    ///
    /// An `eframe::App` in this version is handed the root `Ui` rather than a `Context`, and
    /// the panels are shown inside it (`ui`-first API of egui 0.36); the panels themselves are
    /// the layout, they decide nothing.
    fn draw(&mut self, ui: &mut egui::Ui) {
        let mut actions = Vec::new();
        egui::Panel::top("lws-gui-header").show(ui, |ui| {
            actions.extend(screens::header(self.bridge.view(), ui));
            actions.extend(screens::outcome_banner(self.bridge.view(), ui));
        });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let view = self.bridge.view();
                    actions.extend(match view.screen {
                        view_model::Screen::Status => screens::status::draw(view, ui),
                        view_model::Screen::Pairing => screens::pairing::draw(view, ui),
                        view_model::Screen::Layout => screens::layout::draw(view, ui),
                        view_model::Screen::Settings => screens::settings::draw(view, ui),
                    });
                });
        });
        for action in actions {
            self.bridge.dispatch(action, Instant::now());
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.bridge.pump() {
            ui.ctx().request_repaint();
        }
        self.bridge.view_mut().tick(Instant::now());
        self.draw(ui);
        ui.ctx().request_repaint_after(REPAINT_EVERY);
    }
}

/// The window's look: the defaults, with a little more room than egui's own spacing.
///
/// The owner of this window is not a developer, so the body text is one point larger than
/// egui's default and the widgets are not packed edge to edge.
fn configure(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
    });
}
