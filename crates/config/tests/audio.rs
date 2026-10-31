use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use config::{
    save_audio_locked, AudioConfig, AudioDeviceSelector, AudioMode, AudioPeerConfig, AudioProfile,
    AudioRole, Config, DefaultDeviceRole, HexId, SystemCaptureSelector,
};

fn parse(audio: &str) -> Result<Config, config::ConfigError> {
    Config::from_str(&format!(
        "config_version = 2\n[network]\nlisten_address = '192.168.1.10'\n[audio]\n{audio}"
    ))
}

fn valid(audio: &str) -> bool {
    parse(audio).and_then(|config| config.validate()).is_ok()
}

fn directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "bifrost-audio-config-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn legacy_default_stays_audio_free_and_roundtrips() {
    let config = Config::from_str("config_version = 2").unwrap();
    assert_eq!(config.audio, AudioConfig::default());
    assert!(!config.audio.enabled);
    assert_eq!(config.audio.role, AudioRole::Off);
    assert_eq!(config.audio.profile, AudioProfile::Auto);
    assert_eq!(config.audio.mode, AudioMode::MixAll);
    assert!(!config.to_toml().unwrap().contains("[audio"));
    assert_eq!(
        Config::from_str(&config.to_toml().unwrap()).unwrap(),
        config
    );
    let example = Config::from_str(include_str!("../examples-data/config.example.toml")).unwrap();
    example.audio.validate().unwrap();
    assert_eq!(example.audio.role, AudioRole::Source);
    assert!(example.audio.group_id.is_none());
    assert!(example.audio.allow_send_to.is_empty());
    assert!(example.audio.allow_system_audio_from.is_empty());
}

#[test]
fn unresolved_source_preserves_enabled_intent_without_inventing_group() {
    let text = format!("role = 'source'\nlistener_node_id = '{}'", "aB".repeat(32));
    let config = parse(&text).unwrap();
    config.validate().unwrap();
    let saved = config.to_toml().unwrap();
    let restored = Config::from_str(&saved).unwrap();
    assert_eq!(restored, config);
    assert!(saved.contains(&"ab".repeat(32)));
    assert!(restored.audio.group_id.is_none());
    assert!(valid(&format!("{text}\nenabled = true")));
    assert!(valid("role = 'listener'\nenabled = true"));
    let pending = parse(&format!("{text}\nenabled = true")).unwrap();
    let restored = Config::from_str(&pending.to_toml().unwrap()).unwrap();
    assert!(restored.audio.enabled);
    assert!(restored.audio.group_id.is_none());
    assert!(!valid("role = 'source'\nenabled = true"));
}

#[test]
fn group_override_preserves_all_presence_states_through_toml() {
    assert_eq!(AudioConfig::default().group_id_override, Some(false));
    let source = format!(
        "role = 'source'\nlistener_node_id = '{}'\ngroup_id = '{}'",
        "11".repeat(32),
        "22".repeat(16)
    );
    for policy in [None, Some(false), Some(true)] {
        let field = policy
            .map(|value| format!("\ngroup_id_override = {value}"))
            .unwrap_or_default();
        let config = parse(&format!("{source}{field}")).unwrap();
        config.validate().unwrap();
        assert_eq!(config.audio.group_id_override, policy);
        assert_eq!(
            config.audio.effective_group_override(),
            policy != Some(false)
        );
        let saved = config.to_toml().unwrap();
        assert_eq!(saved.contains("group_id_override"), policy.is_some());
        let restored = Config::from_str(&saved).unwrap();
        assert_eq!(restored, config);
        assert_eq!(restored.audio.group_id_override, policy);
    }
}

