//! The document-level contract of `config`: version gate, unknown keys,
//! defaults, round trip, and the shipped example.

use config::{
    Backend, ClipboardTransfer, Config, ConfigError, Language, LogLevel, RepeatMode,
    TakeoverDetection, N_MAX, SUPPORTED_CONFIG_VERSION,
};

/// The loadable copy of the documented example.
const EXAMPLE: &str = include_str!("../examples-data/config.example.toml");

#[test]
fn shipped_example_parses_and_validates() {
    let config = Config::from_str(EXAMPLE).expect("the shipped example must parse");
    config
        .validate()
        .expect("the shipped example must satisfy every load rule");

    // A few values pinned by the documents, so a silent edit of the example is
    // caught here rather than in the field.
    assert_eq!(config.config_version, SUPPORTED_CONFIG_VERSION);
    assert_eq!(config.network.port_input, 47901);
    assert_eq!(config.network.port_clipboard, 47902);
    assert_eq!(config.input.hotkeys.emergency_release, "Ctrl+Alt+Shift+F12");
    assert_eq!(config.timing.prepare_timeout_ms, 200);
    assert_eq!(config.timing.lease_ttl_ms, 1000);
    assert!(
        !config.topology.links.is_empty(),
        "the example shows a link chain"
    );
}

#[test]
fn a_bare_version_line_fills_every_section_with_its_defaults() {
    let minimal = Config::from_str("config_version = 2").expect("defaults must fill the document");
    assert_eq!(minimal, Config::default());

    // Every documented default, read from the sections rather than from a
    // second copy of the struct literal.
    assert_eq!(minimal.node.display_name, "Workstation");
    assert!(minimal.node.can_own && minimal.node.claim_on_startup);
    assert_eq!(minimal.platform.backend, Backend::Auto);

    assert_eq!(minimal.network.listen_address, "");
    assert_eq!(minimal.network.port_input, 47901);
    assert_eq!(minimal.network.port_clipboard, 47902);
    assert_eq!(minimal.network.reconnect_backoff_ms, [500, 5000]);
    assert_eq!(minimal.network.max_members, 3);
    assert!(!minimal.network.allow_any_interface);
    assert!(minimal.network.peers.is_empty());
    assert!(minimal.discovery.enabled);
    assert!(minimal.discovery.advertise);

    assert_eq!(minimal.input.hotkeys.claim_ownership, "Ctrl+Alt+Shift+Home");
    assert_eq!(minimal.workspace.owner_claim_serial, 0);
    assert_eq!(minimal.workspace.owner_node_id, "");
    assert_eq!(minimal.workspace.link_ping_ms, 1000);
    assert_eq!(minimal.workspace.link_timeout_ms, 3000);
    assert_eq!(minimal.workspace.claim_grant_timeout_ms, 600);

    assert_eq!(minimal.timing.prepare_timeout_ms, 200);
    assert_eq!(minimal.timing.lease_ttl_ms, 1000);
    assert_eq!(minimal.timing.agent_core_silence_ms, 300);
    assert_eq!(minimal.timing.repeat_guard_ms, 150);
    // Derived, never configured.
    assert_eq!(minimal.timing.challenge_interval_ms(), 100);
    assert_eq!(minimal.timing.ipc_heartbeat_ms(), 75);

    assert_eq!(
        minimal.input.hotkeys.emergency_release,
        "Ctrl+Alt+Shift+F12"
    );
    assert_eq!(minimal.input.hotkeys.switch_next, "Ctrl+Alt+Shift+Right");
    assert_eq!(minimal.input.hotkeys.return_local, "Ctrl+Alt+Shift+Left");
    assert_eq!(minimal.input.pointer_gain, 1.0);
    assert_eq!(minimal.input.edge_dwell_ms, 0);
    assert_eq!(minimal.input.edge_hysteresis_px, 8);
    assert_eq!(minimal.input.takeover_threshold_px, 12);
    assert_eq!(minimal.input.repeat_mode, RepeatMode::ReceiverSoftware);
    assert!(minimal.input.require_emergency_hotkey);
    assert_eq!(minimal.input.takeover_detection, TakeoverDetection::Auto);

    assert!(!minimal.clipboard.enabled);
    assert!(minimal.clipboard.skip_secret_marked);
    assert_eq!(minimal.clipboard.max_bytes, 1_048_576);
    assert_eq!(minimal.clipboard.rate_bytes_per_s, 262_144);
    assert_eq!(minimal.clipboard.transfer, ClipboardTransfer::OnOffer);

    assert_eq!(minimal.ui.language, Language::Ru);
    assert!(!minimal.ui.autostart);

    assert_eq!(minimal.log.level, LogLevel::Info);
    assert_eq!(minimal.log.max_file_mb, 5);
    assert_eq!(minimal.log.max_files, 5);

    assert_eq!(minimal.topology.revision, 0);
    assert_eq!(minimal.topology.author, "");
    assert_eq!(minimal.topology.max_seen_revision, 0);
    assert_eq!(minimal.topology.max_seen_author, "");
    assert!(minimal.topology.links.is_empty());
    assert!(minimal.topology.displays.is_empty());
}

