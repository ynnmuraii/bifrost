//! Lightweight desktop process coordination, independent of the GUI framework.
pub mod assets;
mod instance;
mod process;
pub use instance::{
    acquire, signal_existing_gui, spawn_role, start_tray, stop_tray, tray_is_ready,
    tray_is_running, InstanceGuard, InstanceOutcome, Role, TrayReady, TRAY_START_TIMEOUT,
};
pub use process::process_has_exited;
