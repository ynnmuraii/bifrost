//! The `config_version = 2` hotkey keys that predate `[input.hotkeys]`.
//!
//! `input.emergency_hotkey`, `input.switch_hotkey` and `workspace.claim_hotkey`
//! were the three hotkey keys of every shipped build up to `4bb1310`. Commit
//! `6a83ce6` replaced all three with the `[input.hotkeys]` table without an
//! alias, a migration or a version bump, so a `config.toml` written by an
//! earlier build stopped loading — `unknown field `emergency_hotkey`` — and the
//! core refused to start.
//!
//! This module is the compatibility half of the fix. [`strip`] removes the three
//! keys from a parsed document and folds each one into `[input.hotkeys]`, where
//! an explicit table entry wins over the legacy chord (a WARN is logged once per
//! parse, from [`crate::Config::from_str`]). Every other unknown key of `[input]`
//! and `[workspace]` still fails deserialization, so the schema keeps its
//! `deny_unknown_fields` semantics for everything but these three.
//!
//! The mapped chords are plain `[input.hotkeys]` values afterwards, so
//! [`crate::Config::validate`] checks them with the same parser and the same
//! rules as a chord written in the table by hand.

use crate::error::{ConfigError, ErrorCode};
use crate::hotkey::HotkeyAction;

/// The legacy keys: `(section, key, dotted path, action)`.
///
/// The dotted path is carried separately because [`ConfigError::Invalid`] names a
/// `&'static str` key and the sections are not necessarily constant-folded away.
const LEGACY_KEYS: [(&str, &str, &str, HotkeyAction); 3] = [
    (
        "input",
        "emergency_hotkey",
        "input.emergency_hotkey",
        HotkeyAction::EmergencyRelease,
    ),
    (
        "input",
        "switch_hotkey",
        "input.switch_hotkey",
        HotkeyAction::SwitchNext,
    ),
    (
        "workspace",
        "claim_hotkey",
        "workspace.claim_hotkey",
        HotkeyAction::ClaimOwnership,
    ),
];

/// True when `document` still sets one of the legacy hotkey keys.
pub(crate) fn present(document: &toml::Value) -> bool {
    LEGACY_KEYS.into_iter().any(|(section, key, _, _)| {
        document
            .get(section)
            .and_then(toml::Value::as_table)
            .is_some_and(|table| table.contains_key(key))
    })
}

/// Removes every legacy hotkey key from `document` and folds it into
/// `[input.hotkeys]` unless the table already sets that action.
///
/// Returns the actions both the legacy key and `[input.hotkeys]` set — the table
/// won, and the caller logs one warning about them. Errors when a legacy key
/// holds something other than a string, naming the key the person wrote rather
/// than the field it now feeds.
pub(crate) fn strip(document: &mut toml::Value) -> Result<Vec<HotkeyAction>, ConfigError> {
    let mut found: Vec<(HotkeyAction, String)> = Vec::new();
    for (section, key, dotted, action) in LEGACY_KEYS {
        let Some(value) = document
            .get_mut(section)
            .and_then(toml::Value::as_table_mut)
            .and_then(|table| table.remove(key))
        else {
            continue;
        };
        let Some(text) = value.as_str() else {
            return Err(ConfigError::Invalid {
                code: Some(ErrorCode::Hotkey),
                key: dotted,
                value: value.to_string(),
                expected: "a hotkey of the form Ctrl+Alt+Shift+F12".to_owned(),
            });
        };
        found.push((action, text.to_owned()));
    }

    let mut overridden = Vec::new();
    for (action, text) in found {
        if sets(document, action) {
            overridden.push(action);
            continue;
        }
        if let Some(hotkeys) = bindings_table(document) {
            hotkeys.insert(action.as_str().to_owned(), toml::Value::String(text));
        }
    }
    Ok(overridden)
}

/// True when `[input.hotkeys]` sets `action` explicitly.
///
/// A table that merely *defaults* the action does not count: only a key the
/// person actually wrote wins over the legacy chord.
fn sets(document: &toml::Value, action: HotkeyAction) -> bool {
    document
        .get("input")
        .and_then(toml::Value::as_table)
        .and_then(|input| input.get("hotkeys"))
        .and_then(toml::Value::as_table)
        .is_some_and(|hotkeys| hotkeys.contains_key(action.as_str()))
}

/// The `[input.hotkeys]` table of `document`, created when missing.
///
/// `None` when `input` or `hotkeys` exists as something other than a table:
/// deserialization then reports that, which is the error the file deserves.
fn bindings_table(document: &mut toml::Value) -> Option<&mut toml::Table> {
    let root = document.as_table_mut()?;
    if !root.contains_key("input") {
        root.insert("input".to_owned(), toml::Value::Table(toml::Table::new()));
    }
    let input = root.get_mut("input")?.as_table_mut()?;
    if !input.contains_key("hotkeys") {
        input.insert("hotkeys".to_owned(), toml::Value::Table(toml::Table::new()));
    }
    input.get_mut("hotkeys")?.as_table_mut()
}
