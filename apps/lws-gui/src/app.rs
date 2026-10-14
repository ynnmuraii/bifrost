//! The `eframe::App`: drain the client, fold what it said, draw the window, send what it asked.
//!
//! There is deliberately no decision in this file and none in [`crate::ui`]: an
//! [`Action`](crate::view_model::Action) produced by a widget is either a `GuiFrame` (handed
//! to [`GuiClient`](crate::view_model::GuiClient)) or a local change handed to the view model,
//! and the view model is where both are interpreted — [`Bridge`] is the one place that wires
//! them, so the same wiring runs in a test with no display.
//!
//! # Why this window costs nothing while it is idle
//!
//! * The renderer is `glow` (see the crate docs): no GPU device, no pipeline caches, and its
//!   resident size is the smallest of the two.
//! * eframe is reactive: a frame is drawn for an input event, for a repaint somebody asked
//!   for, and for nothing else. This file asks for exactly three things: a repaint when the
//!   client thread says something (`GuiClient::set_wake`), the one frame at which a pending
//!   request runs out (`ViewModel::next_wakeup`), and what `ui::draw` reports — 30 fps for the
//!   particle field, 1 Hz for a pairing countdown — and the field only while the window is
//!   visible *and* focused.
//! * A minimised window asks for no timed frame at all.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::gui_settings::{self, Look};
use crate::ui::{self, Env, UiState};
use crate::view_model::{self, Bridge};

/// The window's options: the size the mockup was drawn for, and the smallest it stays usable at.
pub fn native_options() -> eframe::NativeOptions {
    let (width, height, rgba) = lws_desktop::assets::icon_rgba();
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 720.0])
            .with_title(crate::TITLE)
            .with_icon(egui::IconData {
                rgba: rgba.to_vec(),
                width,
                height,
            }),
        ..Default::default()
    }
}

/// The window: the state that draws, and the client that talks to the core.
pub struct GuiApp {
    bridge: Bridge,
    ui: UiState,
    /// Where `gui.toml` is, when the configuration directory is known.
    look_file: Option<PathBuf>,
    /// The look as last written, so a file is written only when it changes.
    saved_look: Look,
    frame_probe: Option<(PathBuf, Instant, u64)>,
}

impl GuiApp {
    /// Wires the window up from the command line and opens the client thread.
    ///
    /// Every failure before the first frame — the directories, the endpoint, a broken
    /// `config.toml` — is carried into the view model and shown in the top bar, because a
    /// window that refuses to open cannot tell its user anything.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (mut view, client) = view_model::boot(view_model::cli_args_from_env());
        ui::install_fonts(&cc.egui_ctx);
        // The client thread repaints the window when — and only when — it has something to say.
        let ctx = cc.egui_ctx.clone();
        client.set_wake(move || ctx.request_repaint());
        let look_file = view
            .config
            .paths
            .as_ref()
            .map(|paths| gui_settings::path_in(paths.config_dir()));
        if let Some(file) = &look_file {
            view.look = gui_settings::load(file);
        }
        let saved_look = view.look;
        GuiApp {
            bridge: Bridge::new(view, client),
            ui: UiState::new(Instant::now()),
            look_file,
            saved_look,
            frame_probe: std::env::var_os("MDF_FRAME_PROBE").map(|path| {
                (
                    PathBuf::from(path),
                    Instant::now() + Duration::from_secs(5),
                    0,
                )
            }),
        }
    }

    /// Writes `gui.toml` when the look changed.
    fn save_look(&mut self) {
        let look = self.bridge.view().look;
        if look == self.saved_look {
            return;
        }
        self.saved_look = look;
        if let Some(file) = &self.look_file {
            // A look that cannot be remembered is not worth an error dialog.
            let _ = gui_settings::save(file, &look);
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        if let Some((_, started, frames)) = &mut self.frame_probe {
            if now >= *started {
                *frames += 1;
            }
        }
        let arrived = self.bridge.pump();
        self.bridge.view_mut().tick(now);

        let (focused, minimized) = ui.ctx().input(|input| {
            let viewport = input.viewport();
            (viewport.focused, viewport.minimized)
        });
        let minimized = minimized == Some(true);
        let env = Env {
            now,
            // Unknown counts as visible and focused: a platform that does not report it must
            // not lose its animation, and the window's own events still end it on blur.
            animate: !minimized && focused != Some(false),
        };
        let output = ui::draw(ui, self.bridge.view(), &mut self.ui, env);

        let acted = !output.actions.is_empty();
        for action in output.actions {
            self.bridge.dispatch(action, now);
        }
        self.save_look();
        // What an action changed is drawn by the next frame; an arrival was folded after the
        // widgets that show it were laid out.
        if acted || arrived {
            ui.ctx().request_repaint();
        }
        if minimized {
            return;
        }
        let pending = self.bridge.view().next_wakeup(now);
        let wanted: Option<Duration> = match (output.repaint_after, pending) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        if let Some(after) = wanted {
            ui.ctx().request_repaint_after(after);
        }
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        if let Some((path, started, frames)) = &self.frame_probe {
            let _ = std::fs::write(
                path,
                format!(
                    "{frames},{}\n",
                    Instant::now()
                        .saturating_duration_since(*started)
                        .as_secs_f64()
                ),
            );
        }
    }
}
