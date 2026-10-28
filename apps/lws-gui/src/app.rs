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
//!   particle field, 1 Hz for a pairing countdown. The field asks for frames only while the
//!   window is visible *and* focused *and* the pointer is over the map, and stops as soon as
//!   the particles have settled where the pointer holds them.
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
            .with_app_id("bifrost")
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
    /// The minimised flag `egui` reported last frame, so a change is logged exactly once.
    ///
    /// The other half of the diagnosis: a window that minimises *without* a
    /// «the window was asked to close» line above it was not this program's close handler.
    reported_minimized: Option<bool>,
    updater: crate::updater_runtime::Runtime,
}

impl GuiApp {
    /// Wires the window up from the command line and opens the client thread.
    ///
    /// Every failure before the first frame — the directories, the endpoint, a broken
    /// `config.toml` — is carried into the view model and shown in the top bar, because a
    /// window that refuses to open cannot tell its user anything.
    pub fn new(cc: &eframe::CreationContext<'_>, updated: bool, login: bool) -> Self {
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
        let saved_look = view.look.clone();
        view.settings.autostart = crate::autostart::enabled();
        let start_core = should_start_core(view.config.config_was_missing, updated, login);
        let mut bridge = Bridge::new(view, client);
        if start_core {
            bridge.dispatch(view_model::Action::StartCore, Instant::now());
        }
        GuiApp {
            bridge,
            ui: UiState::new(Instant::now()),
            look_file,
            saved_look,
            reported_minimized: None,
            updater: crate::updater_runtime::Runtime::new(),
        }
    }

    /// Writes `gui.toml` when the look changed.
    fn save_look(&mut self) {
        let look = &self.bridge.view().look;
        if look == &self.saved_look {
            return;
        }
        self.saved_look = look.clone();
        if let Some(file) = &self.look_file {
            // A look that cannot be remembered is not worth an error dialog.
            let _ = gui_settings::save(file, look);
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let arrived = self.bridge.pump();
        self.bridge.view_mut().tick(now);
        self.updater.tick(&mut self.bridge, ui.ctx());
        let close_requested = ui.ctx().input(|input| input.viewport().close_requested());
        if close_requested
            && !crate::updater_runtime::RESTART.load(std::sync::atomic::Ordering::Acquire)
        {
            // The one place this window is asked to go away, and the one place it decides
            // what "away" means. The origin is the OS (the X, Alt+F4, a taskbar close, a
            // shutdown): nothing in this workspace sends a close request of its own, and the
            // tray and a second launch only ever send *show* (`lws_desktop::acquire`,
            // `app::close_decision`). Logged at INFO because it is rare and because an owner
            // once reported the window minimising on «Открыть окно сопряжения» with a
            // collapsed dock, and the file has to answer that next time.
            let tray_ready = lws_desktop::tray_is_ready();
            let decision = close_decision(tray_ready);
            tracing::info!(
                origin = "a native close request (the title bar's X, Alt+F4, a taskbar close)",
                tray_ready,
                ?decision,
                dock_open = self.bridge.view().look.dock_open,
                modal = ?self.bridge.view().modal,
                "the window was asked to close"
            );
            if decision == Close::Minimize {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }

        let (focused, minimized) = ui.ctx().input(|input| {
            let viewport = input.viewport();
            (viewport.focused, viewport.minimized)
        });
        let minimized = minimized == Some(true);
        if self.reported_minimized != Some(minimized) {
            self.reported_minimized = Some(minimized);
            // Above, the close handler says when *this program* minimised the window. A
            // minimisation with no such line before it was the OS's own idea (or the window
            // manager's), which is what an owner's report of "it minimised by itself" has to
            // be told apart from.
            tracing::info!(minimized, focused = ?focused, "the window's minimised state changed");
        }
        let env = Env {
            now,
            // Animate only when the viewport positively reports focus and is not minimized.
            animate: !minimized && focused == Some(true),
        };
        let output = ui::draw(ui, self.bridge.view(), &mut self.ui, env);

        let acted = !output.actions.is_empty();
        for action in output.actions {
            if let view_model::Action::Update(update) = action {
                self.updater.action(update, &mut self.bridge, ui.ctx());
            } else {
                self.bridge.dispatch(action, now);
            }
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

/// What a close request of the window does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Close {
    /// The window goes; the tray icon stays as the way back.
    Quit,
    /// There is no tray icon: closing would leave the user with no window and no icon, so the
    /// window only goes to the taskbar.
    Minimize,
}

pub(crate) fn close_decision(tray_ready: bool) -> Close {
    if tray_ready {
        Close::Quit
    } else {
        Close::Minimize
    }
}

fn should_start_core(config_was_missing: bool, updated: bool, login: bool) -> bool {
    config_was_missing || updated || login
}

#[cfg(test)]
mod close_tests {
    use super::*;

    #[test]
    fn a_window_is_not_closed_into_nothing() {
        assert_eq!(close_decision(true), Close::Quit);
        assert_eq!(close_decision(false), Close::Minimize);
    }

    #[test]
    fn login_starts_core_but_manual_launch_preserves_a_stopped_core() {
        assert!(should_start_core(false, false, true));
        assert!(!should_start_core(false, false, false));
        assert!(should_start_core(true, false, false));
        assert!(should_start_core(false, true, false));
    }
}
