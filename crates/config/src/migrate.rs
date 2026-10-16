//! The one-shot, on-disk half of the legacy-hotkey compatibility.
//!
//! [`crate::Config::load`] accepts the legacy keys everywhere, so a read-only
//! role — the GUI, the tray, a CLI status — starts on a pre-`[input.hotkeys]`
//! file without touching it. The core owns `config.toml`, so it calls
//! [`migrate_legacy_hotkeys`] once: the file is read, the legacy keys are folded
//! into `[input.hotkeys]` and the whole document is written back through
//! [`crate::update_locked`], the same lock and the same single writer every other
//! persistence path uses, so a concurrent ownership or layout write survives.

use std::path::Path;

use crate::error::ConfigError;
use crate::legacy;
use crate::lock::update_locked;

/// Rewrites `config_path` through the single-writer path when it still carries a
/// legacy hotkey key, and says whether it did.
///
/// The second start therefore does not write: once `[input.hotkeys]` is in the
/// file there is no legacy key left to find.
pub fn migrate_legacy_hotkeys(config_path: &Path) -> Result<bool, ConfigError> {
    if !carries_legacy_hotkeys(config_path)? {
        return Ok(false);
    }
    update_locked(config_path, |_| ())?;
    Ok(true)
}

/// True when the file at `config_path` still sets a legacy hotkey key.
fn carries_legacy_hotkeys(config_path: &Path) -> Result<bool, ConfigError> {
    let text = std::fs::read_to_string(config_path).map_err(|source| ConfigError::Io {
        path: config_path.to_path_buf(),
        source,
    })?;
    let document: toml::Value =
        toml::from_str(&text).map_err(|source| ConfigError::Parse { source })?;
    Ok(legacy::present(&document))
}