#[test]
fn group_override_requires_a_source_group() {
    for role in [AudioRole::Off, AudioRole::Listener, AudioRole::Source] {
        let mut config = AudioConfig {
            role,
            group_id_override: Some(true),
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.group_id = Some(HexId([2; 16]));
        assert_eq!(config.validate().is_ok(), role == AudioRole::Source);
    }
}

#[test]
fn legacy_group_pin_survives_restart_and_unrelated_locked_save() {
    let directory = directory();
    let path = directory.join("config.toml");
    let legacy = parse(&format!(
        "role = 'source'\nlistener_node_id = '{}'\ngroup_id = '{}'",
        "11".repeat(32),
        "22".repeat(16)
    ))
    .unwrap();
    assert_eq!(legacy.audio.group_id_override, None);
    assert!(legacy.audio.effective_group_override());
    legacy.save(&path).unwrap();
    let mut restored = Config::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(restored.audio.group_id_override, None);
    assert!(restored.audio.effective_group_override());
    restored.audio.background_gain_db = -24.0;
    save_audio_locked(&path, restored.audio.clone()).unwrap();
    let restarted = Config::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(restarted.audio, restored.audio);
    assert_eq!(restarted.audio.group_id_override, None);
    assert!(restarted.audio.effective_group_override());
    restored.audio.group_id_override = Some(false);
    save_audio_locked(&path, restored.audio).unwrap();
    let unpinned = Config::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(unpinned.audio.group_id, legacy.audio.group_id);
    assert_eq!(unpinned.audio.group_id_override, Some(false));
    assert!(!unpinned.audio.effective_group_override());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn legacy_group_policy_only_pins_sources_with_a_group() {
    for role in [AudioRole::Off, AudioRole::Listener, AudioRole::Source] {
        for group_id in [None, Some(HexId([2; 16]))] {
            let config = AudioConfig {
                role,
                group_id,
                group_id_override: None,
                ..Default::default()
            };
            assert_eq!(
                config.effective_group_override(),
                role == AudioRole::Source && group_id.is_some()
            );
        }
    }
}

#[test]
fn rejects_malformed_ids_enums_ports_and_nested_unknowns() {
    for text in [
        "group_id = ''",
        "group_id = 'abcd'",
        "listener_node_id = 'peer:abc'",
        "role = 'receiver'",
        "profile = 'fast'",
        "mode = 'follow_cursor'",
        "media_port = 0",
        "media_port = 47902",
        "media_port = 65536",
        "enabled = true",
        "mystery = true",
        "[audio.microphone]\nmystery = false",
        "[audio.output_device]\nkind = 'follow_default'\nrole = 'gaming'",
        "[audio.capture_device]\nkind = 'default_monitor'\nextra = true",
        "[audio.output_device]\nkind = 'stable_id'\nid = ''",
        "[audio.microphone]\nstreaming_enabled = true",
        "[audio.microphone]\nptt_enabled = true",
        "[audio.microphone]\nptt_hotkey = 'not a hotkey'",
    ] {
        assert!(!valid(text), "accepted {text}");
    }
    assert!(valid("media_port = 47901"));
    assert!(valid("media_port = 65535"));
}

#[test]
fn consent_is_explicit_bounded_unique_and_directional() {
    let id = "11".repeat(32);
    let other = "22".repeat(32);
    assert!(valid(&format!(
        "role = 'source'\nlistener_node_id = '{id}'\nallow_send_to = ['{id}']"
    )));
    assert!(!valid(&format!(
        "role = 'source'\nlistener_node_id = '{id}'\nallow_send_to = ['{other}']"
    )));
    assert!(!valid(&format!(
        "role = 'source'\nallow_system_audio_from = ['{id}']"
    )));
    assert!(!valid(&format!(
        "role = 'listener'\nallow_send_to = ['{id}']"
    )));
    assert!(!valid(&format!("allow_system_audio_from = ['{id}']")));
    assert!(!valid(&format!(
        "role = 'listener'\nallow_system_audio_from = ['{id}', '{id}']"
    )));
    let four = (1..=4)
        .map(|byte| format!("'{}'", format!("{byte:02x}").repeat(32)))
        .collect::<Vec<_>>()
        .join(",");
    assert!(!valid(&format!(
        "role = 'listener'\nallow_system_audio_from = [{four}]"
    )));
    let paired = Config::from_str(&format!(
        "config_version = 2\n[[network.peers]]\nnode_id = '{id}'\naddress = '192.168.1.11'"
    ))
    .unwrap();
    assert_eq!(paired.audio, AudioConfig::default());
}

#[test]
fn peer_gain_and_media_permission_are_independent() {
    let id = "11".repeat(32);
    for gain in ["-1", "101", "nan", "inf", "-inf"] {
        assert!(!valid(&format!(
            "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'\ngain_percent = {gain}"
        )));
    }
    assert!(valid(&format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'\ngain_percent = 25\nmuted = true"
    )));
    assert!(!valid(&format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'\nallow_media_control = true"
    )));
    assert!(valid(&format!("role = 'source'\nlistener_node_id = '{id}'\n[[audio.peers]]\nnode_id = '{id}'\nallow_media_control = true")));
    assert!(!valid(&format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'\n[[audio.peers]]\nnode_id = '{id}'"
    )));
}

