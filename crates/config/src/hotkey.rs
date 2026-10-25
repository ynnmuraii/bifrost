//! Shared hotkey grammar and canonical rendering for configuration and registration.

use core::fmt;
use serde::{Deserialize, Serialize};

use crate::{ConfigError, ErrorCode};

/// A configurable desktop shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    EmergencyRelease,
    SwitchNext,
    ReturnLocal,
    ClaimOwnership,
    GameGuardToggle,
}

impl HotkeyAction {
    /// Configuration field name for this action.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmergencyRelease => "emergency_release",
            Self::SwitchNext => "switch_next",
            Self::ReturnLocal => "return_local",
            Self::ClaimOwnership => "claim_ownership",
            Self::GameGuardToggle => "game_guard_toggle",
        }
    }

    fn config_key(self) -> &'static str {
        match self {
            Self::EmergencyRelease => "input.hotkeys.emergency_release",
            Self::SwitchNext => "input.hotkeys.switch_next",
            Self::ReturnLocal => "input.hotkeys.return_local",
            Self::ClaimOwnership => "input.hotkeys.claim_ownership",
            Self::GameGuardToggle => "input.hotkeys.game_guard_toggle",
        }
    }
}

/// `[input.hotkeys]`. Empty non-emergency chords disable their action.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyBindings {
    pub emergency_release: String,
    pub switch_next: String,
    pub return_local: String,
    pub claim_ownership: String,
    pub game_guard_toggle: String,
}

impl Default for HotkeyBindings {
    fn default() -> Self {
        Self {
            emergency_release: "Ctrl+Alt+Shift+F12".to_owned(),
            switch_next: "Ctrl+Alt+Shift+Right".to_owned(),
            return_local: "Ctrl+Alt+Shift+Left".to_owned(),
            claim_ownership: "Ctrl+Alt+Shift+Home".to_owned(),
            game_guard_toggle: "Ctrl+Alt+Shift+G".to_owned(),
        }
    }
}

impl HotkeyBindings {
    pub fn get(&self, action: HotkeyAction) -> &str {
        match action {
            HotkeyAction::EmergencyRelease => &self.emergency_release,
            HotkeyAction::SwitchNext => &self.switch_next,
            HotkeyAction::ReturnLocal => &self.return_local,
            HotkeyAction::ClaimOwnership => &self.claim_ownership,
            HotkeyAction::GameGuardToggle => &self.game_guard_toggle,
        }
    }

    pub fn set(&mut self, action: HotkeyAction, chord: String) {
        *match action {
            HotkeyAction::EmergencyRelease => &mut self.emergency_release,
            HotkeyAction::SwitchNext => &mut self.switch_next,
            HotkeyAction::ReturnLocal => &mut self.return_local,
            HotkeyAction::ClaimOwnership => &mut self.claim_ownership,
            HotkeyAction::GameGuardToggle => &mut self.game_guard_toggle,
        } = chord;
    }

    pub fn iter(&self) -> impl Iterator<Item = (HotkeyAction, &str)> {
        [
            HotkeyAction::EmergencyRelease,
            HotkeyAction::SwitchNext,
            HotkeyAction::ReturnLocal,
            HotkeyAction::ClaimOwnership,
            HotkeyAction::GameGuardToggle,
        ]
        .into_iter()
        .map(|action| (action, self.get(action)))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut parsed = [None, None, None, None, None];
        for (index, (action, chord)) in self.iter().enumerate() {
            let field = action.config_key();
            if chord.trim().is_empty() && action != HotkeyAction::EmergencyRelease {
                continue;
            }
            let hotkey = Hotkey::parse(chord).map_err(|error| {
                ConfigError::coded(ErrorCode::Hotkey, field, chord, error.to_string())
            })?;
            if parsed.iter().flatten().any(|previous| previous == &hotkey) {
                return Err(ConfigError::coded(
                    ErrorCode::Hotkey,
                    field,
                    chord,
                    "a distinct hotkey for each action",
                ));
            }
            if cfg!(target_os = "linux") && hotkey.is_linux_vt_switch() {
                return Err(ConfigError::coded(
                    ErrorCode::Hotkey,
                    field,
                    chord,
                    "a combination other than Ctrl+Alt+F1..F12 (Linux VT switch)",
                ));
            }
            parsed[index] = Some(hotkey);
        }
        Ok(())
    }
}

/// One modifier of a hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Modifier {
    /// Logical Control.
    Ctrl,
    /// Alt / Option.
    Alt,
    /// Shift.
    Shift,
    /// Super / Windows / Command.
    Super,
}

impl Modifier {
    /// The canonical spelling used when a hotkey is rendered back to text.
    pub const fn as_str(self) -> &'static str {
        match self {
            Modifier::Ctrl => "Ctrl",
            Modifier::Alt => "Alt",
            Modifier::Shift => "Shift",
            Modifier::Super => "Super",
        }
    }

    fn parse(token: &str) -> Option<Modifier> {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "alt" => Some(Modifier::Alt),
            "shift" => Some(Modifier::Shift),
            "super" | "meta" | "win" | "cmd" => Some(Modifier::Super),
            _ => None,
        }
    }
}

impl fmt::Display for Modifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed hotkey with canonical modifiers and a supported key name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    modifiers: Vec<Modifier>,
    key: String,
}

