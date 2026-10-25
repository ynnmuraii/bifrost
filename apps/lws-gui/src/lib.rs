//! `lws-gui` — the Bifrost window: one binary, no web runtime.
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

pub use protocol::app_version;
pub use ui_text;

pub mod app;
pub mod autostart;
mod core_exit;
pub mod gui_settings;
pub mod headless_update;
pub mod map;
pub mod ui;
pub mod updater;
pub mod updater_runtime;
pub mod view_model;

// The tests draw the screens with the very same `egui` the window uses, so the version is
// re-exported instead of being repeated in `[dev-dependencies]` (where it could drift).
pub use eframe::egui;

/// The window's title, used by the binary and by the tests: the product name the owner
/// reads in the title bar and in the taskbar. (The top bar draws the wordmark
/// `BIFROST`, in the logo's pixel style.)
pub const TITLE: &str = "Bifrost";

/// Opens the window and returns when it is closed. This is the `gui` role of the one
/// `bifrost` executable (`apps/bifrost`); `args` is argv after the program name and
/// after the `gui` token, and the window reads its global flags (`--root`, …) from the process
/// command line itself (`view_model::cli_args_from_env`, which ignores what it does not know).
///
/// Everything the window needs before its first frame — the directories, the endpoint,
/// `config.toml`, the client thread — is resolved by [`app::GuiApp::new`].
pub fn run_gui(mut args: Vec<String>) -> std::process::ExitCode {
    let updated = args.iter().any(|arg| arg == "--updated");
    args.retain(|arg| arg != "--updated");
    // The window's decisions go to a file (`init_logging`): on Windows this role has no
    // console at all, and "why did the window do that?" is a question about its window.
    init_logging(&view_model::CliArgs::parse(args.iter().cloned()));
    let instance = match lws_desktop::acquire(lws_desktop::Role::Gui) {
        Ok(lws_desktop::InstanceOutcome::Started(instance)) => instance,
        Ok(lws_desktop::InstanceOutcome::Existing) => {
            // `acquire` has just signalled the window that already exists to show itself.
            tracing::info!(
                origin = "single-instance show signal",
                "another Bifrost window is running: it was asked to show itself"
            );
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("bifrost gui: single instance: {error}");
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
                    eprintln!("bifrost gui: tray: {error}");
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
                tracing::info!(
                    origin = "show signal (tray «Открыть Bifrost», or a second launch)",
                    "showing the window"
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            })?;
            Ok(Box::new(app::GuiApp::new(cc, updated)))
        }),
    );
    if updater_runtime::RESTART.load(std::sync::atomic::Ordering::Acquire) {
        // Release the GUI instance lock before starting (or exec-ing) its replacement.
        drop(instance);
        return match updater_runtime::restart(&args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("bifrost restart: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match result {
        Ok(()) => {
            // The close handler only lets the window go while the icon is up; this covers a
            // tray that died in between.
            if !lws_desktop::tray_is_ready() {
                if let Err(error) =
                    lws_desktop::start_tray(&tray_args, lws_desktop::TRAY_START_TIMEOUT)
                {
                    eprintln!("bifrost gui: tray: {error}");
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            // No console in this role on Windows; the message is for a terminal launch.
            eprintln!("bifrost gui: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The window's own log: `<state>/logs/lws-gui.log`, rotating as the daemon's does.
///
/// On Windows this role is a `windows_subsystem = "windows"` program with no console, so
/// neither `eprintln!` nor a stderr layer reaches anybody: a person asking "why did the window
/// do that?" has to be able to open a file. The one thing the window does to its own window —
/// a close request answered with a minimisation rather than an exit (`app::close_decision`) —
/// is exactly such a question, and its origin was unknown the last time the owner reported it.
///
/// The level and the rotation are `config.toml`'s `[log]` section, read here as the daemon
/// reads it, so one file of settings covers both roles; `LWS_LOG` overrides either. A host
/// whose directories cannot be resolved, or whose log directory cannot be opened, keeps
/// stderr alone rather than refusing to open a window.
fn init_logging(args: &view_model::CliArgs) {
    use std::io::IsTerminal;

    let paths = args.resolve().ok();
    let configured = paths
        .as_ref()
        .and_then(|paths| std::fs::read_to_string(paths.config_file()).ok())
        .and_then(|text| config::Config::from_str(&text).ok());
    let level = configured
        .as_ref()
        .map(|config| match config.log.level {
            config::LogLevel::Trace => "trace",
            config::LogLevel::Debug => "debug",
            config::LogLevel::Info => "info",
            config::LogLevel::Warn => "warn",
            config::LogLevel::Error => "error",
        })
        .unwrap_or("info");
    let (max_file_mb, max_files) = configured
        .as_ref()
        .map(|config| (config.log.max_file_mb, config.log.max_files))
        .unwrap_or((5, 5));
    let sink = paths
        .as_ref()
        .and_then(|paths| logging::file_sink(&paths.logs_dir(), "lws-gui", max_file_mb, max_files));
    let filter = tracing_subscriber::EnvFilter::try_from_env("LWS_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level));
    // The same rule as core and the agent: colour belongs to a stderr terminal, never to the
    // file (`logging::subscriber`), whose reader is a person or a parser.
    logging::init(
        filter,
        sink.clone().map(logging::file_writer),
        std::io::stderr().is_terminal(),
    );
    if let Some(sink) = sink {
        if let Ok(path) = sink.lock().map(|file| file.path().to_path_buf()) {
            tracing::info!(path = %path.display(), "logging to the state directory");
        }
    }
}

#[cfg(test)]
mod tests {
    /// The owner reads the product name in the title bar and in the taskbar.
    #[test]
    fn the_window_title_is_the_product_name() {
        assert_eq!(super::TITLE, "Bifrost");
    }
}
