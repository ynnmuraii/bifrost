//! Path resolution (through explicitly injected roots), the atomic writer, and
//! the `[topology]` → `geometry` conversion.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use config::{
    node_id_from_hex, parse_node_ref, peer_reference, save_atomic, Config, ConfigError, ErrorCode,
    Paths,
};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A private directory under the system temp root. The tests never touch the
/// per-user configuration directories.
fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "lws-config-tests-{}-{}-{label}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the temporary directory must be creatable");
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("file must be readable")
}

#[test]
fn explicit_roots_drive_every_derived_path() {
    let root = temp_dir("paths");
    let config_dir = root.join("roaming").join("lan-workspace");
    let data_dir = config_dir.clone();
    let state_dir = root.join("local").join("lan-workspace");
    let runtime_dir = root.join("runtime").join("lan-workspace");

    let paths = Paths::new(
        &config_dir,
        &data_dir,
        &state_dir,
        Some(runtime_dir.clone()),
    );

    assert_eq!(paths.config_dir(), config_dir);
    assert_eq!(paths.data_dir(), data_dir);
    assert_eq!(paths.state_dir(), state_dir);
    assert_eq!(paths.runtime_dir(), Some(runtime_dir.as_path()));

    assert_eq!(paths.config_file(), config_dir.join("config.toml"));
    assert_eq!(
        paths.config_backup_file(),
        config_dir.join("config.toml.bak")
    );
    assert_eq!(paths.trust_file(), config_dir.join("trust.toml"));
    assert_eq!(
        paths.layout_pending_file(),
        config_dir.join("layout.pending.toml")
    );
    assert_eq!(paths.logs_dir(), state_dir.join("logs"));
    assert_eq!(
        paths.portal_restore_file(),
        state_dir.join("portal-restore.toml")
    );
    assert_eq!(paths.gui_ipc(), Some(runtime_dir.join("gui.sock")));

    if cfg!(windows) {
        // A DPAPI blob in the roaming directory; there is no Linux-style key file.
        assert_eq!(paths.identity_file(), config_dir.join("identity.dpapi"));
    } else {
        assert_eq!(paths.identity_file(), data_dir.join("identity.key"));
    }

    // Windows has no runtime directory: the GUI endpoint is a named pipe, so
    // `gui_ipc` is absent instead of guessed.
    let pipe_only = Paths::new(&config_dir, &data_dir, &state_dir, None);
    assert_eq!(pipe_only.gui_ipc(), None);
    assert!(pipe_only.to_string().contains("<none>"));

    let rendered = paths.to_string();
    assert!(rendered.contains(&config_dir.display().to_string()));
    assert!(rendered.contains(&state_dir.display().to_string()));
    assert!(rendered.contains(&runtime_dir.display().to_string()));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_paths_resolve_without_creating_anything() {
    // Resolving is pure: it reads environment variables and builds paths. The
    // assertions below deliberately use only path structure, so the test also
    // holds on a machine where the directories do not exist.
    let paths = config::default_paths().expect("APPDATA/HOME must be resolvable on this host");

    assert!(paths.config_dir().ends_with(config::paths::APP_DIR));
    assert!(paths.state_dir().ends_with(config::paths::APP_DIR));
    assert_eq!(paths.config_file().parent(), Some(paths.config_dir()));

    if cfg!(windows) {
        assert!(paths.runtime_dir().is_none());
        assert_eq!(paths.gui_ipc(), None);
        assert!(paths.identity_file().ends_with("identity.dpapi"));
    } else {
        assert!(paths.identity_file().ends_with("identity.key"));
    }
}

#[test]
fn atomic_save_never_leaves_a_partial_file_and_keeps_the_previous_version() {
    let root = temp_dir("save");
    let path = root.join("nested").join("config.toml");
    let temporary = root.join("nested").join("config.toml.tmp");
    let backup = root.join("nested").join("config.toml.bak");

    // The parent directory is created on demand.
    save_atomic(&path, "first").expect("first save");
    assert_eq!(read(&path), "first");
    assert!(!backup.exists(), "there was no previous version to keep");
    assert!(!temporary.exists(), "the temporary file must not survive");

    save_atomic(&path, "second").expect("second save");
    assert_eq!(read(&path), "second");
    assert_eq!(read(&backup), "first");
    assert!(!temporary.exists());

    save_atomic(&path, "third").expect("third save");
    assert_eq!(read(&path), "third");
    assert_eq!(read(&backup), "second");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_failing_save_reports_the_path_it_could_not_use() {
    let root = temp_dir("save-error");
    let blocker = root.join("blocker");
    std::fs::write(&blocker, "not a directory").expect("write the blocker file");

    let error = save_atomic(&blocker.join("config.toml"), "payload")
        .expect_err("a file cannot be used as a parent directory");
    match error {
        ConfigError::Io { path, .. } => {
            assert!(
                path.starts_with(&blocker),
                "reported path was {}",
                path.display()
            );
        }
        other => panic!("expected an io error, got {other}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn config_save_writes_the_serialized_text_and_load_reads_it_back() {
    let root = temp_dir("config-save");
    let path = root.join("config.toml");
    let config = Config::from_str(
        "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n[timing]\nlease_ttl_ms = 1000\n",
    )
    .expect("parse");

    config.save(&path).expect("save");
    assert_eq!(
        read(&path),
        config.to_toml().expect("serialize"),
        "saving must write exactly the serialized document"
    );

    let loaded = Config::load(&path).expect("load parses and validates");
    assert_eq!(loaded, config);

    // A saved document is a valid document: the round trip through the file
    // does not change it.
    loaded.save(&path).expect("second save");
    assert_eq!(Config::load(&path).expect("reload"), config);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_reports_missing_files_and_invalid_documents() {
    let root = temp_dir("load");
    let missing = root.join("absent.toml");
    match Config::load(&missing) {
        Err(ConfigError::Io { path, .. }) => assert_eq!(path, missing),
        other => panic!("expected an io error, got {other:?}"),
    }

    // `load` validates, so a document that parses but breaks a rule is refused.
    let invalid = root.join("invalid.toml");
    std::fs::write(&invalid, "config_version = 2\n").expect("write");
    match Config::load(&invalid) {
        Err(error) => assert_eq!(error.code(), Some(ErrorCode::Listen)),
        Ok(config) => panic!("a document with no listen address must not load: {config:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_recovers_valid_backup_without_accepting_invalid_backup() {
    let root = temp_dir("recovery");
    let path = root.join("config.toml");
    let backup = root.join("config.toml.bak");
    let text = "config_version = 2\n[node]\ndisplay_name = \"WORKSTATION\"\n[network]\nlisten_address = \"192.168.1.10\"\n";
    std::fs::write(&backup, text).unwrap();
    for damaged in ["not valid TOML [", "config_version = 2\n"] {
        std::fs::write(&path, damaged).unwrap();
        let recovered = Config::load(&path).expect("a valid backup recovers a damaged primary");
        assert_eq!(recovered.node.display_name, "WORKSTATION");
        assert_eq!(read(&backup), text, "recovery preserves the backup");
    }
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().node.display_name,
        "WORKSTATION"
    );
    std::fs::write(&path, "not valid TOML [").unwrap();
    std::fs::write(&backup, "config_version = 2\n").unwrap();
    assert!(matches!(
        Config::load(&path),
        Err(ConfigError::Parse { .. })
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn first_run_recovers_missing_primary_before_creating_defaults() {
    let root = temp_dir("first-run-recovery");
    let paths = Paths::new(&root, &root, &root, None);
    let backup = paths.config_backup_file();
    let text = "config_version = 2\n[node]\ndisplay_name = \"LAPTOP\"\n[network]\nlisten_address = \"192.168.1.20\"\n";
    std::fs::write(&backup, text).unwrap();
    assert!(!config::ensure_config(&paths).expect("recovery is not first-run creation"));
    assert_eq!(
        Config::load(paths.config_file()).unwrap().node.display_name,
        "LAPTOP"
    );
    assert_eq!(read(&backup), text);
    std::fs::remove_file(paths.config_file()).unwrap();
    std::fs::write(&backup, "broken [").unwrap();
    assert!(
        config::ensure_config(&paths).is_err(),
        "do not replace a damaged backup with defaults"
    );
    assert!(!paths.config_file().exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn node_id_hex_conversion_round_trips_and_rejects_anything_else() {
    let id = [0xab_u8; 32];
    let reference = peer_reference(&id);
    assert_eq!(reference, format!("peer:{}", "ab".repeat(32)));

    assert_eq!(node_id_from_hex(&"ab".repeat(32)), Some(id));
    assert_eq!(
        node_id_from_hex(&"AB".repeat(32)),
        Some(id),
        "hex is case-insensitive"
    );
    assert_eq!(
        node_id_from_hex(&"ab".repeat(31)),
        None,
        "63 digits is short"
    );
    assert_eq!(
        node_id_from_hex(&"ab".repeat(33)),
        None,
        "66 digits is long"
    );
    assert_eq!(node_id_from_hex(&"zz".repeat(32)), None, "not hexadecimal");
    assert_eq!(node_id_from_hex(""), None);
    assert_eq!(node_id_from_hex("peer:ab"), None);

    // A full round trip through the textual form the configuration uses.
    let hex = reference.strip_prefix("peer:").expect("prefix");
    assert_eq!(node_id_from_hex(hex), Some(id));
}

#[test]
fn node_references_resolve_self_and_peers_and_reject_everything_else() {
    let local = [0x11_u8; 32];
    let peer = [0x22_u8; 32];

    assert_eq!(
        parse_node_ref("self", &local).expect("self resolves"),
        geometry::NodeId::new(local)
    );
    assert_eq!(
        parse_node_ref(&peer_reference(&peer), &local).expect("peer resolves"),
        geometry::NodeId::new(peer)
    );
    // The hex digits are case-insensitive; the `peer:` prefix itself is a
    // literal of the grammar.
    let uppercase_hex = format!(
        "peer:{}",
        peer_reference(&peer)
            .strip_prefix("peer:")
            .expect("prefix")
            .to_uppercase()
    );
    assert_eq!(
        parse_node_ref(&uppercase_hex, &local).expect("peer resolves"),
        geometry::NodeId::new(peer),
        "the hex digits are case-insensitive"
    );

    for bad in [
        "",
        "SELF",
        "peer:",
        "peer:abcd",
        "peer:zz",
        "localhost",
        "self:",
    ] {
        match parse_node_ref(bad, &local) {
            Err(ConfigError::Invalid { code, key, .. }) => {
                assert_eq!(code, Some(ErrorCode::Topology), "for `{bad}`");
                assert_eq!(key, "topology.links.node", "for `{bad}`");
            }
            other => panic!("`{bad}` must be refused, got {other:?}"),
        }
    }
}

#[test]
fn topology_links_convert_to_the_geometry_contract() {
    let local = [0x11_u8; 32];
    let peer = [0x22_u8; 32];
    let document = format!(
        "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n\
         [topology]\n\
         [[topology.links]]\n\
         id = \"win-right-to-a-left\"\n\
         from = {{ node = \"self\", display = \"dp1\", edge = \"right\", start = 200.0, end = 1080.0 }}\n\
         to = {{ node = \"{}\", display = \"eDP-1\", edge = \"left\", start = 0.0, end = 720.0 }}\n\
         bidirectional = true\n",
        peer_reference(&peer)
    );
    let config = Config::from_str(&document).expect("parse");
    config.validate().expect("the document is valid");

    let links = config
        .topology
        .to_transition_links(&local)
        .expect("references resolve");
    let expected = geometry::TransitionLink::new(
        geometry::LinkId::new("win-right-to-a-left"),
        geometry::Endpoint::new(
            geometry::NodeId::new(local),
            geometry::DisplayId::new("dp1"),
            geometry::Edge::Right,
            geometry::Segment::new(200.0, 1080.0),
        ),
        geometry::Endpoint::new(
            geometry::NodeId::new(peer),
            geometry::DisplayId::new("eDP-1"),
            geometry::Edge::Left,
            geometry::Segment::new(0.0, 720.0),
        ),
        true,
    );
    assert_eq!(links, vec![expected]);
}

#[test]
fn topology_displays_are_a_gui_snapshot_and_convert_to_nothing() {
    // «снимок для GUI; источник истины — агент узла»: inventing rotation, scale
    // or native bounds from a snapshot would fabricate data.
    let document = "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n\
                    [topology]\n\
                    [[topology.displays]]\nnode = \"self\"\nid = \"dp1\"\nlogical = [0.0, 0.0, 1920.0, 1080.0]\nprimary = true\n";
    let config = Config::from_str(document).expect("parse");
    config.validate().expect("a display-only document is valid");
    assert_eq!(config.topology.displays.len(), 1);
    assert!(config
        .topology
        .to_transition_links(&[0x11; 32])
        .expect("no links to convert")
        .is_empty());
}

#[test]
fn an_unresolvable_node_reference_fails_at_conversion_time() {
    // `validate` only requires the text to be non-empty; identity is parsed at
    // conversion time, which is where the real error belongs.
    let document = "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n\
                    [topology]\n\
                    [[topology.links]]\nid = \"l1\"\n\
                    from = { node = \"bogus\", display = \"d1\", edge = \"right\", start = 0.0, end = 10.0 }\n\
                    to = { node = \"self\", display = \"d2\", edge = \"left\", start = 0.0, end = 10.0 }\n";
    let config = Config::from_str(document).expect("parse");
    config
        .validate()
        .expect("the loader does not police the reference format");

    let error = config
        .topology
        .to_transition_links(&[0x11; 32])
        .expect_err("`bogus` is not a node reference");
    assert_eq!(error.code(), Some(ErrorCode::Topology));
    assert!(error.to_string().contains("topology.links.from"), "{error}");
}

#[test]
fn loading_clamps_clipboard_settle_without_rejecting_config() {
    let root = temp_dir("clipboard-settle");
    let path = root.join("config.toml");
    for (configured, expected) in [(0, 200), (700, 700), (5000, 3000)] {
        let document = format!(
            "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n\
             [clipboard]\npolicy = \"broadcast\"\nsettle_ms = {configured}\n"
        );
        std::fs::write(&path, document).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.clipboard.policy,
            config::ClipboardRoutingPolicy::Broadcast
        );
        assert_eq!(config.clipboard.settle_ms, expected);
    }
    std::fs::remove_dir_all(root).unwrap();
}
