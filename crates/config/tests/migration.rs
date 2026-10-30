//! The upgrade path off the pre-`[input.hotkeys]` hotkey keys.
//!
//! Every build up to `4bb1310` wrote `input.emergency_hotkey`,
//! `input.switch_hotkey` and `workspace.claim_hotkey`; `6a83ce6` replaced them
//! with `[input.hotkeys]` without an alias, so such a file stopped loading. These
//! tests hold the two halves of the fix: the loader maps the legacy keys onto the
//! bindings (accepting every example that ever shipped), and the core's migration
//! rewrites the file once, keeping every other value.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use config::{Config, SUPPORTED_CONFIG_VERSION};

/// The example exactly as it shipped *before* `6a83ce6` moved the three hotkey
/// keys into `[input.hotkeys]` — the shape every existing installation carries.
/// A future schema change must keep this loading, so it is a fixture rather than
/// a copy of the current example.
const LEGACY_EXAMPLE: &str = include_str!("data/config.example.pre-hotkeys.toml");

/// The example of the current schema.
const EXAMPLE: &str = include_str!("../examples-data/config.example.toml");

/// A document that satisfies the rules unrelated to this file's subject, so a
/// case can be a section and nothing else.
const VALID: &str = "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n";

/// The owner's `[input]` section: the two legacy keys plus the fields that stayed.
const LEGACY_INPUT: &str = r#"
[input]
emergency_hotkey = "Ctrl+Alt+Shift+F12"
switch_hotkey = "Ctrl+Alt+Shift+Right"
pointer_gain = 1.0
edge_dwell_ms = 0
edge_hysteresis_px = 8
takeover_threshold_px = 12
repeat_mode = "receiver_software"
require_emergency_hotkey = true
takeover_detection = "auto"
"#;

fn parse(text: &str) -> Config {
    Config::from_str(text).expect("the document must parse")
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bifrost-config-migration-{}-{}-{label}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::write(path, text).expect("write the configuration");
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read the configuration")
}

/// True when some line of `text` assigns `name` — as a key, not as the tail of a
/// longer one (`require_emergency_hotkey` is not `emergency_hotkey`).
fn sets_key(text: &str, name: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with(name) && line[name.len()..].trim_start().starts_with('=')
    })
}

// ---------------------------------------------------------------------------
// The loader: a shipped document still loads, and the keys keep their chords.
// ---------------------------------------------------------------------------

/// The regression net: *every* `config.example.toml` that ever shipped must keep
/// loading and validating, so a schema edit cannot silently break upgrades again.
#[test]
fn every_shipped_example_still_loads() {
    for (label, text) in [
        ("pre-hotkeys (pre-6a83ce6)", LEGACY_EXAMPLE),
        ("current", EXAMPLE),
    ] {
        let config = Config::from_str(text)
            .unwrap_or_else(|error| panic!("{label}: the example must parse: {error}"));
        config
            .validate()
            .unwrap_or_else(|error| panic!("{label}: the example must validate: {error}"));
        assert_eq!(config.config_version, SUPPORTED_CONFIG_VERSION);
    }
}

#[test]
fn the_legacy_example_keeps_every_chord() {
    let config = parse(LEGACY_EXAMPLE);
    config.validate().expect("the mapped chords are valid");
    assert_eq!(config.input.hotkeys.emergency_release, "Ctrl+Alt+Shift+F12");
    assert_eq!(config.input.hotkeys.switch_next, "Ctrl+Alt+Shift+Right");
    // `workspace.claim_hotkey` had its own default; the old chord survives.
    assert_eq!(config.input.hotkeys.claim_ownership, "Ctrl+Alt+Shift+F11");
    // The one action the old document could not name falls back to the default.
    assert_eq!(config.input.hotkeys.return_local, "Ctrl+Alt+Shift+Left");
}

#[test]
fn the_legacy_input_section_maps_to_the_bindings() {
    let config = parse(&format!("{VALID}{LEGACY_INPUT}"));
    config.validate().expect("the mapped chords are valid");
    assert_eq!(config.input.hotkeys.emergency_release, "Ctrl+Alt+Shift+F12");
    assert_eq!(config.input.hotkeys.switch_next, "Ctrl+Alt+Shift+Right");
    assert_eq!(config.input.pointer_gain, 1.0);
    assert_eq!(config.input.takeover_threshold_px, 12);
}

