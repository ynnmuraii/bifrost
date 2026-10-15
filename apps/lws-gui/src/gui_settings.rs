//! The window's own settings: theme, background, map view and dock, in a file of the GUI's own.
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
    #[default]
    Attract,
    /// «Без движения»: nothing moves, nothing is redrawn on a timer.
    Off,
}

/// How the links between computers are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkStyle {
    /// «ASCII-изолиния»: a chain of glyphs.
    #[default]
    Ascii,
    /// «Линия»: a dashed line.
    Line,
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
    /// How links are drawn.
    pub link_style: LinkStyle,
    /// Map scale relative to its fitted base, stored as a percentage (70..=150).
    pub zoom_percent: u16,
    /// How far the map was dragged with the right button, in points (`-PAN_MAX..=PAN_MAX`).
    pub pan_x: i16,
    /// The same, vertically.
    pub pan_y: i16,
    /// The pairing dock is open (`false`: only its handle shows).
    pub dock_open: bool,
}

/// The furthest the map may be panned, in points.
pub const PAN_MAX: i16 = 2000;

impl Default for Look {
    fn default() -> Self {
        Look {
            theme: Theme::default(),
            field: FieldMode::default(),
            pace: Pace::default(),
            clouds: true,
            link_style: LinkStyle::default(),
            zoom_percent: 100,
            pan_x: 0,
            pan_y: 0,
            dock_open: true,
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
        .map(|mut look: Look| {
            look.zoom_percent = look.zoom_percent.clamp(70, 150);
            look.pan_x = look.pan_x.clamp(-PAN_MAX, PAN_MAX);
            look.pan_y = look.pan_y.clamp(-PAN_MAX, PAN_MAX);
            look
        })
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
    fn the_defaults_are_light_with_the_field_gathering_and_ascii_links() {
        let look = Look::default();
        assert_eq!(look.theme, Theme::Light);
        assert_eq!(look.field, FieldMode::Attract);
        assert_eq!(look.link_style, LinkStyle::Ascii);
        assert_eq!(look.pace, Pace::Calm);
        assert!(look.clouds && look.dock_open);
        assert_eq!((look.pan_x, look.pan_y), (0, 0));
    }

    #[test]
    fn a_choice_survives_a_restart() {
        let dir = dir("roundtrip");
        let path = path_in(&dir);
        let look = Look {
            theme: Theme::Dark,
            field: FieldMode::Repel,
            pace: Pace::Live,
            clouds: false,
            link_style: LinkStyle::Line,
            zoom_percent: 125,
            pan_x: -120,
            pan_y: 37,
            dock_open: false,
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
        assert_eq!(partial.field, FieldMode::Attract);
        assert_eq!(partial.zoom_percent, 100);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_core_config_is_not_the_place() {
        assert_eq!(FILE_NAME, "gui.toml");
        assert_ne!(FILE_NAME, "config.toml");
    }

    #[test]
    fn persisted_zoom_and_pan_are_bounded_without_touching_core_config() {
        let dir = dir("zoom");
        let path = path_in(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for (stored, expected) in [(0, 70), (105, 105), (65535, 150)] {
            std::fs::write(&path, format!("zoom_percent = {stored}\n")).expect("write");
            assert_eq!(load(&path).zoom_percent, expected);
        }
        std::fs::write(&path, "pan_x = 30000\npan_y = -30000\n").expect("write");
        let look = load(&path);
        assert_eq!((look.pan_x, look.pan_y), (PAN_MAX, -PAN_MAX));
        assert!(!dir.join("config.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
