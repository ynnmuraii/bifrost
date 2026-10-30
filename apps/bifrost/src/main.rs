//! `bifrost` — the one executable of Bifrost.
//!
//! It only chooses a role from `argv` and hands over the rest of the arguments:
//!
//! | `argv`                          | role                                              |
//! |---------------------------------|---------------------------------------------------|
//! | *(none)* or `gui [GLOBAL]`      | the window (`bifrost_gui::run_gui`)                   |
//! | `agent …`                       | the input agent (`bifrost_agent::run_agent`), internal|
//! | `tray [GLOBAL]`                 | the lightweight system tray (`bifrost_tray::run_tray`) |
//! | anything else                   | the core CLI (`bifrost_core::run_core`): `run`, `status`, `switch`, `claim`, `peers`, `pair`, `revoke`, `layout …`, `--help` … |
//!
//! The agent stays a *process of its own*: the core starts `current_exe() agent …`, which is
//! what keeps crash isolation, the watchdog and the agent's no-network rule (held by
//! `bifrost-agent`'s `netguard` and its CI test, not by the linker any more). The window starts the
//! core the same way, as `current_exe() <flags> run`.
//!
//! `windows_subsystem = "windows"` is what makes the window a window and not a console
//! program on Windows: without it, double-clicking the `.exe` opens a black console behind it.
//! The command-line and agent roles bind the parent terminal back (`console`).

#![cfg_attr(windows, windows_subsystem = "windows")]

mod console;

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    adopt_default_directories(&mut args);
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            console::attach_parent();
            println!("Bifrost {}", bifrost_gui::app_version());
            ExitCode::SUCCESS
        }
        None => bifrost_gui::run_gui(args),
        Some("gui") => {
            args.remove(0);
            bifrost_gui::run_gui(args)
        }
        Some("update") => {
            console::attach_parent();
            args.remove(0);
            bifrost_gui::headless_update::run(args)
        }
        Some("tray") => {
            args.remove(0);
            bifrost_tray::run_tray(args)
        }
        Some("agent") => {
            console::attach_parent();
            args.remove(0);
            bifrost_agent::run_agent(args)
        }
        Some(_) => {
            console::attach_parent();
            bifrost_core::run_core(args)
        }
    }
}

/// Moves the pre-0.4.0 application directories once, before any role reads them.
///
/// Explicit slots stay untouched, except legacy default paths forwarded by a
/// pre-0.4.0 updater. `--root`, help/version and the agent role remain inert.
fn adopt_default_directories(args: &mut [String]) {
    let quiet = args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "--version" | "-V" | "agent"));
    let help = args.iter().any(|arg| arg == "--help" || arg == "-h");
    if !quiet && !help {
        config::relocate::adopt(args);
    }
}
