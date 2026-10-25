//! Every load-validation rule, at its boundaries, plus the error contract the
//! GUI reads (`CONFIG_*` codes, key, value, expected).

use config::{Config, ConfigError, ErrorCode};

/// A document that is valid as written; cases below append one section to it.
const VALID: &str = "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n";

fn parse(text: &str) -> Config {
    Config::from_str(text).expect("the case must be syntactically valid")
}

fn reject(extra: &str) -> ConfigError {
    parse(&format!("{VALID}{extra}"))
        .validate()
        .expect_err("the case must be rejected")
}

fn accept(extra: &str) {
    parse(&format!("{VALID}{extra}"))
        .validate()
        .expect("the case must be accepted");
}

fn code_of(error: &ConfigError) -> Option<ErrorCode> {
    error.code()
}

fn key_of(error: &ConfigError) -> &'static str {
    match error {
        ConfigError::Invalid { key, .. } => key,
        other => panic!("expected a rule violation, got {other}"),
    }
}

#[test]
fn listen_address_rules() {
    accept("");

    // Concrete addresses, IPv4 and IPv6.
    let config = parse("config_version = 2\n[network]\nlisten_address = \"fd00::1\"\n");
    config
        .validate()
        .expect("a concrete IPv6 address is allowed");

    for (address, allowed_without_flag) in [
        ("", false),
        ("nonsense", false),
        ("0.0.0.0", false),
        ("::", false),
        ("192.168.1.10", true),
    ] {
        let document = format!("config_version = 2\n[network]\nlisten_address = \"{address}\"\n");
        let result = parse(&document).validate();
        if allowed_without_flag {
            result.expect("a concrete address needs no flag");
        } else {
            let error = result.expect_err("this address cannot bind without a flag");
            assert_eq!(code_of(&error), Some(ErrorCode::Listen));
            assert_eq!(key_of(&error), "network.listen_address");
        }
    }

    // The documented escape hatch for binding everything.
    let any =
        "config_version = 2\n[network]\nlisten_address = \"0.0.0.0\"\nallow_any_interface = true\n";
    parse(any)
        .validate()
        .expect("allow_any_interface = true makes 0.0.0.0 expressible");
}

#[test]
fn port_and_membership_boundaries() {
    let document = |extra: &str| {
        format!("config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n{extra}")
    };

    for (extra, code, key) in [
        ("port_input = 0\n", None, "network.port_input"),
        ("port_clipboard = 0\n", None, "network.port_clipboard"),
        (
            "max_members = 0\n",
            Some(ErrorCode::Members),
            "network.max_members",
        ),
        (
            "max_members = 5\n",
            Some(ErrorCode::Members),
            "network.max_members",
        ),
    ] {
        let error = parse(&document(extra))
            .validate()
            .expect_err("value must be rejected");
        assert_eq!(error.code(), code, "for `{extra}`");
        assert_eq!(key_of(&error), key, "for `{extra}`");
    }

    for accepted in [
        "port_input = 1\n",
        "port_input = 65535\n",
        "port_clipboard = 65535\n",
        "max_members = 1\n",
        "max_members = 4\n",
    ] {
        parse(&document(accepted))
            .validate()
            .expect("value must be accepted");
    }
}

#[test]
fn reconnect_backoff_is_ordered_and_inside_half_a_second_to_five_seconds() {
    for (value, allowed) in [
        ("[500, 5000]", true),
        ("[500, 500]", true),
        ("[1000, 5000]", true),
        ("[499, 5000]", false),
        ("[500, 5001]", false),
        ("[1000, 999]", false),
    ] {
        let document =
            format!("config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\nreconnect_backoff_ms = {value}\n");
        let result = parse(&document).validate();
        if allowed {
            result.expect("backoff must be accepted");
        } else {
            let error = result.expect_err("backoff must be rejected");
            assert_eq!(
                code_of(&error),
                None,
                "an inferred sanity rule has no CONFIG_* code"
            );
            assert_eq!(key_of(&error), "network.reconnect_backoff_ms");
        }
    }
}

