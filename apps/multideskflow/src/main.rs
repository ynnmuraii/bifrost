//! `multideskflow` — the one executable of LAN Workspace.
//!
//! It only chooses a role from `argv` and hands over the rest of the arguments:
//!
//! | `argv`                          | role                                              |
//! |---------------------------------|---------------------------------------------------|
//! | *(none)* or `gui [GLOBAL]`      | the window (`lws_gui::run_gui`)                   |
//! | `agent …`                       | the input agent (`lws_agent::run_agent`), internal|
//! | `tray [GLOBAL]`                 | the lightweight system tray (`lws_tray::run_tray`) |
//! | anything else                   | the core CLI (`lws_core::run_core`): `run`, `status`, `switch`, `claim`, `peers`, `pair`, `revoke`, `layout …`, `--help` … |
//!
//! The agent stays a *process of its own*: the core starts `current_exe() agent …`, which is
//! what keeps crash isolation, the watchdog and the agent's no-network rule (held by
//! `lws-agent`'s `netguard` and its CI test, not by the linker any more). The window starts the
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
    match args.first().map(String::as_str) {
        None => lws_gui::run_gui(args),
        Some("gui") => {
            args.remove(0);
            lws_gui::run_gui(args)
        }
        Some("tray") => {
            args.remove(0);
            lws_tray::run_tray(args)
        }
        Some("agent") => {
            console::attach_parent();
            args.remove(0);
            lws_agent::run_agent(args)
        }
        Some(_) => {
            console::attach_parent();
            lws_core::run_core(args)
        }
    }
}
