//! The window's own settings: theme and background, in a file of the GUI's own.
//!
//! The choice of a theme is a fact about this window and not about the service, so it lives in
//! `gui.toml` next to `config.toml` and the core never reads it (and `config.toml` is never
//! written for it). A missing or broken file is the defaults, and a failed write is silent: a
//! look that cannot be remembered must not become an error dialog.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file name inside the configuration directory.
pub const FILE_NAME: &str = "gui.toml";

/// «Antigravity Light» or «Overmind Dark».
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// «Antigravity Light».
    #[default]
    Light,
    /// «Overmind Dark».
    Dark,
}

impl Theme {
    /// The other one.
    pub const fn toggled(self) -> Theme {
        match self {
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::Light,
        }
    }
}

/// What the particles do near the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldMode {
    /// «Разлетаются».
    Repel,
    /// «Собираются».
    Attract,
    /// «Без движения»: nothing moves, nothing is redrawn on a timer.
    #[default]
    Off,
}

/// How fast they drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pace {
    /// «Спокойно».
    #[default]
    Calm,
    /// «Живо».
    Live,
}

/// The look of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Look {
    /// The theme.
    pub theme: Theme,
    /// The particle field.
    pub field: FieldMode,
    /// Its pace.
    pub pace: Pace,
    /// «ASCII-облака».
    pub clouds: bool,
}

impl Default for Look {
    fn default() -> Self {
        Look {
            theme: Theme::default(),
            field: FieldMode::default(),
            pace: Pace::default(),
            clouds: true,
        }
    }
}

/// Where the settings of the configuration directory `config_dir` are.
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(FILE_NAME)
}

/// Reads the settings; anything that is not a valid file is the defaults.
pub fn load(path: &Path) -> Look {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the settings through a temporary file, so a crash never leaves half of one.
pub fn save(path: &Path, look: &Look) -> std::io::Result<()> {
    let text = toml::to_string(look).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lws-gui-look-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_defaults_are_light_with_a_still_field() {
        let look = Look::default();
        assert_eq!(look.theme, Theme::Light);
        assert_eq!(look.field, FieldMode::Off);
        assert_eq!(look.pace, Pace::Calm);
        assert!(look.clouds);
    }

    #[test]
    fn a_choice_survives_a_restart() {
        let dir = dir("roundtrip");
        let path = path_in(&dir);
        let look = Look {
            theme: Theme::Dark,
            field: FieldMode::Attract,
            pace: Pace::Live,
            clouds: false,
        };
        save(&path, &look).expect("the directory is created");
        assert_eq!(load(&path), look);
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_broken_file_is_the_defaults_and_a_partial_one_keeps_what_it_says() {
        let dir = dir("broken");
        let path = path_in(&dir);
        assert_eq!(load(&path), Look::default());
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(&path, "theme = [").expect("write");
        assert_eq!(load(&path), Look::default());
        std::fs::write(&path, "theme = \"dark\"\n").expect("write");
        let partial = load(&path);
        assert_eq!(partial.theme, Theme::Dark);
        assert_eq!(partial.field, FieldMode::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_core_config_is_not_the_place() {
        assert_eq!(FILE_NAME, "gui.toml");
        assert_ne!(FILE_NAME, "config.toml");
    }
}
