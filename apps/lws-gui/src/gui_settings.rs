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

/// «Светлая» or «Тёмная»: the same window with different colours, and the same radii. The
/// stored values stay `light` and `dark`, so a file written before the rename still reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// «Светлая».
    #[default]
    Light,
    /// «Тёмная».
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

/// The look of the window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Look {
    /// The theme.
    pub theme: Theme,
    /// The particle field.
    pub field: FieldMode,
    /// The speed of the particles, `SPEED_MIN..=SPEED_MAX`.
    pub speed: f32,
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

/// The slowest the particles may be told to chase.
pub const SPEED_MIN: f32 = 0.3;
/// The fastest.
pub const SPEED_MAX: f32 = 2.0;
/// The speed the slider starts at: exactly the pace «Спокойно».
pub const SPEED_DEFAULT: f32 = 1.0;
/// The speed «Живо» used to be, for a file that still says `pace = "live"`.
const SPEED_LIVE: f32 = 1.8;

impl Default for Look {
    fn default() -> Self {
        Look {
            theme: Theme::default(),
            field: FieldMode::default(),
            speed: SPEED_DEFAULT,
            clouds: true,
            link_style: LinkStyle::default(),
            zoom_percent: 100,
            pan_x: 0,
            pan_y: 0,
            dock_open: true,
        }
    }
}

/// The two-step pace of the window before the slider replaced it; read, never written.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Pace {
    /// «Спокойно».
    Calm,
    /// «Живо».
    Live,
}

impl Pace {
    /// The speed the slider shows for it.
    const fn speed(self) -> f32 {
        match self {
            Pace::Calm => SPEED_DEFAULT,
            Pace::Live => SPEED_LIVE,
        }
    }
}

/// `gui.toml` as any version of the window may have written it.
#[derive(Deserialize)]
#[serde(default)]
struct Stored {
    theme: Theme,
    field: FieldMode,
    clouds: bool,
    link_style: LinkStyle,
    zoom_percent: u16,
    pan_x: i16,
    pan_y: i16,
    dock_open: bool,
    /// The slider, since it replaced the pace.
    speed: Option<f32>,
    /// The pace, as a file written before the slider still says it.
    pace: Option<Pace>,
}

impl Default for Stored {
    fn default() -> Self {
        let look = Look::default();
        Stored {
            theme: look.theme,
            field: look.field,
            clouds: look.clouds,
            link_style: look.link_style,
            zoom_percent: look.zoom_percent,
            pan_x: look.pan_x,
            pan_y: look.pan_y,
            dock_open: look.dock_open,
            speed: None,
            pace: None,
        }
    }
}

impl From<Stored> for Look {
    fn from(stored: Stored) -> Look {
        Look {
            theme: stored.theme,
            field: stored.field,
            // A file with both says the speed; one with neither is a file of the pace era.
            speed: stored
                .speed
                .or_else(|| stored.pace.map(Pace::speed))
                .unwrap_or(SPEED_DEFAULT),
            clouds: stored.clouds,
            link_style: stored.link_style,
            zoom_percent: stored.zoom_percent,
            pan_x: stored.pan_x,
            pan_y: stored.pan_y,
            dock_open: stored.dock_open,
        }
    }
}

impl<'de> Deserialize<'de> for Look {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Stored::deserialize(deserializer)?.into())
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
            look.speed = look.speed.clamp(SPEED_MIN, SPEED_MAX);
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
        assert_eq!(look.speed, SPEED_DEFAULT);
        assert_eq!(look.speed, 1.0);
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
            speed: 1.4,
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
    fn the_old_two_step_pace_reads_as_the_speed_it_meant() {
        let dir = dir("pace");
        let path = path_in(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for (stored, expected) in [
            ("pace = \"calm\"", SPEED_DEFAULT),
            ("pace = \"live\"", SPEED_LIVE),
            ("pace = \"live\"\nspeed = 0.5", 0.5),
        ] {
            std::fs::write(&path, format!("{stored}\n")).expect("write");
            let look = load(&path);
            assert_eq!(look.speed, expected, "{stored}");
            // The written file no longer carries a pace: the slider is the only fact.
            save(&path, &look).expect("write");
            let text = std::fs::read_to_string(&path).expect("read");
            assert!(text.contains("speed = "), "{text}");
            assert!(!text.contains("pace"), "{text}");
        }
        // And the speeds a file may hold are bounded like the slider bounds them.
        for (stored, expected) in [(0.0, SPEED_MIN), (1.6, 1.6), (9.0, SPEED_MAX)] {
            std::fs::write(&path, format!("speed = {stored}\n")).expect("write");
            assert_eq!(load(&path).speed, expected);
        }
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