#[test]
fn a_legacy_key_of_the_wrong_type_names_the_key_it_replaced() {
    let error = Config::from_str(&format!("{VALID}[input]\nemergency_hotkey = 12\n"))
        .expect_err("a non-string chord is not a chord");
    assert!(
        error.to_string().contains("input.emergency_hotkey"),
        "the error must name the key the file carries: {error}"
    );
}

// ---------------------------------------------------------------------------
// Precedence: `[input.hotkeys]` wins, and only over the keys it actually sets.
// ---------------------------------------------------------------------------

#[test]
fn the_new_table_wins_over_a_legacy_key() {
    let config = parse(&format!(
        "{VALID}{LEGACY_INPUT}[input.hotkeys]\nemergency_release = \"Ctrl+Alt+Shift+F9\"\n"
    ));
    config.validate().expect("the surviving chords are valid");
    assert_eq!(config.input.hotkeys.emergency_release, "Ctrl+Alt+Shift+F9");
    // The actions the table does not set still come from the legacy keys.
    assert_eq!(config.input.hotkeys.switch_next, "Ctrl+Alt+Shift+Right");
}

#[test]
fn a_defaulted_table_entry_does_not_win_over_a_legacy_key() {
    // The table is present but silent about `switch_next`, so the legacy chord
    // is the one the person actually wrote and must survive.
    let config = parse(&format!(
        "{VALID}{LEGACY_INPUT}[input.hotkeys]\nreturn_local = \"Ctrl+Alt+Shift+F8\"\n"
    ));
    config.validate().expect("the surviving chords are valid");
    assert_eq!(config.input.hotkeys.switch_next, "Ctrl+Alt+Shift+Right");
    assert_eq!(config.input.hotkeys.return_local, "Ctrl+Alt+Shift+F8");
}

#[test]
fn the_claim_hotkey_precedence_holds_too() {
    let config = parse(&format!(
        "{VALID}[workspace]\nclaim_hotkey = \"Ctrl+Alt+Shift+F11\"\n\
         [input.hotkeys]\nclaim_ownership = \"Ctrl+Alt+Shift+F10\"\n"
    ));
    config.validate().expect("the surviving chord is valid");
    assert_eq!(config.input.hotkeys.claim_ownership, "Ctrl+Alt+Shift+F10");
}

// ---------------------------------------------------------------------------
// `deny_unknown_fields` for everything else.
// ---------------------------------------------------------------------------

#[test]
fn any_other_unknown_input_field_is_still_rejected() {
    let error = Config::from_str(&format!("{VALID}{LEGACY_INPUT}whatever = 1\n"))
        .expect_err("an unknown field is still an unknown field");
    assert!(
        error.to_string().contains("whatever"),
        "the error must name the unknown key: {error}"
    );
}

#[test]
fn any_other_unknown_workspace_field_is_still_rejected() {
    let error = Config::from_str(&format!(
        "{VALID}[workspace]\nclaim_hotkey = \"Ctrl+Alt+Shift+F11\"\nwhatever = 1\n"
    ))
    .expect_err("an unknown field is still an unknown field");
    assert!(
        error.to_string().contains("whatever"),
        "the error must name the unknown key: {error}"
    );
}

// ---------------------------------------------------------------------------
// The core's on-disk migration.
// ---------------------------------------------------------------------------

