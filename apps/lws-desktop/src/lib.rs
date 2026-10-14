//! Lightweight desktop process coordination, independent of the GUI framework.
pub mod assets;
mod instance;
pub use instance::{
    acquire, signal_existing_gui, spawn_role, InstanceGuard, InstanceOutcome, Role,
};
