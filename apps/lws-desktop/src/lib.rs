//! Lightweight desktop process coordination, independent of the GUI framework.
pub mod assets;
mod instance;
pub use instance::{
    acquire, signal_existing_gui, spawn_role, start_tray, tray_is_ready, InstanceGuard,
    InstanceOutcome, Role, TrayReady, TRAY_START_TIMEOUT,
};