#[test]
fn discovery_defaults_and_opt_out_are_backward_compatible() {
    for table in ["", "[discovery]\n"] {
        let config = Config::from_str(&format!("config_version = 2\n{table}"))
            .expect("omitted discovery settings must default on");
        assert_eq!(config.discovery, config::DiscoveryConfig::default());
        assert!(config.discovery.enabled);
        assert!(config.discovery.advertise);
    }
    for (settings, enabled, advertise) in [
        ("enabled = false", false, true),
        ("advertise = false", true, false),
        ("enabled = false\nadvertise = false", false, false),
    ] {
        let config = Config::from_str(&format!("config_version = 2\n[discovery]\n{settings}"))
            .expect("discovery opt-out must parse");
        assert_eq!(config.discovery.enabled, enabled);
        assert_eq!(config.discovery.advertise, advertise);
        let text = config.to_toml().expect("discovery settings must serialize");
        assert_eq!(
            Config::from_str(&text).expect("round trip must parse"),
            config
        );
    }
}

#[test]
fn defaults_alone_are_not_a_runnable_configuration() {
    let error = Config::default()
        .validate()
        .expect_err("a configuration with no listen address cannot run");
    assert!(matches!(
        error,
        ConfigError::Invalid {
            code: Some(config::ErrorCode::Listen),
            key: "network.listen_address",
            ..
        }
    ));
    assert_eq!(error.code(), Some(config::ErrorCode::Listen));
}

#[test]
fn the_version_gate_runs_before_the_rest_of_the_document() {
    // A v1 file carries keys that do not exist in v2. The unsupported-version
    // answer must win over the unknown-key answer, otherwise the GUI would tell
    // the user about a field instead of about the version.
    let v1 = "config_version = 1\n[node]\nrole = \"source\"\n";
    assert!(matches!(
        Config::from_str(v1),
        Err(ConfigError::UnsupportedVersion { ref found }) if found == "1"
    ));

    assert!(matches!(
        Config::from_str("config_version = 3"),
        Err(ConfigError::UnsupportedVersion { ref found }) if found == "3"
    ));
    // A non-integer version is still a version problem, not a type problem.
    assert!(matches!(
        Config::from_str("config_version = \"2\""),
        Err(ConfigError::UnsupportedVersion { .. })
    ));
    assert!(matches!(
        Config::from_str("[node]\ndisplay_name = \"x\""),
        Err(ConfigError::MissingVersion)
    ));

    // The FromStr implementation is the same gate.
    let parsed: Result<Config, _> = "config_version = 2".parse();
    assert_eq!(
        parsed.expect("FromStr must accept the supported version"),
        Config::default()
    );
}

