//! The binary: `eframe::run_native` around [`lws_gui::app::GuiApp`], and nothing else.
//!
//! Everything the window needs to know before its first frame — the directories, the
//! endpoint, `config.toml`, the client thread — is resolved by
//! [`lws_gui::app::GuiApp::new`], so this file stays the three lines that open a window.

use lws_gui::app::GuiApp;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title(lws_gui::TITLE),
        ..Default::default()
    };
    eframe::run_native(
        lws_gui::TITLE,
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}
