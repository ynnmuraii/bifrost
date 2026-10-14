//! `lws-gui` — the LAN Workspace window: one binary, no web runtime.
//!
//! # Why eframe/egui with glow
//!
//! Pure Rust, one binary, no webview and no web runtime: the window is the same kind of
//! program as `lws-core`, built by the same toolchain for Windows, X11 and Wayland, and it
//! draws immediately — which is what a status window needs, since every pixel of it is a
//! reading of the current `Status`. The renderer is `glow`, whose libraries are loaded at
//! runtime, so the build needs no system `-dev` packages on Linux.
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
//!   `cargo test -p lws-gui` never needs a display or a GPU.
//! * [`screens`] — one module per screen: widgets in, [`view_model::Action`]s out.
//! * [`app`] — the `eframe::App` that glues the two together and talks to the core.

#![forbid(unsafe_code)]

pub mod app;
pub mod screens;
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
    // `--help` is answered by the dispatcher; a window has no other arguments of its own.
    let _ = args;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1024.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title(TITLE),
        ..Default::default()
    };
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(|cc| Ok(Box::new(app::GuiApp::new(cc)))),
    );
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // No console in this role on Windows; the message is for a terminal launch.
            eprintln!("multideskflow gui: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