#[test]
fn timing_bounds_and_the_fixed_repeat_guard() {
    for (extra, allowed, key) in [
        (
            "prepare_timeout_ms = 150\n",
            true,
            "timing.prepare_timeout_ms",
        ),
        (
            "prepare_timeout_ms = 400\n",
            true,
            "timing.prepare_timeout_ms",
        ),
        (
            "prepare_timeout_ms = 149\n",
            false,
            "timing.prepare_timeout_ms",
        ),
        (
            "prepare_timeout_ms = 401\n",
            false,
            "timing.prepare_timeout_ms",
        ),
        ("lease_ttl_ms = 300\n", true, "timing.lease_ttl_ms"),
        ("lease_ttl_ms = 1000\n", true, "timing.lease_ttl_ms"),
        ("lease_ttl_ms = 299\n", false, "timing.lease_ttl_ms"),
        ("lease_ttl_ms = 1001\n", false, "timing.lease_ttl_ms"),
        (
            "agent_core_silence_ms = 200\n",
            true,
            "timing.agent_core_silence_ms",
        ),
        (
            "agent_core_silence_ms = 500\n",
            true,
            "timing.agent_core_silence_ms",
        ),
        (
            "agent_core_silence_ms = 199\n",
            false,
            "timing.agent_core_silence_ms",
        ),
        (
            "agent_core_silence_ms = 501\n",
            false,
            "timing.agent_core_silence_ms",
        ),
        ("repeat_guard_ms = 150\n", true, "timing.repeat_guard_ms"),
        ("repeat_guard_ms = 149\n", false, "timing.repeat_guard_ms"),
        ("repeat_guard_ms = 151\n", false, "timing.repeat_guard_ms"),
    ] {
        let document = format!("{VALID}[timing]\n{extra}");
        let result = parse(&document).validate();
        if allowed {
            result.expect("value must be accepted");
        } else {
            let error = result.expect_err("value must be rejected");
            assert_eq!(code_of(&error), Some(ErrorCode::Timing), "for `{extra}`");
            assert_eq!(key_of(&error), key, "for `{extra}`");
        }
    }
}

#[test]
fn derived_timing_periods_follow_their_source_values() {
    let config = parse(&format!(
        "{VALID}[timing]\nlease_ttl_ms = 1000\nagent_core_silence_ms = 500\n"
    ));
    assert_eq!(config.timing.challenge_interval_ms(), 100);
    assert_eq!(config.timing.ipc_heartbeat_ms(), 125);

    let fastest = parse(&format!(
        "{VALID}[timing]\nlease_ttl_ms = 300\nagent_core_silence_ms = 200\n"
    ));
    assert_eq!(fastest.timing.challenge_interval_ms(), 60);
    assert_eq!(fastest.timing.ipc_heartbeat_ms(), 50);
}