#[test]
fn modern_source_retains_inactive_media_grants_without_changing_legacy_validation() {
    let selected = "11".repeat(32);
    let previous = "22".repeat(32);
    let source = format!(
        "role = 'source'\nlistener_node_id = '{selected}'\n\
         [[audio.peers]]\nnode_id = '{previous}'\nallow_media_control = true"
    );
    assert!(
        !valid(&source),
        "legacy policy remains selected-peer strict"
    );
    let listener = format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{previous}'\nallow_media_control = true"
    );
    assert!(!valid(&listener), "legacy Listener policy stays strict");
    for sharing in [false, true] {
        assert!(valid(&format!("system_sharing = {sharing}\n{source}")));
        assert!(valid(&format!("system_sharing = {sharing}\n{listener}")));
    }
}

#[test]
fn typed_device_selectors_roundtrip() {
    let config = parse("role = 'listener'\n[audio.output_device]\nkind = 'stable_id'\nid = 'endpoint:headset'\n[audio.capture_device]\nkind = 'default_monitor'\n[audio.microphone.capture_device]\nkind = 'follow_default'\nrole = 'communications'").unwrap();
    config.validate().unwrap();
    assert_eq!(
        config.audio.output_device,
        AudioDeviceSelector::StableId {
            id: "endpoint:headset".into()
        }
    );
    assert_eq!(
        config.audio.capture_device,
        SystemCaptureSelector::DefaultMonitor {}
    );
    assert_eq!(
        config.audio.microphone.capture_device,
        AudioDeviceSelector::FollowDefault {
            role: DefaultDeviceRole::Communications
        }
    );
    assert_eq!(
        Config::from_str(&config.to_toml().unwrap()).unwrap(),
        config
    );
}

#[test]
fn tuning_clamps_on_load_but_programmatic_invalid_values_are_rejected() {
    let config =
        parse("focus_debounce_ms = 1\nfocus_ramp_ms = 9999\nbackground_gain_db = -99").unwrap();
    assert_eq!(config.audio.focus_debounce_ms, 100);
    assert_eq!(config.audio.focus_ramp_ms, 1000);
    assert_eq!(config.audio.background_gain_db, -60.0);
    let config =
        parse("focus_debounce_ms = 9000\nfocus_ramp_ms = 0\nbackground_gain_db = 99").unwrap();
    assert_eq!(config.audio.focus_debounce_ms, 2000);
    assert_eq!(config.audio.focus_ramp_ms, 20);
    assert_eq!(config.audio.background_gain_db, 0.0);
    assert!(parse("background_gain_db = nan").is_err());
    let mut audio = AudioConfig {
        background_gain_db: f32::NAN,
        ..AudioConfig::default()
    };
    assert!(audio.validate().is_err());
    audio.background_gain_db = -18.0;
    audio.focus_ramp_ms = 0;
    assert!(audio.validate().is_err());
}

