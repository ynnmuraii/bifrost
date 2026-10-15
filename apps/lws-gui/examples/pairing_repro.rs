fn main() -> eframe::Result {
    let mut options = lws_gui::app::native_options();
    options.viewport = options.viewport.with_inner_size([960.0, 700.0]).with_title("UI pairing repro (isolated)");
    eframe::run_native("UI pairing repro (isolated)", options, Box::new(|cc| Ok(Box::new(lws_gui::app::GuiApp::new(cc)))))
}
