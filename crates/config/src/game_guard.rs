//! Local edge-switch policy. Explicit switches and remote leases never consult it.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Always,
    Off,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GameGuardConfig {
    pub mode: Mode,
    pub block_apps: Vec<String>,
    pub allow_apps: Vec<String>,
}

/// Basenames/WM_CLASS only: no paths, controls, wildcards or unbounded IPC text.
pub fn sanitize_app(app: &str) -> Option<String> {
    let app = app.trim();
    if app.is_empty()
        || app.len() > 128
        || app.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | '*' | '?' | ':' | '"' | '<' | '>' | '|')
        })
    {
        return None;
    }
    Some(app.to_ascii_lowercase())
}

impl GameGuardConfig {
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        for (name, apps) in [
            ("game_guard.block_apps", &self.block_apps),
            ("game_guard.allow_apps", &self.allow_apps),
        ] {
            if apps.len() > 64 || apps.iter().any(|app| sanitize_app(app).is_none()) {
                return Err(crate::ConfigError::invalid(name, "invalid application list", "at most 64 basenames/WM_CLASS values, each 1..128 bytes without paths or controls"));
            }
        }
        Ok(())
    }

    pub fn reason(&self, signals: &protocol::ipc::GameGuardSignals) -> Option<&'static str> {
        match self.mode {
            Mode::Off => return None,
            Mode::Always => return Some("always"),
            Mode::Auto => {}
        }
        let contains = |list: &[String]| {
            !signals.app.is_empty()
                && list.iter().any(|item| {
                    item.trim().chars().flat_map(char::to_lowercase).eq(signals
                        .app
                        .trim()
                        .chars()
                        .flat_map(char::to_lowercase))
                })
        };
        if contains(&self.allow_apps) {
            return None;
        }
        if contains(&self.block_apps) {
            return Some("app_list");
        }
        if !signals.available {
            return None;
        }
        if signals.cursor_confined {
            Some("cursor_confined")
        } else if signals.fullscreen {
            Some("fullscreen")
        } else if signals.presentation {
            Some("presentation")
        } else {
            None
        }
    }

    /// Only a local edge push is subject to this policy, never explicit/emergency input.
    pub fn suppress(&self, signals: &protocol::ipc::GameGuardSignals, local_edge: bool) -> bool {
        local_edge && self.reason(signals).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn config_defaults_parse_and_round_trip() {
        let default = crate::Config::from_str("config_version = 2").unwrap();
        assert_eq!(default.game_guard, GameGuardConfig::default());
        let config = crate::Config::from_str("config_version = 2\n[game_guard]\nmode = 'always'\nblock_apps = ['Game.exe']\nallow_apps = ['player.exe']").unwrap();
        assert_eq!(config.game_guard.mode, Mode::Always);
        let encoded = toml::to_string(&config).unwrap();
        assert_eq!(
            crate::Config::from_str(&encoded).unwrap().game_guard,
            config.game_guard
        );
        assert!(
            crate::Config::from_str("config_version = 2\n[game_guard]\nmode = 'unknown'").is_err()
        );
        let invalid = GameGuardConfig {
            block_apps: vec!["x.exe".into(); 65],
            ..Default::default()
        };
        assert!(invalid.validate().is_err());
    }
    #[test]
    fn policy_table() {
        for mode in [Mode::Auto, Mode::Always, Mode::Off] {
            for available in [false, true] {
                for fullscreen in [false, true] {
                    for cursor_confined in [false, true] {
                        let signals = protocol::ipc::GameGuardSignals {
                            available,
                            fullscreen,
                            cursor_confined,
                            ..Default::default()
                        };
                        let config = GameGuardConfig {
                            mode,
                            ..Default::default()
                        };
                        let expected = mode == Mode::Always
                            || (mode == Mode::Auto && available && (fullscreen || cursor_confined));
                        assert_eq!(config.reason(&signals).is_some(), expected);
                        assert_eq!(config.suppress(&signals, true), expected);
                        assert!(
                            !config.suppress(&signals, false),
                            "explicit/emergency and remote input always bypass the guard"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn app_exceptions_and_sanitization() {
        let mut config = GameGuardConfig {
            block_apps: vec!["GAME.EXE".into()],
            ..Default::default()
        };
        let signals = protocol::ipc::GameGuardSignals {
            app: "game.exe".into(),
            ..Default::default()
        };
        assert_eq!(config.reason(&signals), Some("app_list"));
        config.allow_apps.push("Game.exe".into());
        assert_eq!(config.reason(&signals), None);
        for invalid in ["", "../game.exe", "C:\\game.exe", "game\n.exe", "*"] {
            assert!(sanitize_app(invalid).is_none());
        }
        assert_eq!(sanitize_app(" GAME.EXE ").as_deref(), Some("game.exe"));
        assert!(sanitize_app(&"x".repeat(129)).is_none());
        assert_eq!(sanitize_app(&"İ".repeat(64)).unwrap().len(), 128);
    }
}