#[test]
fn the_core_migration_rewrites_the_file_exactly_once() {
    let dir = temp_dir("rewrite");
    let path = dir.join("config.toml");
    write(&path, LEGACY_EXAMPLE);

    let before = Config::load(&path).expect("the legacy file loads through the map");

    assert!(
        config::migrate_legacy_hotkeys(&path).expect("the first migration writes"),
        "a legacy file must be migrated"
    );

    let migrated = read(&path);
    for key in ["emergency_hotkey", "switch_hotkey", "claim_hotkey"] {
        assert!(
            !sets_key(&migrated, key),
            "`{key}` must be gone from the rewritten file:\n{migrated}"
        );
    }
    for key in [
        "emergency_release",
        "switch_next",
        "return_local",
        "claim_ownership",
    ] {
        assert!(
            sets_key(&migrated, key),
            "[input.hotkeys] must set `{key}`:\n{migrated}"
        );
    }

    // One write: `save_atomic` keeps the version it replaced, and that version is
    // the untouched legacy document.
    assert_eq!(read(&dir.join("config.toml.bak")), LEGACY_EXAMPLE);

    // The untouched original is also copied next to the file, before the rewrite.
    let preserved = dir.join("config.toml.pre-hotkeys");
    assert_eq!(read(&preserved), LEGACY_EXAMPLE);

    // Everything but the hotkey spelling is untouched — the strongest form of
    // "keeps the rest of the file" the typed writer can give.
    let after = Config::load(&path).expect("the migrated file loads");
    assert_eq!(after, before);

    // The core's very next persistence write — the ownership serial, a moment
    // after the migration — replaces `.bak` with the migrated document, so only
    // the copy still holds the original. That is the loss the copy exists to
    // prevent.
    config::update_locked(&path, |config| {
        config.workspace.owner_claim_serial = 7;
        config.workspace.owner_node_id = config::peer_reference(&[0x11; 32]);
    })
    .expect("the ownership write");
    assert_ne!(
        read(&dir.join("config.toml.bak")),
        LEGACY_EXAMPLE,
        "the ownership write replaces the .bak that held the original"
    );
    assert_eq!(
        read(&preserved),
        LEGACY_EXAMPLE,
        "the copy must survive every later write, byte for byte"
    );

    // And the second start neither rewrites the file nor touches the copy.
    let backup = read(&dir.join("config.toml.bak"));
    assert!(!config::migrate_legacy_hotkeys(&path).expect("the second check"));
    assert_eq!(
        read(&dir.join("config.toml.bak")),
        backup,
        "the second check must not have written anything"
    );
    assert_eq!(
        read(&preserved),
        LEGACY_EXAMPLE,
        "the second check must not have touched the copy"
    );
}

#[test]
fn a_current_file_is_never_rewritten() {
    let dir = temp_dir("current");
    let path = dir.join("config.toml");
    write(&path, EXAMPLE);

    assert!(!config::migrate_legacy_hotkeys(&path).expect("nothing to migrate"));
    assert_eq!(read(&path), EXAMPLE, "the file must be byte-identical");
    assert!(
        !dir.join("config.toml.bak").exists(),
        "no write means no .bak"
    );
    assert!(
        !dir.join("config.toml.pre-hotkeys").exists(),
        "no migration means no copy either"
    );
}

#[test]
fn an_existing_pre_hotkeys_copy_is_never_overwritten() {
    // `create_new`, not a copy-aside-and-replace: a copy an earlier run left
    // behind *is* the original, and a second attempt must not clobber it.
    let dir = temp_dir("preserved");
    let path = dir.join("config.toml");
    write(&path, LEGACY_EXAMPLE);
    let preserved = dir.join("config.toml.pre-hotkeys");
    let earlier = "# the copy an earlier migration left behind\n";
    write(&preserved, earlier);

    assert!(config::migrate_legacy_hotkeys(&path).expect("the migration writes"));

    assert_eq!(
        read(&preserved),
        earlier,
        "the existing copy must not be replaced"
    );
    assert!(
        !sets_key(&read(&path), "emergency_hotkey"),
        "the file is migrated regardless"
    );
}

#[test]
fn the_migration_survives_a_concurrent_ownership_write() {
    // The migration takes the same lock as the ownership store, so a value the
    // store writes between the two reads is what ends up on disk.
    let dir = temp_dir("concurrent");
    let path = dir.join("config.toml");
    write(&path, LEGACY_EXAMPLE);

    assert!(config::migrate_legacy_hotkeys(&path).expect("the migration writes"));
    let migrated = Config::load(&path).expect("the migrated file loads");
    assert_eq!(
        migrated.input.hotkeys.emergency_release,
        "Ctrl+Alt+Shift+F12"
    );

    // A later store write keeps the migrated bindings: nothing regressed them.
    config::update_locked(&path, |config| {
        config.workspace.owner_claim_serial = 7;
        config.workspace.owner_node_id = config::peer_reference(&[0x11; 32]);
    })
    .expect("the ownership write");
    let after = Config::load(&path).expect("the ownership file loads");
    assert_eq!(after.input.hotkeys, migrated.input.hotkeys);
}