#[test]
fn activation_identity_checks_require_current_trust_and_reject_self() {
    let local = HexId([1; 32]);
    let remote = HexId([2; 32]);
    let mut audio = AudioConfig {
        role: AudioRole::Source,
        listener_node_id: Some(remote),
        ..AudioConfig::default()
    };
    assert!(audio.validate_identity(local, &BTreeSet::new()).is_err());
    assert!(audio
        .validate_identity(local, &BTreeSet::from([remote]))
        .is_ok());
    audio.listener_node_id = Some(local);
    assert!(audio
        .validate_identity(local, &BTreeSet::from([local]))
        .is_err());
    audio.role = AudioRole::Listener;
    assert!(audio.validate_identity(local, &BTreeSet::new()).is_ok());
    audio.allow_system_audio_from.push(local);
    assert!(audio
        .validate_identity(local, &BTreeSet::from([local]))
        .is_err());
}

#[test]
fn first_audio_save_preserves_original_once_and_serializes_concurrent_changes() {
    let directory = directory();
    let path = directory.join("config.toml");
    let original =
        "# pre-audio document\nconfig_version = 2\n[network]\nlisten_address = '192.168.1.10'\n";
    fs::write(&path, original).unwrap();
    let audio = AudioConfig {
        role: AudioRole::Source,
        listener_node_id: Some(HexId([2; 32])),
        ..AudioConfig::default()
    };
    save_audio_locked(&path, audio).unwrap();
    let backup = directory.join("config.toml.pre-audio");
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    std::thread::scope(|scope| {
        let path = &path;
        scope.spawn(move || {
            config::update_locked(path, |config| config.node.display_name = "LAPTOP".into())
                .unwrap()
        });
        scope.spawn(move || {
            config::update_locked(path, |config| config.audio.profile = AudioProfile::Wifi).unwrap()
        });
    });
    let saved = Config::load(&path).unwrap();
    assert_eq!(saved.node.display_name, "LAPTOP");
    assert_eq!(saved.audio.profile, AudioProfile::Wifi);
    assert_eq!(fs::read_to_string(&backup).unwrap(), original);
    assert_ne!(
        fs::read_to_string(directory.join("config.toml.bak")).unwrap(),
        original
    );
    let mut invalid = saved.audio;
    invalid.enabled = true;
    invalid.listener_node_id = None;
    let before = fs::read_to_string(&path).unwrap();
    assert!(save_audio_locked(&path, invalid).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn no_audio_write_does_not_create_upgrade_backup_and_existing_backup_is_untouched() {
    let directory = directory();
    let path = directory.join("config.toml");
    let backup = directory.join("config.toml.pre-audio");
    let config =
        Config::from_str("config_version = 2\n[network]\nlisten_address = '192.168.1.10'").unwrap();
    config.save(&path).unwrap();
    assert!(!backup.exists());
    fs::write(&backup, "retained backup").unwrap();
    save_audio_locked(
        &path,
        AudioConfig {
            role: AudioRole::Listener,
            ..AudioConfig::default()
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&backup).unwrap(), "retained backup");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn generic_atomic_saves_do_not_inspect_audio_or_create_upgrade_backups() {
    let directory = directory();
    for name in ["portal-tokens.toml", "layout.toml", "diagnostics.toml"] {
        let path = directory.join(name);
        // Opaque previous contents must not be interpreted as a config document.
        let original = [0xff, 0xfe];
        fs::write(&path, original).unwrap();
        let contents = "[audio]\nenabled = true\n";
        config::save_atomic(&path, contents).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), contents);
        assert_eq!(
            fs::read(directory.join(format!("{name}.bak"))).unwrap(),
            original
        );
        assert!(!directory.join(format!("{name}.pre-audio")).exists());
        assert!(!directory.join(format!("{name}.pre-audio.tmp")).exists());
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn manual_audio_document_does_not_invent_a_pre_audio_backup() {
    let directory = directory();
    let path = directory.join("config.toml");
    let backup = directory.join("config.toml.pre-audio");
    for audio in ["role = 'off'", "role = 'listener'"] {
        let original =
            format!("# manually configured audio\nconfig_version = 2\n[network]\nlisten_address = '192.168.1.10'\n[audio]\n{audio}\n");
        fs::write(&path, &original).unwrap();
        save_audio_locked(
            &path,
            AudioConfig {
                role: AudioRole::Listener,
                ..AudioConfig::default()
            },
        )
        .unwrap();
        assert!(!backup.exists());
        assert!(!directory.join("config.toml.pre-audio.tmp").exists());
        assert_eq!(
            fs::read_to_string(directory.join("config.toml.bak")).unwrap(),
            original
        );
        config::update_locked(&path, |config| config.audio.profile = AudioProfile::Wifi).unwrap();
        assert!(!backup.exists());
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn programmatic_peer_gains_reject_out_of_range_values() {
    let audio = AudioConfig {
        role: AudioRole::Listener,
        peers: vec![AudioPeerConfig {
            node_id: HexId([1; 32]),
            system_send_override: None,
            system_receive_override: None,
            allow_mic_control: false,
            allow_media_control: false,
            gain_percent: 101,
            muted: false,
        }],
        ..AudioConfig::default()
    };
    assert!(audio.validate().is_err());
}

#[test]
fn cosmetic_clamps_emit_warnings() {
    struct Warnings(std::sync::Arc<AtomicU64>);
    impl tracing::Subscriber for Warnings {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
    let count = std::sync::Arc::new(AtomicU64::new(0));
    tracing::subscriber::with_default(Warnings(count.clone()), || {
        parse("focus_debounce_ms = 0\nfocus_ramp_ms = 0\nbackground_gain_db = 1").unwrap();
    });
    assert_eq!(count.load(Ordering::Relaxed), 3);
}

#[test]
fn microphone_consent_defaults_closed_and_gui_ptt_needs_no_global_hotkey() {
    let id = "11".repeat(32);
    let listener = parse(&format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'"
    ))
    .unwrap();
    assert!(!listener.audio.peers[0].allow_mic_control);
    assert!(valid(&format!(
        "role = 'listener'\n[[audio.peers]]\nnode_id = '{id}'\nallow_mic_control = true"
    )));
    assert!(!valid(&format!("role = 'source'\nlistener_node_id = '{id}'\n[[audio.peers]]\nnode_id = '{id}'\nallow_mic_control = true")));
    assert!(valid("role = 'listener'\n[audio.microphone]\ncontrol_enabled = true\nptt_enabled = true\nptt_hotkey = ''"));
    assert!(!valid(
        "role = 'listener'\n[audio.microphone]\nptt_enabled = true"
    ));
}

#[test]
fn physical_microphone_target_is_listener_local_only() {
    let local = HexId([1; 32]);
    let remote = HexId([2; 32]);
    let mut audio = AudioConfig {
        role: AudioRole::Listener,
        ..AudioConfig::default()
    };
    audio.microphone.pinned_target_node_id = Some(local);
    assert!(audio.validate_identity(local, &BTreeSet::new()).is_ok());
    audio.microphone.pinned_target_node_id = Some(remote);
    assert!(audio
        .validate_identity(local, &BTreeSet::from([remote]))
        .is_err());
}

#[test]
fn system_policy_preserves_legacy_lists_and_applies_explicit_consent() {
    let selected = HexId([1; 32]);
    let other = HexId([2; 32]);
    for role in [AudioRole::Source, AudioRole::Listener] {
        let mut audio = AudioConfig {
            enabled: true,
            role,
            listener_node_id: (role == AudioRole::Source).then_some(selected),
            ..Default::default()
        };
        assert!(audio.system_enabled());
        assert!(!audio.system_send_allowed(selected));
        assert!(!audio.system_receive_allowed(selected));
        if role == AudioRole::Source {
            audio.allow_send_to.push(selected);
        } else {
            audio.allow_system_audio_from.push(selected);
        }
        assert_eq!(
            audio.system_send_allowed(selected),
            role == AudioRole::Source
        );
        assert_eq!(
            audio.system_receive_allowed(selected),
            role == AudioRole::Listener
        );
        assert!(!audio.system_send_allowed(other));
        assert!(!audio.system_receive_allowed(other));

        for sharing in [None, Some(false), Some(true)] {
            audio.system_sharing = sharing;
            for permission in [None, Some(false), Some(true)] {
                audio.peers = vec![AudioPeerConfig {
                    node_id: selected,
                    system_send_override: (role == AudioRole::Source)
                        .then_some(permission)
                        .flatten(),
                    system_receive_override: (role == AudioRole::Listener)
                        .then_some(permission)
                        .flatten(),
                    allow_mic_control: false,
                    allow_media_control: false,
                    gain_percent: 100,
                    muted: false,
                }];
                for listed in [false, true] {
                    audio.allow_send_to.clear();
                    audio.allow_system_audio_from.clear();
                    if listed {
                        if role == AudioRole::Source {
                            audio.allow_send_to.push(selected);
                        } else {
                            audio.allow_system_audio_from.push(selected);
                        }
                    }
                    audio.validate().unwrap();
                    let allowed = sharing != Some(false)
                        && permission.unwrap_or(sharing == Some(true) || listed);
                    assert_eq!(
                        audio.system_send_allowed(selected),
                        role == AudioRole::Source && allowed
                    );
                    assert_eq!(
                        audio.system_receive_allowed(selected),
                        role == AudioRole::Listener && allowed
                    );
                    assert!(!audio.system_send_allowed(other));
                    assert_eq!(
                        audio.system_receive_allowed(other),
                        role == AudioRole::Listener && sharing == Some(true)
                    );
                    audio.enabled = false;
                    assert!(!audio.system_enabled());
                    assert!(!audio.system_send_allowed(selected));
                    assert!(!audio.system_receive_allowed(selected));
                    audio.enabled = true;
                }
            }
        }
    }
}

#[test]
fn system_policy_never_bypasses_role_or_source_selection() {
    let selected = HexId([1; 32]);
    let other = HexId([2; 32]);
    let mut audio = AudioConfig {
        enabled: true,
        role: AudioRole::Source,
        listener_node_id: Some(selected),
        system_sharing: Some(true),
        peers: vec![AudioPeerConfig {
            node_id: other,
            system_send_override: Some(true),
            system_receive_override: None,
            allow_mic_control: false,
            allow_media_control: false,
            gain_percent: 100,
            muted: false,
        }],
        ..Default::default()
    };
    assert!(!audio.system_send_allowed(other));
    assert!(audio.validate().is_err());
    audio.listener_node_id = None;
    assert!(!audio.system_send_allowed(selected));
    assert!(!audio.system_send_allowed(other));
    audio.role = AudioRole::Listener;
    assert!(audio.validate().is_err());
    audio.peers[0].system_send_override = None;
    audio.peers[0].system_receive_override = Some(true);
    audio.validate().unwrap();
    audio.role = AudioRole::Source;
    assert!(audio.validate().is_err());
    audio.role = AudioRole::Off;
    assert!(!audio.system_enabled());
    assert!(!audio.system_send_allowed(other));
    assert!(!audio.system_receive_allowed(other));
    assert!(audio.validate().is_err());
    audio.enabled = false;
    audio.peers.clear();
    assert!(audio.validate().is_err());
    audio.system_sharing = Some(false);
    audio.validate().unwrap();
}

#[test]
fn system_policy_toml_roundtrips_presence_and_unrelated_preferences() {
    let id = "11".repeat(32);
    for role in [AudioRole::Source, AudioRole::Listener] {
        let role_text = if role == AudioRole::Source {
            "source"
        } else {
            "listener"
        };
        let mut config = parse(&format!(
            "role = '{role_text}'\nenabled = true\nlistener_node_id = '{id}'\n\
             profile = 'wifi'\nmode = 'follow_focus'\nresume_system_audio = true\n\
             media_port = 48001\noutput_fallback_to_default = true\n\
             focus_debounce_ms = 300\nfocus_ramp_ms = 200\nbackground_gain_db = -24.0\n\
             background_mute = true\n\
             [audio.output_device]\nkind = 'stable_id'\nid = 'speaker'\n\
             [audio.capture_device]\nkind = 'stable_id'\nid = 'monitor'\n\
             [audio.microphone]\ncontrol_enabled = true\nptt_enabled = true\n\
             ptt_hotkey = ''\nmuted = false\nvirtual_device = 'existing-mic'\n\
             [audio.microphone.capture_device]\nkind = 'stable_id'\nid = 'physical-mic'\n\
             [[audio.peers]]\nnode_id = '{id}'\nallow_mic_control = {}\n\
             allow_media_control = {}\ngain_percent = {}\nmuted = {}",
            role == AudioRole::Listener,
            role == AudioRole::Source,
            if role == AudioRole::Listener { 42 } else { 100 },
            role == AudioRole::Listener,
        ))
        .unwrap();
        let original = config.audio.clone();
        assert_eq!(original.system_sharing, None);
        assert_eq!(original.peers[0].system_send_override, None);
        assert_eq!(original.peers[0].system_receive_override, None);
        for sharing in [None, Some(false), Some(true)] {
            for permission in [None, Some(false), Some(true)] {
                config.audio.system_sharing = sharing;
                if role == AudioRole::Source {
                    config.audio.peers[0].system_send_override = permission;
                } else {
                    config.audio.peers[0].system_receive_override = permission;
                }
                config.audio.validate().unwrap();
                let saved = config.to_toml().unwrap();
                assert_eq!(saved.contains("system_sharing ="), sharing.is_some());
                let permission_key = if role == AudioRole::Source {
                    "system_send_override ="
                } else {
                    "system_receive_override ="
                };
                assert_eq!(saved.contains(permission_key), permission.is_some());
                let mut restored = Config::from_str(&saved).unwrap();
                assert_eq!(restored, config);
                restored.audio.system_sharing = None;
                restored.audio.peers[0].system_send_override = None;
                restored.audio.peers[0].system_receive_override = None;
                assert_eq!(restored.audio, original);
            }
        }
    }
}

#[test]
fn system_overrides_use_existing_peer_identity_and_count_bounds() {
    let local = HexId([1; 32]);
    let remote = HexId([2; 32]);
    let mut audio = parse(&format!(
        "role = 'listener'\nenabled = true\nsystem_sharing = true\n\
         [[audio.peers]]\nnode_id = '{remote}'\nsystem_receive_override = false"
    ))
    .unwrap()
    .audio;
    assert!(audio.validate_identity(local, &BTreeSet::new()).is_err());
    audio
        .validate_identity(local, &BTreeSet::from([remote]))
        .unwrap();
    audio.peers.push(audio.peers[0].clone());
    assert!(audio.validate().is_err());
    audio.peers.pop();
    for byte in 3..=5 {
        let mut peer = audio.peers[0].clone();
        peer.node_id = HexId([byte; 32]);
        audio.peers.push(peer);
    }
    assert!(audio.validate().is_err());
    assert!(parse(
        "role = 'listener'\n[[audio.peers]]\nnode_id = 'bad'\nsystem_receive_override = true"
    )
    .is_err());
}