impl Hotkey {
    /// Parse `Ctrl+Alt+Shift+F12`-style text.
    ///
    /// Order and case do not matter; a repeated modifier is rejected because it
    /// is almost always a typo (`Ctrl+Ctrl+...`).
    pub fn parse(text: &str) -> Result<Hotkey, HotkeyError> {
        let mut parts = text.split('+').map(str::trim);
        let first = parts.next().unwrap_or_default();
        if first.is_empty() {
            return Err(HotkeyError::Empty);
        }

        let mut modifiers: Vec<Modifier> = Vec::new();
        let mut key: Option<String> = None;
        for token in std::iter::once(first).chain(parts) {
            if token.is_empty() {
                return Err(HotkeyError::EmptyPart(text.to_owned()));
            }
            if let Some(previous) = key.take() {
                return Err(HotkeyError::ModifierAfterKey {
                    after: previous,
                    token: token.to_owned(),
                });
            }
            match Modifier::parse(token) {
                Some(modifier) => {
                    if modifiers.contains(&modifier) {
                        return Err(HotkeyError::DuplicateModifier(token.to_owned()));
                    }
                    modifiers.push(modifier);
                }
                None => key = Some(token.to_owned()),
            }
        }

        let Some(key) = key else {
            return Err(HotkeyError::MissingKey(text.to_owned()));
        };
        let key = canonical_key(&key).ok_or_else(|| HotkeyError::UnsupportedKey(key.clone()))?;
        if modifiers.is_empty() && !is_function_key(&key) {
            return Err(HotkeyError::MissingModifier);
        }
        modifiers.sort_unstable();
        Ok(Hotkey { modifiers, key })
    }

    /// The modifiers, in canonical order and without repetitions.
    pub fn modifiers(&self) -> &[Modifier] {
        &self.modifiers
    }

    /// How many modifiers the hotkey has.
    pub fn modifier_count(&self) -> usize {
        self.modifiers.len()
    }

    /// The canonical key name.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether `modifier` is part of the hotkey.
    pub fn has(&self, modifier: Modifier) -> bool {
        self.modifiers.contains(&modifier)
    }

    /// The Linux virtual-terminal switch conflict of the validation table:
    /// `Ctrl+Alt+F1..F12` **without** Shift changes VT, so
    /// `input.hotkeys.emergency_release` must not be it.
    ///
    /// The rule is platform-specific; this predicate is always available so a
    /// tester can check it on any host, while [`crate::Config::validate`] applies
    /// it when the target OS is Linux.
    pub fn is_linux_vt_switch(&self) -> bool {
        let plain_ctrl_alt = self.has(Modifier::Ctrl)
            && self.has(Modifier::Alt)
            && !self.has(Modifier::Shift)
            && self.modifier_count() == 2;
        plain_ctrl_alt && is_function_key_f1_to_f12(&self.key)
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for modifier in &self.modifiers {
            write!(f, "{modifier}+")?;
        }
        f.write_str(&self.key)
    }
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('F')
        .and_then(|digits| digits.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number))
}

fn canonical_key(token: &str) -> Option<String> {
    let upper = token.to_ascii_uppercase();
    if upper.len() == 1 && upper.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(upper);
    }
    if is_function_key(&upper) {
        return Some(upper);
    }
    let key = match token.to_ascii_lowercase().as_str() {
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdown" | "pgdn" => "PageDown",
        "insert" | "ins" => "Insert",
        "delete" | "del" => "Delete",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "enter" | "return" => "Enter",
        "escape" | "esc" => "Escape",
        "space" => "Space",
        _ => return None,
    };
    Some(key.to_owned())
}

fn is_function_key_f1_to_f12(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let Some(digits) = lower.strip_prefix('f') else {
        return false;
    };
    match digits.parse::<u8>() {
        Ok(number) => (1..=12).contains(&number),
        Err(_) => false,
    }
}

/// Why a hotkey string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotkeyError {
    /// The whole string is empty.
    Empty,
    /// An empty part between two `+`.
    EmptyPart(String),
    /// The same modifier twice.
    DuplicateModifier(String),
    /// A modifier appeared after the key.
    ModifierAfterKey {
        /// The key token that came before.
        after: String,
        /// The modifier-like token that followed.
        token: String,
    },
    /// There is no key: only modifiers, or nothing.
    MissingKey(String),
    /// The key has no portable registration mapping.
    UnsupportedKey(String),
    /// Non-function keys require at least one modifier.
    MissingModifier,
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::Empty => f.write_str("the hotkey is empty"),
            HotkeyError::EmptyPart(text) => {
                write!(f, "`{text}` has an empty part between `+` signs")
            }
            HotkeyError::DuplicateModifier(token) => {
                write!(f, "modifier `{token}` is repeated")
            }
            HotkeyError::ModifierAfterKey { after, token } => write!(
                f,
                "modifier `{token}` appears after the key `{after}`; modifiers come first"
            ),
            HotkeyError::MissingKey(text) => {
                write!(f, "`{text}` has no key, only modifiers")
            }
            HotkeyError::UnsupportedKey(key) => write!(f, "unsupported hotkey key `{key}`"),
            HotkeyError::MissingModifier => {
                f.write_str("at least one modifier is required except for F1..F24")
            }
        }
    }
}

impl std::error::Error for HotkeyError {}
