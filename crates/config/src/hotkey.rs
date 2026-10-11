//! Hotkey parsing for the `Ctrl+Alt+Shift+F12` strings of the configuration.
//!
//! The validation table of `docs/03-architecture.md` counts modifiers
//! («`workspace.claim_hotkey` | ≥2 модификатора + клавиша», «`input.emergency_hotkey`
//! | ≥3 модификатора + клавиша») and forbids reusing the same combination, so the
//! loader needs the parts, not just the text.
//!
//! Grammar: one or more modifiers joined by `+`, then a key, e.g.
//! `Ctrl+Alt+Shift+F12`. Modifier names are case-insensitive: `ctrl`/`control`,
//! `alt`, `shift`, `super`/`meta`/`win`/`cmd`. Modifiers may appear in any order
//! and are normalised to a canonical order, so `Alt+Ctrl+F1` equals `Ctrl+Alt+F1`.

use core::fmt;

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

/// A parsed hotkey: two or more modifiers plus a key name.
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

    /// The key name, exactly as written in the configuration.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether `modifier` is part of the hotkey.
    pub fn has(&self, modifier: Modifier) -> bool {
        self.modifiers.contains(&modifier)
    }

    /// The Linux virtual-terminal switch conflict of the validation table:
    /// `Ctrl+Alt+F1..F12` **without** Shift changes VT, so
    /// `input.emergency_hotkey` must not be it.
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
        }
    }
}

impl std::error::Error for HotkeyError {}
