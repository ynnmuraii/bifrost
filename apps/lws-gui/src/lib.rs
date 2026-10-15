//! `lws-gui` — the LAN Workspace window: one binary, no web runtime.
//!
//! # Why eframe/egui with glow
//!
//! Pure Rust, one binary, no webview and no web runtime: the window is the same kind of
//! program as `lws-core`, built by the same toolchain for Windows, X11 and Wayland. The
//! renderer is `glow` (OpenGL, libraries loaded at runtime, no system `-dev` packages on
//! Linux) and not `wgpu`, because this window is open all day and mostly idle: glow creates no
//! GPU device, no pipeline or shader caches and no staging belts, so its resident size is a
//! fraction of wgpu's, and with eframe's reactive loop an idle frame costs nothing at all
//! (`docs/design` of the UI and the measurements in the build report).
//!
//! # Responsibility
//!
//! `docs/03-architecture.md`, «GUI (NFR)» and the `lws-gui` row of the responsibility table:
//! the window shows *where input goes*, who owns it, what this machine can and cannot do,
//! and it is the only place a human pairs two computers or edits the layout document. It has
//! no path to the FSM, the network or the input devices — everything it does is a `GuiFrame`
//! on the local channel (`crate::view_model`), and closing it does not touch a running
//! session.
//!
//! # Layout of the crate
//!
//! * [`view_model`] — all state and all decisions, with no `egui` type in sight, so
//!   `cargo test -p lws-gui` never needs a display or a GPU; [`view_model::layout`] is the
//!   map's model on top of [`map`].
//! * [`map`] — the geometry of «Карта мониторов»: placement, snapping, links; pure functions.
//! * [`gui_settings`] — the theme and background, in the GUI's own `gui.toml`.
//! * [`ui`] — the window of the mockup: widgets in, [`view_model::Action`]s out.
//! * [`app`] — the `eframe::App` that glues the two together and decides when to repaint.

#![forbid(unsafe_code)]

pub mod app;
pub mod gui_settings;
pub mod map;
pub mod ui;
pub mod view_model;

// The tests draw the screens with the very same `egui` the window uses, so the version is
// re-exported instead of being repeated in `[dev-dependencies]` (where it could drift).
pub use eframe::egui;

/// The window's title, used by the binary and by the tests.
pub const TITLE: &str = "LAN Workspace";

/// Opens the window and returns when it is closed. This is the `gui` role of the one
/// `multideskflow` executable (`apps/multideskflow`); `args` is argv after the program name and
/// after the `gui` token, and the window reads its global flags (`--root`, …) from the process
/// command line itself (`view_model::cli_args_from_env`, which ignores what it does not know).
///
/// Everything the window needs before its first frame — the directories, the endpoint,
/// `config.toml`, the client thread — is resolved by [`app::GuiApp::new`].
pub fn run_gui(args: Vec<String>) -> std::process::ExitCode {
    let instance = match lws_desktop::acquire(lws_desktop::Role::Gui) {
        Ok(lws_desktop::InstanceOutcome::Started(instance)) => instance,
        Ok(lws_desktop::InstanceOutcome::Existing) => return std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("multideskflow gui: single instance: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    // The tray is started with the window, not after it closes: by the time the user closes the
    // window the icon is normally there, and the close handler (`app::close_decision`) can
    // refuse to leave the user with neither.
    let tray_args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
    {
        let tray_args = tray_args.clone();
        let _ = std::thread::Builder::new()
            .name("lws-tray-start".into())
            .spawn(move || {
                if let Err(error) =
                    lws_desktop::start_tray(&tray_args, lws_desktop::TRAY_START_TIMEOUT)
                {
                    eprintln!("multideskflow gui: tray: {error}");
                }
            });
    }
    let options = app::native_options();
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();
            instance.set_show_handler(move || {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            })?;
            Ok(Box::new(app::GuiApp::new(cc)))
        }),
    );
    match result {
        Ok(()) => {
            // The close handler only lets the window go while the icon is up; this covers a
            // tray that died in between.
            if !lws_desktop::tray_is_ready() {
                if let Err(error) =
                    lws_desktop::start_tray(&tray_args, lws_desktop::TRAY_START_TIMEOUT)
                {
                    eprintln!("multideskflow gui: tray: {error}");
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            // No console in this role on Windows; the message is for a terminal launch.
            eprintln!("multideskflow gui: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