#[test]
fn unknown_keys_are_rejected_at_every_depth_and_name_the_table() {
    let top = Config::from_str("config_version = 2\nbogus = 1\n").expect_err("top-level unknown");
    match top {
        ConfigError::Parse { ref source } => {
            let text = source.to_string();
            assert!(
                text.contains("bogus"),
                "the message must name the key: {text}"
            );
        }
        other => panic!("expected a parse error, got {other}"),
    }

    let nested =
        Config::from_str("config_version = 2\n[input]\nbogus = 1\n").expect_err("nested unknown");
    match nested {
        ConfigError::Parse { ref source } => {
            let text = source.to_string();
            assert!(
                text.contains("bogus"),
                "the message must name the key: {text}"
            );
            assert!(
                text.contains("input") && text.contains("expected"),
                "the message must name the failing table and list the allowed fields: {text}"
            );
        }
        other => panic!("expected a parse error, got {other}"),
    }

    // A wrong type is a parse error too, not a validation error.
    assert!(matches!(
        Config::from_str("config_version = 2\n[network]\nmax_members = \"three\"\n"),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn a_partially_written_section_keeps_the_defaults_of_its_other_fields() {
    let config = Config::from_str("config_version = 2\n[timing]\nlease_ttl_ms = 500\n")
        .expect("a partial section is allowed");
    assert_eq!(config.timing.lease_ttl_ms, 500);
    assert_eq!(config.timing.prepare_timeout_ms, 200);
    assert_eq!(config.timing.agent_core_silence_ms, 300);
    assert_eq!(config.timing.repeat_guard_ms, 150);
    assert_eq!(config.timing.challenge_interval_ms(), 100);
}

#[test]
fn unknown_enum_values_list_the_accepted_literals() {
    let cases: [(&str, &[&str]); 6] = [
        (
            "config_version = 2\n[platform]\nbackend = \"foo\"\n",
            &["auto", "x11", "wayland"],
        ),
        (
            "config_version = 2\n[input]\nrepeat_mode = \"sometimes\"\n",
            &["receiver_native", "receiver_software"],
        ),
        (
            "config_version = 2\n[input]\ntakeover_detection = \"mouse\"\n",
            &["auto", "evdev", "off"],
        ),
        (
            "config_version = 2\n[clipboard]\ntransfer = \"always\"\n",
            &["on_offer", "on_focus_enter"],
        ),
        (
            "config_version = 2\n[ui]\nlanguage = \"de\"\n",
            &["ru", "en"],
        ),
        (
            "config_version = 2\n[log]\nlevel = \"verbose\"\n",
            &["trace", "debug", "info", "warn", "error"],
        ),
    ];

    for (document, expected_values) in cases {
        match Config::from_str(document) {
            Err(ConfigError::Parse { source }) => {
                let text = source.to_string();
                for value in expected_values {
                    assert!(
                        text.contains(value),
                        "the message must list `{value}`: {text}"
                    );
                }
            }
            other => panic!("expected a parse error for `{document}`, got {other:?}"),
        }
    }
}

#[test]
fn enum_literals_are_case_insensitive_and_fixed() {
    let config = Config::from_str(
        "config_version = 2\n[platform]\nbackend = \"X11\"\n[input]\ntakeover_detection = \"OFF\"\n",
    )
    .expect("enum parsing is case-insensitive");
    assert_eq!(config.platform.backend, Backend::X11);
    assert_eq!(config.platform.backend.as_str(), "x11");
    assert_eq!(config.input.takeover_detection, TakeoverDetection::Off);
    assert_eq!(TakeoverDetection::ALL.len(), 3);
}

#[test]
fn parse_to_toml_to_parse_is_stable_for_the_example_and_the_defaults() {
    for config in [
        Config::from_str(EXAMPLE).expect("example"),
        Config::default(),
    ] {
        let text = config.to_toml().expect("a config must serialize");
        let again = Config::from_str(&text).expect("its own output must parse");
        assert_eq!(again, config, "round trip changed the document");
    }
}

#[test]
fn shared_protocol_constants_are_not_duplicated_here() {
    // The loader reuses the wire numbers; this pins the reuse so a local copy
    // cannot drift from the protocol crate.
    assert_eq!(N_MAX, 4);
    assert_eq!(config::N_MAX, protocol::N_MAX);
    assert_eq!(SUPPORTED_CONFIG_VERSION, 2);
    assert_eq!(
        protocol::PORT_INPUT_CONTROL,
        Config::default().network.port_input
    );
    assert_eq!(
        protocol::PORT_CLIPBOARD,
        Config::default().network.port_clipboard
    );
}