#[test]
fn the_derived_periods_are_not_configurable() {
    // The capped challenge period is computed, never configured.
    let document = format!("{VALID}[timing]\nchallenge_interval_ms = 100\n");
    assert!(matches!(
        Config::from_str(&document),
        Err(ConfigError::Parse { .. })
    ));

    let document = format!("{VALID}[timing]\nipc_heartbeat_ms = 75\n");
    assert!(matches!(
        Config::from_str(&document),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn workspace_ownership_pair_must_be_consistent() {
    for (extra, allowed) in [
        ("owner_claim_serial = 0\nowner_node_id = \"\"\n", true),
        ("owner_claim_serial = 1\nowner_node_id = \"9c1e\"\n", true),
        ("owner_claim_serial = 1\nowner_node_id = \"\"\n", false),
        ("owner_claim_serial = 0\nowner_node_id = \"9c1e\"\n", false),
    ] {
        let document = format!("{VALID}[workspace]\n{extra}");
        let result = parse(&document).validate();
        if allowed {
            result.expect("a consistent pair is accepted");
        } else {
            let error = result.expect_err("an inconsistent pair is rejected");
            assert_eq!(code_of(&error), Some(ErrorCode::Owner), "for `{extra}`");
        }
    }
}

#[test]
fn workspace_link_and_claim_timing_bounds() {
    for (extra, allowed) in [
        ("link_ping_ms = 1000\nlink_timeout_ms = 3000\n", true),
        ("link_ping_ms = 2999\nlink_timeout_ms = 3000\n", true),
        ("link_ping_ms = 3000\nlink_timeout_ms = 3000\n", false),
        ("link_ping_ms = 0\nlink_timeout_ms = 3000\n", false),
        ("link_ping_ms = 1000\nlink_timeout_ms = 0\n", false),
        ("link_ping_ms = 5000\nlink_timeout_ms = 1000\n", false),
    ] {
        let document = format!("{VALID}[workspace]\n{extra}");
        let result = parse(&document).validate();
        if allowed {
            result.expect("value must be accepted");
        } else {
            let error = result.expect_err("value must be rejected");
            assert_eq!(
                code_of(&error),
                None,
                "this ordering rule has no CONFIG_* code"
            );
        }
    }

    accept("[workspace]\nclaim_grant_timeout_ms = 1\n");
    let error = reject("[workspace]\nclaim_grant_timeout_ms = 0\n");
    assert_eq!(code_of(&error), None);
    assert_eq!(key_of(&error), "workspace.claim_grant_timeout_ms");
}

#[test]
fn hotkey_bindings_validate_supported_distinct_chords() {
    accept("[input.hotkeys]\nclaim_ownership = \"\"\n");
    accept("[input.hotkeys]\nclaim_ownership = \"Ctrl+F11\"\n");
    accept("[input.hotkeys]\nemergency_release = \"F24\"\n");
    accept("[input.hotkeys]\nswitch_next = \"Ctrl+Right\"\n");
    for (field, chord) in [
        ("emergency_release", ""),
        ("emergency_release", "Right"),
        ("switch_next", "Ctrl+Unknown"),
        ("return_local", "shift+alt+control+right"),
        ("claim_ownership", "Ctrl++F11"),
    ] {
        let error = reject(&format!("[input.hotkeys]\n{field} = \"{chord}\"\n"));
        assert_eq!(code_of(&error), Some(ErrorCode::Hotkey));
        assert_eq!(key_of(&error), format!("input.hotkeys.{field}"));
    }
}

#[test]
fn legacy_hotkey_fields_are_not_silently_accepted() {
    for field in [
        "[input]\nemergency_hotkey = \"Ctrl+F12\"\n",
        "[input]\nswitch_hotkey = \"Ctrl+Right\"\n",
        "[workspace]\nclaim_hotkey = \"Ctrl+Home\"\n",
    ] {
        assert!(toml::from_str::<config::Config>(&format!("{VALID}{field}")).is_err());
    }
}

#[test]
fn pointer_gain_must_be_positive_and_finite() {
    accept("[input]\npointer_gain = 2.5\n");
    for bad in ["0.0", "-1.0", "nan", "inf"] {
        let document = format!("{VALID}[input]\npointer_gain = {bad}\n");
        let result = parse(&document).validate();
        let error = result.expect_err("a non-positive or non-finite gain is refused");
        assert_eq!(code_of(&error), None, "this rule has no CONFIG_* code");
        assert_eq!(key_of(&error), "input.pointer_gain");
    }
}

#[test]
fn clipboard_and_log_bounds() {
    accept("[clipboard]\nmax_bytes = 1048576\n");
    accept("[clipboard]\nmax_bytes = 0\n");

    let error = reject("[clipboard]\nmax_bytes = 1048577\n");
    assert_eq!(code_of(&error), Some(ErrorCode::Clipboard));
    assert_eq!(key_of(&error), "clipboard.max_bytes");

    let error = reject("[clipboard]\nrate_bytes_per_s = 0\n");
    assert_eq!(code_of(&error), None);
    assert_eq!(key_of(&error), "clipboard.rate_bytes_per_s");

    let error = reject("[log]\nmax_file_mb = 0\n");
    assert_eq!(code_of(&error), None);
    assert_eq!(key_of(&error), "log.max_file_mb");

    let error = reject("[log]\nmax_files = 0\n");
    assert_eq!(code_of(&error), None);
    assert_eq!(key_of(&error), "log.max_files");
}

#[test]
fn a_rule_violation_renders_code_key_value_and_expectation() {
    let error = reject("[timing]\nprepare_timeout_ms = 10\n");
    assert_eq!(code_of(&error), Some(ErrorCode::Timing));
    assert_eq!(ErrorCode::Timing.as_str(), "CONFIG_TIMING");

    let text = error.to_string();
    assert!(text.contains("CONFIG_TIMING"), "{text}");
    assert!(text.contains("timing.prepare_timeout_ms"), "{text}");
    assert!(text.contains("10"), "{text}");
    assert!(text.contains("expected"), "{text}");

    // Errors that are not rule violations carry no code.
    assert_eq!(code_of(&ConfigError::MissingVersion), None);
}

#[test]
fn environment_owned_codes_exist_but_are_never_raised_by_this_loader() {
    // The table declares these two, but only the platform layer can evaluate
    // them (session kind, `input` group membership). A loader that started
    // raising them would be inventing an environment it cannot see.
    assert_eq!(ErrorCode::Backend.as_str(), "CONFIG_BACKEND");
    assert_eq!(ErrorCode::Takeover.as_str(), "CONFIG_TAKEOVER");

    let config = parse(
        "config_version = 2\n[network]\nlisten_address = \"192.168.1.10\"\n\
         [platform]\nbackend = \"x11\"\n[input]\ntakeover_detection = \"evdev\"\n",
    );
    config
        .validate()
        .expect("neither the session check nor the group check belongs to the loader");
}

#[test]
fn topology_document_invariants() {
    // revision > 0 needs its author, the floor never trails, and a non-zero
    // floor needs the author applied with it.
    let error = reject("[topology]\nrevision = 1\n");
    assert_eq!(code_of(&error), Some(ErrorCode::Topology));
    assert_eq!(error.code(), Some(ErrorCode::Topology));
    assert_eq!(ErrorCode::Topology.as_str(), "INVALID_TOPOLOGY");
    assert_eq!(key_of(&error), "topology.author");

    let error = reject("[topology]\nrevision = 2\nauthor = \"aa\"\nmax_seen_revision = 1\n");
    assert_eq!(key_of(&error), "topology.max_seen_revision");

    let error = reject("[topology]\nmax_seen_revision = 1\n");
    assert_eq!(key_of(&error), "topology.max_seen_author");

    accept("[topology]\nrevision = 2\nauthor = \"aa\"\nmax_seen_revision = 2\nmax_seen_author = \"aa\"\n");
}

#[test]
fn topology_link_integrity_rules() {
    let peer = config::peer_reference(&[0x22; 32]);

    let link = |id: &str, from_node: &str, to_node: &str, display: &str, start: &str, end: &str| {
        format!(
            "[[topology.links]]\nid = \"{id}\"\n\
             from = {{ node = \"{from_node}\", display = \"{display}\", edge = \"right\", start = {start}, end = {end} }}\n\
             to = {{ node = \"{to_node}\", display = \"d2\", edge = \"left\", start = 0.0, end = 10.0 }}\n"
        )
    };

    // A well-formed link is accepted.
    accept(&format!(
        "[topology]\n{}",
        link("l1", "self", &peer, "d1", "0.0", "10.0")
    ));

    for (extra, key) in [
        (
            link("", "self", &peer, "d1", "0.0", "10.0"),
            "topology.links.id",
        ),
        (
            link("l1", "self", &peer, "", "0.0", "10.0"),
            "topology.links.from",
        ),
        (
            link("l1", "", &peer, "d1", "0.0", "10.0"),
            "topology.links.from",
        ),
        (
            link("l1", "self", &peer, "d1", "10.0", "10.0"),
            "topology.links.from",
        ),
        (
            link("l1", "self", &peer, "d1", "10.0", "1.0"),
            "topology.links.from",
        ),
        (
            link("l1", "self", &peer, "d1", "nan", "10.0"),
            "topology.links.from",
        ),
        (
            link("l1", "self", "self", "d1", "0.0", "10.0"),
            "topology.links",
        ),
    ] {
        let document = format!("{VALID}[topology]\n{extra}");
        let error = parse(&document)
            .validate()
            .expect_err("the link must be rejected");
        assert_eq!(error.code(), Some(ErrorCode::Topology), "for `{key}`");
        assert_eq!(key_of(&error), key);
    }

    // Duplicate ids are refused even when both links are otherwise fine.
    let duplicate = format!(
        "[topology]\n{}{}",
        link("l1", "self", &peer, "d1", "0.0", "10.0"),
        link("l1", "self", &peer, "d1", "20.0", "30.0")
    );
    let error = reject(&duplicate);
    assert_eq!(key_of(&error), "topology.links.id");
}

#[test]
fn validate_cannot_tell_self_from_its_own_peer_reference() {
    // Documented limitation: `Config::validate` has no local node id, so
    // `from.node = "self"` versus `to.node = "peer:<the local id>"` looks like a
    // link between two different nodes. Identity parsing happens later, in
    // `PeerConfig::node_id_bytes`, `parse_node_ref` and `to_transition_links`.
    let peer = config::peer_reference(&[0x33; 32]);
    let document = format!(
        "{VALID}[topology]\n\
         [[topology.links]]\nid = \"l1\"\n\
         from = {{ node = \"self\", display = \"d1\", edge = \"right\", start = 0.0, end = 10.0 }}\n\
         to = {{ node = \"{peer}\", display = \"d2\", edge = \"left\", start = 0.0, end = 10.0 }}\n"
    );
    parse(&document)
        .validate()
        .expect("the textual comparison cannot see that these are the same node");
}

#[test]
fn the_pointer_rule_uses_the_documented_challenge_and_heartbeat_only() {
    // Both derived values stay consistent with the documented formulas across
    // the whole legal range of their sources.
    for lease in [300u32, 499, 500, 501, 999, 1000] {
        let config = parse(&format!("{VALID}[timing]\nlease_ttl_ms = {lease}\n"));
        config.validate().expect("in range");
        assert_eq!(config.timing.challenge_interval_ms(), (lease / 5).min(100));
    }
    for silence in [200u32, 300, 499, 500] {
        let config = parse(&format!(
            "{VALID}[timing]\nagent_core_silence_ms = {silence}\n"
        ));
        config.validate().expect("in range");
        assert_eq!(config.timing.ipc_heartbeat_ms(), silence / 4);
    }
}

#[test]
fn clipboard_routing_defaults_clamps_and_roundtrips() {
    use config::ClipboardRoutingPolicy;

    let defaults = Config::default();
    assert_eq!(defaults.clipboard.policy, ClipboardRoutingPolicy::Follow);
    assert_eq!(defaults.clipboard.settle_ms, 700);
    let omitted = parse(VALID);
    assert_eq!(omitted.clipboard, defaults.clipboard);

    for (input, expected) in [
        (0, 200),
        (199, 200),
        (200, 200),
        (700, 700),
        (3000, 3000),
        (3001, 3000),
        (u32::MAX, 3000),
    ] {
        let document = format!("{VALID}[clipboard]\npolicy = \"broadcast\"\nsettle_ms = {input}\n");
        let config = parse(&document);
        assert_eq!(config.clipboard.policy, ClipboardRoutingPolicy::Broadcast);
        assert_eq!(config.clipboard.settle_ms, expected);
        let encoded = toml::to_string(&config).unwrap();
        assert_eq!(parse(&encoded).clipboard, config.clipboard);
    }
    assert!(Config::from_str(&format!("{VALID}[clipboard]\npolicy = \"unknown\"\n")).is_err());
}
